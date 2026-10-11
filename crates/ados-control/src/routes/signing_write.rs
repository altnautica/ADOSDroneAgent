//! MAVLink v2 signing write routes: FC enrollment and store clear.
//!
//! The agent never holds a signing key. These write routes let the GCS push a
//! one-shot key to the FC and clear the FC's signing store, each by building a
//! MAVLink frame and writing it to
//! `/run/ados/mavlink.sock`, the same socket the router forwards to the FC. They
//! are the write companions to the [`crate::routes::signing`] reads.
//!
//! ## What the frames are (matched to the Python signing emitter)
//!
//! The Python routes pack their frames with a standalone v2 encoder whose source
//! identity is `srcSystem=255, srcComponent=190` (mission-planner component), and
//! the router forwards the frame verbatim, so the header identity is on the wire.
//! This surface stamps the same `255/190` source so an enroll/disable from here
//! is wire-identical to one the Python routes sent.
//!
//! - **`POST /api/mavlink/signing/enroll-fc`** parses the 64-hex-char body into a
//!   32-byte key and sends `SETUP_SIGNING` (the key + an initial timestamp in
//!   10-microsecond units since 2015-01-01 UTC) **twice, 200 ms apart**, so a
//!   single-frame radio hiccup during enrollment does not lose the key. The key
//!   buffer is overwritten with zeros before the route returns. The response is
//!   `{sent, verified, key_id, enrolled_at}`: `key_id` is the first 8 hex chars of
//!   sha256(key) (a fingerprint, never the key), `enrolled_at` an ISO-8601 UTC
//!   timestamp at seconds precision.
//! - **`POST /api/mavlink/signing/disable-on-fc`** sends `SETUP_SIGNING` with an
//!   all-zero key and a zero timestamp, which ArduPilot recognises as
//!   "disable signing", and returns `{sent, verified}`.
//!
//! ## Sent is not applied
//!
//! Writing the frame proves only that the router took it. An FC that already
//! enforces signing drops an unsigned `SETUP_SIGNING` on every link but its
//! USB channel, and says nothing. So each route watches the FC's own frames
//! for [`VERIFY_WINDOW`] after the send, on the same socket connection:
//! enrollment is `verified` once a frame from the target arrives signed with
//! the new key, and a disable once a frame from the target arrives unsigned.
//! `verified: false` means no such frame was seen, not that the write failed.
//!
//! ## Error shapes (matched verbatim to the Python routes)
//!
//! Both gate on the FC being connected first (`503 {"detail":"FC not
//! connected"}` when not). A failure to reach the MAVLink socket is `503
//! {"detail":"MAVLink command link unavailable"}` (the Python connect-failure
//! branch). A bad body on enroll is `400 {"detail": <parse error>}` (the exact
//! `parse_key_hex` message). Any other send failure degrades to the route's
//! `500 {"detail": ...}` ("enrollment failed" / "disable failed"), never a panic.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use ados_protocol::mavlink::ardupilotmega::{MavMessage, SETUP_SIGNING_DATA};
use ados_protocol::mavlink::{self, MavHeader};

use crate::ipc::{AckStream, FrameRead};
use crate::routes::detail;
use crate::state::AppState;

mod frames;

use frames::*;

/// How long a signing write watches the FC's frames for proof it applied.
const VERIFY_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// The source identity stamped on every signing frame, matching the Python
/// signing encoder (`srcSystem=255, srcComponent=MAV_COMP_ID_MISSIONPLANNER`), so
/// an enroll/disable from this surface is wire-identical to one the Python
/// routes emitted. The router forwards the frame verbatim, so this header is on
/// the wire and is the parity target.
const SOURCE_SYSTEM_ID: u8 = 255;
const SOURCE_COMPONENT_ID: u8 = 190;

/// The 32-byte signing-key length the FC's signing store expects.
const SIGNING_KEY_LEN: usize = 32;

/// The agent-held key's file name, in the agent config directory.
const AGENT_KEY_FILE: &str = "mavlink-signing.key";

/// Seconds from the POSIX epoch (1970-01-01 UTC) to 2015-01-01 UTC, the MAVLink
/// signing epoch the initial timestamp is measured from.
const EPOCH_2015_UNIX_SECONDS: i64 = 1_420_070_400;

/// 10-microsecond units per second: the `SETUP_SIGNING` `initial_timestamp` is in
/// 10-µs units, so seconds-since-2015 is scaled by this (`* 100_000` in Python).
const TEN_US_PER_SECOND: f64 = 100_000.0;

/// Body for `POST /api/mavlink/signing/enroll-fc`. `key_hex` is the 32-byte key as
/// exactly 64 lowercase hex chars (NEVER logged); the three target/link fields fall
/// back to their documented defaults when omitted. The numeric fields are bounded in
/// the handler, so an out-of-range value is a 400, not a clamp.
#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    /// The 32-byte MAVLink signing key as 64 lowercase hex chars. Sensitive: never
    /// logged, and the parsed bytes are zeroized before the route returns.
    pub key_hex: String,
    /// The signing link id. Carried in the response log only; not on the wire.
    #[serde(default)]
    pub link_id: i64,
    /// The target system id. Defaults to the resolved FC target.
    #[serde(default)]
    pub target_system: Option<i64>,
    /// The target component id. Defaults to the resolved FC target.
    #[serde(default)]
    pub target_component: Option<i64>,
    /// Let the agent keep the key and sign its own frames. Honoured only with
    /// `mavlink.agent_signing` on; the key is then stored at
    /// `<config dir>/mavlink-signing.key` (0600) for the router to load.
    #[serde(default)]
    pub agent_holds_key: bool,
}

/// `POST /api/mavlink/signing/enroll-fc` → push a 32-byte signing key to the FC.
///
/// Gates on the FC being connected (`503` when not), parses+validates the hex key
/// (`400` with the exact parse error on a bad body), then builds `SETUP_SIGNING`
/// and writes it to the MAVLink socket twice. On success returns `{sent: true,
/// verified, key_id, enrolled_at}` — `verified` is whether a frame from the
/// target signed with this key was seen within [`VERIFY_WINDOW`], `key_id` the
/// first 8 hex of sha256(key), never the key. A failure to reach the socket is
/// the Python `503 "MAVLink command link unavailable"`; any other failure is the
/// Python `500 "enrollment failed"`. The parsed key bytes are zeroized before the
/// route returns on every path.
pub async fn enroll_fc(State(state): State<AppState>, Json(req): Json<EnrollRequest>) -> Response {
    if !state.fc_connected() {
        return detail(StatusCode::SERVICE_UNAVAILABLE, "FC not connected");
    }

    // Parse + validate the field bounds (the Pydantic ge/le validators), then the
    // hex. A bad body is a 400 before any send, matching the Python validators.
    let fc = crate::fc_target::fc_target(&state);
    let target_system = match bounded_u8(req.target_system.unwrap_or(i64::from(fc.0)), 1, 255) {
        Ok(v) => v,
        Err(()) => return detail(StatusCode::BAD_REQUEST, "target_system out of range"),
    };
    let target_component = match bounded_u8(req.target_component.unwrap_or(i64::from(fc.1)), 0, 255)
    {
        Ok(v) => v,
        Err(()) => return detail(StatusCode::BAD_REQUEST, "target_component out of range"),
    };
    if !(0..=255).contains(&req.link_id) {
        return detail(StatusCode::BAD_REQUEST, "link_id out of range");
    }

    // parse_key_hex parity: a non-64-char or non-hex body is a 400 carrying the
    // exact Python error message. The returned bytes are sensitive.
    let mut key = match parse_key_hex(&req.key_hex) {
        Ok(bytes) => bytes,
        Err(msg) => return detail(StatusCode::BAD_REQUEST, msg),
    };

    // Build the SETUP_SIGNING frame once (the same bytes are sent twice). The
    // key stays in memory until the observation window closes, because checking
    // the FC's signatures needs it; it is zeroized on every path after that.
    let initial_ts = initial_timestamp_10us(OffsetDateTime::now_utc());
    let key_id = fingerprint(&key);
    let frame = match build_setup_signing_frame(target_system, target_component, &key, initial_ts) {
        Ok(bytes) => bytes,
        Err(()) => {
            zeroize(&mut key);
            tracing::error!("signing enroll frame serialize failed");
            return detail(StatusCode::INTERNAL_SERVER_ERROR, "enrollment failed");
        }
    };

    // One connection carries the sends and the observation, so it sees only FC
    // frames broadcast after it connected.
    let mut stream = match state.mavlink.open_ack_stream().await {
        Ok(s) => s,
        Err(e) => {
            zeroize(&mut key);
            tracing::warn!(error = %e, "signing enroll connect failed");
            return detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "MAVLink command link unavailable",
            );
        }
    };

    // Send twice, 200 ms apart, matching the Python double-send for radio
    // resilience. A failure on the first frame is the link-unavailable 503.
    if let Err(e) = stream.write_frame(&frame).await {
        zeroize(&mut key);
        tracing::warn!(error = %e, "signing enroll send (1/2) failed");
        return detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "MAVLink command link unavailable",
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    if let Err(e) = stream.write_frame(&frame).await {
        zeroize(&mut key);
        // The FIRST frame already reached the FC, so the key may well be
        // enrolled and the FC may already be rejecting unsigned frames. A bare
        // 500 "enrollment failed" tells the operator the opposite and invites
        // them to walk away from an aircraft they can no longer command.
        // Report the ambiguity instead, and hand back the fingerprint so the
        // GCS can name the key the operator must keep.
        tracing::error!(error = %e, key_id = %key_id, "signing enroll send (2/2) failed after first frame was sent");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "detail": "enrollment may have partially applied: the first SETUP_SIGNING frame \
                           was sent but the confirming repeat failed. Treat the FC as possibly \
                           enrolled with this key — verify signing state before flying, and keep \
                           the key.",
                "partially_applied": true,
                "key_id": key_id,
            })),
        )
            .into_response();
    }

    let verified = observe(&mut stream, VERIFY_WINDOW, |f| {
        signed_by(f, target_system, &key)
    })
    .await;
    let agent_holds_key = req.agent_holds_key && store_agent_key(&state, &key);
    zeroize(&mut key);

    let enrolled_at = iso8601_seconds_utc(OffsetDateTime::now_utc());

    // Log the fingerprint only, never the key.
    tracing::info!(key_id = %key_id, link_id = req.link_id, target_system, verified, agent_holds_key, "signing enroll sent");
    (
        StatusCode::OK,
        Json(json!({
            "sent": true,
            "verified": verified,
            "key_id": key_id,
            "enrolled_at": enrolled_at,
            "agent_holds_key": agent_holds_key,
        })),
    )
        .into_response()
}

/// The agent-held key file, beside the agent config.
fn agent_key_path(state: &AppState) -> std::path::PathBuf {
    state
        .pairing_paths
        .config
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(AGENT_KEY_FILE)
}

/// Store the enrolled key for the router when `mavlink.agent_signing` is on.
/// Returns whether the key is now held by the agent.
fn store_agent_key(state: &AppState, key: &[u8]) -> bool {
    let config = crate::config::load_config_object(&state.pairing_paths.config);
    let enabled = config
        .pointer("/mavlink/agent_signing")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !enabled {
        tracing::info!("signing_agent_key_not_stored_agent_signing_off");
        return false;
    }
    match ados_protocol::sidecar::write_atomic(&agent_key_path(state), key, 0o600) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "signing_agent_key_store_failed");
            false
        }
    }
}

/// Remove the agent-held key: the FC no longer has one to check against.
fn delete_agent_key(state: &AppState) {
    match std::fs::remove_file(agent_key_path(state)) {
        Ok(()) => tracing::info!("signing_agent_key_deleted"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e, "signing_agent_key_delete_failed"),
    }
}

/// The optional `POST .../disable-on-fc` body: the same target an enrolment names,
/// so an FC enrolled at a system id other than 1 is the one that gets cleared.
#[derive(Debug, Deserialize)]
pub struct DisableRequest {
    #[serde(default)]
    pub target_system: Option<i64>,
    #[serde(default)]
    pub target_component: Option<i64>,
}

/// `POST /api/mavlink/signing/disable-on-fc` → clear the FC's signing store.
///
/// Takes an optional `{target_system, target_component}` body (defaulting to the
/// resolved FC target, with the same bounds enrolment uses). Gates on the FC
/// being connected
/// (`503` when not), then sends `SETUP_SIGNING` with an all-zero key + a zero
/// timestamp (ArduPilot reads this as "disable signing") and returns `{sent:
/// true, verified}`, `verified` being whether an unsigned frame from the target
/// was seen within [`VERIFY_WINDOW`]. The frame itself is unsigned, so an FC
/// that enforces signing applies it only from its USB channel; anywhere else
/// it is dropped and `verified` stays false. A socket failure is the Python
/// `503 "MAVLink command link unavailable"`; any other failure is the Python
/// `500 "disable failed"`.
pub async fn disable_on_fc(
    State(state): State<AppState>,
    body: Option<Json<DisableRequest>>,
) -> Response {
    if !state.fc_connected() {
        return detail(StatusCode::SERVICE_UNAVAILABLE, "FC not connected");
    }
    let fc = crate::fc_target::fc_target(&state);
    let (target_system, target_component) = match disable_target(body.map(|Json(b)| b), fc) {
        Ok(t) => t,
        Err(msg) => return detail(StatusCode::BAD_REQUEST, msg),
    };

    let zero_key = [0u8; SIGNING_KEY_LEN];
    let frame = match build_setup_signing_frame(target_system, target_component, &zero_key, 0) {
        Ok(bytes) => bytes,
        Err(()) => {
            tracing::error!("signing disable frame serialize failed");
            return detail(StatusCode::INTERNAL_SERVER_ERROR, "disable failed");
        }
    };

    let mut stream = match state.mavlink.open_ack_stream().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "signing disable connect failed");
            return detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "MAVLink command link unavailable",
            );
        }
    };
    if let Err(e) = stream.write_frame(&frame).await {
        tracing::warn!(error = %e, "signing disable send failed");
        return detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "MAVLink command link unavailable",
        );
    }
    delete_agent_key(&state);

    let verified = observe(&mut stream, VERIFY_WINDOW, |f| {
        unsigned_from(f, target_system)
    })
    .await;
    tracing::info!(
        target_system,
        target_component,
        verified,
        "signing disable sent"
    );
    (
        StatusCode::OK,
        Json(json!({ "sent": true, "verified": verified })),
    )
        .into_response()
}

/// Read FC frames off `stream` until one satisfies `matches` or `window` closes.
/// A closed or stalled stream ends the watch unproven.
async fn observe(
    stream: &mut AckStream,
    window: std::time::Duration,
    mut matches: impl FnMut(&[u8]) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        match stream.read_frame(deadline - now).await {
            FrameRead::Frame(f) if matches(&f) => return true,
            FrameRead::Frame(_) => {}
            FrameRead::Timeout | FrameRead::Eof => return false,
        }
    }
}

#[cfg(test)]
mod tests;
