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

/// How long a signing write watches the FC's frames for proof it applied.
const VERIFY_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// The source identity stamped on every signing frame, matching the Python
/// signing encoder (`srcSystem=255, srcComponent=MAV_COMP_ID_MISSIONPLANNER`), so
/// an enroll/disable from this surface is wire-identical to one the Python
/// routes emitted. The router forwards the frame verbatim, so this header is on
/// the wire and is the parity target.
const SOURCE_SYSTEM_ID: u8 = 255;
const SOURCE_COMPONENT_ID: u8 = 190;

/// The default target the Python signing helpers use when the body omits the
/// target fields: system 1, component 1.
const DEFAULT_TARGET_SYSTEM: u8 = 1;
const DEFAULT_TARGET_COMPONENT: u8 = 1;

/// The 32-byte signing-key length the FC's signing store expects.
const SIGNING_KEY_LEN: usize = 32;

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
    /// The target system id. Defaults to 1.
    #[serde(default = "default_target_system")]
    pub target_system: i64,
    /// The target component id. Defaults to 1.
    #[serde(default = "default_target_component")]
    pub target_component: i64,
}

fn default_target_system() -> i64 {
    DEFAULT_TARGET_SYSTEM as i64
}

fn default_target_component() -> i64 {
    DEFAULT_TARGET_COMPONENT as i64
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
    let target_system = match bounded_u8(req.target_system, 1, 255) {
        Ok(v) => v,
        Err(()) => return detail(StatusCode::BAD_REQUEST, "target_system out of range"),
    };
    let target_component = match bounded_u8(req.target_component, 0, 255) {
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
    zeroize(&mut key);

    let enrolled_at = iso8601_seconds_utc(OffsetDateTime::now_utc());

    // Log the fingerprint only, never the key.
    tracing::info!(key_id = %key_id, link_id = req.link_id, target_system, verified, "signing enroll sent");
    (
        StatusCode::OK,
        Json(json!({
            "sent": true,
            "verified": verified,
            "key_id": key_id,
            "enrolled_at": enrolled_at,
        })),
    )
        .into_response()
}

/// The optional `POST .../disable-on-fc` body: the same target an enrolment names,
/// so an FC enrolled at a system id other than 1 is the one that gets cleared.
#[derive(Debug, Deserialize)]
pub struct DisableRequest {
    #[serde(default = "default_target_system")]
    pub target_system: i64,
    #[serde(default = "default_target_component")]
    pub target_component: i64,
}

/// `POST /api/mavlink/signing/disable-on-fc` → clear the FC's signing store.
///
/// Takes an optional `{target_system, target_component}` body (default 1/1, the
/// same defaults and bounds enrolment uses). Gates on the FC being connected
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
    let (target_system, target_component) = match disable_target(body.map(|Json(b)| b)) {
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

/// The MAVLink frame layout the signing checks read.
const V2_MAGIC: u8 = 0xFD;
const V1_MAGIC: u8 = 0xFE;
const V2_HEADER_LEN: usize = 10;
const V2_CRC_LEN: usize = 2;
const V2_SIGNATURE_BLOCK_LEN: usize = 13;
const V2_SIGNATURE_LEN: usize = 6;
const IFLAG_SIGNED: u8 = 0x01;

/// Whether `frame` is a MAVLink 2 frame from `system` carrying a valid
/// signature under `key`. The signature is the first 6 bytes of
/// sha256(key ‖ every frame byte before the signature), which covers the
/// header, payload, checksum, link id and timestamp.
fn signed_by(frame: &[u8], system: u8, key: &[u8]) -> bool {
    if frame.len() < V2_HEADER_LEN
        || frame[0] != V2_MAGIC
        || frame[2] & IFLAG_SIGNED == 0
        || frame[5] != system
    {
        return false;
    }
    let total = V2_HEADER_LEN + usize::from(frame[1]) + V2_CRC_LEN + V2_SIGNATURE_BLOCK_LEN;
    if frame.len() != total {
        return false;
    }
    let (signed, signature) = frame.split_at(total - V2_SIGNATURE_LEN);
    let digest = Sha256::new()
        .chain_update(key)
        .chain_update(signed)
        .finalize();
    digest[..V2_SIGNATURE_LEN] == *signature
}

/// Whether `frame` is an unsigned frame from `system`: a MAVLink 2 frame with
/// the signed flag clear, or any MAVLink 1 frame (v1 has no signing).
fn unsigned_from(frame: &[u8], system: u8) -> bool {
    match frame.first() {
        Some(&V2_MAGIC) => {
            frame.len() >= V2_HEADER_LEN && frame[2] & IFLAG_SIGNED == 0 && frame[5] == system
        }
        Some(&V1_MAGIC) => frame.len() >= 6 && frame[3] == system,
        _ => false,
    }
}

/// The disable target: the body's system/component (bounded like enrolment), or
/// 1/1 when no body is sent.
fn disable_target(body: Option<DisableRequest>) -> Result<(u8, u8), &'static str> {
    let Some(b) = body else {
        return Ok((DEFAULT_TARGET_SYSTEM, DEFAULT_TARGET_COMPONENT));
    };
    let system = bounded_u8(b.target_system, 1, 255).map_err(|()| "target_system out of range")?;
    let component =
        bounded_u8(b.target_component, 0, 255).map_err(|()| "target_component out of range")?;
    Ok((system, component))
}

/// Parse a 64-char lowercase-hex string into a 32-byte key, mirroring the Python
/// `parse_key_hex` error messages verbatim so the 400 body is byte-identical:
/// a non-64-length input is `key_hex must be 64 hex chars, got <n>`; a non-hex
/// input is `key_hex is not valid hex: <reason>`.
fn parse_key_hex(key_hex: &str) -> Result<Vec<u8>, String> {
    if key_hex.len() != 64 {
        return Err(format!(
            "key_hex must be 64 hex chars, got {}",
            key_hex.len()
        ));
    }
    decode_hex(key_hex).map_err(|reason| format!("key_hex is not valid hex: {reason}"))
}

/// Decode a hex string (even length, only `0-9a-fA-F`) into bytes. Returns a
/// short reason string on a bad char or an odd length, used to build the Python
/// `key_hex is not valid hex:` message. (The length is already checked to 64 by
/// the caller, so the odd-length arm is defensive.)
fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd-length string".to_string());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/// One hex nibble (0-15) for an ASCII hex digit, or a reason string for a
/// non-hex byte.
fn hex_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        other => Err(format!("Non-hexadecimal digit found: '{}'", other as char)),
    }
}

/// Coerce a JSON-parsed integer into a `u8` within `[lo, hi]`, returning `Err`
/// when out of range — the Rust equivalent of the Pydantic `ge`/`le` field
/// validators. An in-range value is the byte; anything outside is a 400 at the
/// call site.
fn bounded_u8(value: i64, lo: i64, hi: i64) -> Result<u8, ()> {
    if (lo..=hi).contains(&value) {
        Ok(value as u8)
    } else {
        Err(())
    }
}

/// The first 8 hex chars of sha256 over the key — a fingerprint safe to display
/// and log, mirroring the Python `_fingerprint`.
fn fingerprint(key: &[u8]) -> String {
    let digest = Sha256::digest(key);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..8].to_string()
}

/// Seconds since 2015-01-01 UTC, in 10-microsecond units, mirroring the Python
/// `_initial_timestamp_10us` (`int((time.time() - EPOCH_2015) * 100_000)`).
fn initial_timestamp_10us(now: OffsetDateTime) -> u64 {
    let seconds_since_2015 = now.unix_timestamp() as f64 + now.nanosecond() as f64 / 1e9
        - EPOCH_2015_UNIX_SECONDS as f64;
    let scaled = (seconds_since_2015 * TEN_US_PER_SECOND) as i64;
    scaled.max(0) as u64
}

/// Build a `SETUP_SIGNING` v2 frame with the signing source identity. The key is
/// copied into the 32-byte field (callers pass exactly 32 bytes). Returns `Err`
/// only on a serialize failure, which the route maps to a 500.
fn build_setup_signing_frame(
    target_system: u8,
    target_component: u8,
    key: &[u8],
    initial_timestamp: u64,
) -> Result<Vec<u8>, ()> {
    let mut secret_key = [0u8; SIGNING_KEY_LEN];
    let n = key.len().min(SIGNING_KEY_LEN);
    secret_key[..n].copy_from_slice(&key[..n]);
    let msg = MavMessage::SETUP_SIGNING(SETUP_SIGNING_DATA {
        target_system,
        target_component,
        secret_key,
        initial_timestamp,
    });
    serialize_signing(&msg)
}

/// Serialize a message into a complete v2 frame with the signing source identity.
fn serialize_signing(msg: &MavMessage) -> Result<Vec<u8>, ()> {
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        // The router stamps its own sequence on its frames; a client-written
        // signing frame carries 0 (ArduPilot routes SETUP_SIGNING by target
        // regardless of the sequence).
        sequence: 0,
    };
    mavlink::serialize_v2(header, msg).map_err(|e| {
        tracing::error!(error = %e, "signing frame serialize failed");
    })
}

/// Overwrite a key buffer with zeros, mirroring the Python in-place zeroize so a
/// raw key never lingers in memory after the route returns.
fn zeroize(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
}

/// Render a UTC `OffsetDateTime` as `YYYY-MM-DDTHH:MM:SS+00:00`, byte-for-byte
/// what the Python `datetime.now(tz=timezone.utc).isoformat(timespec="seconds")`
/// produces: a zero-padded date + time at seconds precision and the explicit
/// `+00:00` UTC offset (Python uses the numeric offset, never `Z`). Built from the
/// date/time fields directly so no format-description machinery is needed.
fn iso8601_seconds_utc(dt: OffsetDateTime) -> String {
    let date = dt.date();
    let time = dt.time();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        date.year(),
        u8::from(date.month()),
        date.day(),
        time.hour(),
        time.minute(),
        time.second(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_clears_the_fc_the_caller_names() {
        let named = DisableRequest {
            target_system: 7,
            target_component: 1,
        };
        assert_eq!(disable_target(Some(named)), Ok((7, 1)));
        assert_eq!(
            disable_target(None),
            Ok((DEFAULT_TARGET_SYSTEM, DEFAULT_TARGET_COMPONENT))
        );
        let bad = DisableRequest {
            target_system: 0,
            target_component: 1,
        };
        assert!(disable_target(Some(bad)).is_err());
    }
    use serde_json::Value;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    // ── hex parsing: parity with the Python parse_key_hex messages ────────────

    #[test]
    fn parse_key_hex_accepts_a_64_char_lowercase_key() {
        let hex = "00".repeat(32);
        let bytes = parse_key_hex(&hex).unwrap();
        assert_eq!(bytes.len(), 32);
        assert!(bytes.iter().all(|b| *b == 0));
    }

    #[test]
    fn parse_key_hex_round_trips_a_known_value() {
        let hex = format!("0102{}", "ff".repeat(30));
        let bytes = parse_key_hex(&hex).unwrap();
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[1], 0x02);
        assert_eq!(bytes[2], 0xff);
    }

    #[test]
    fn parse_key_hex_wrong_length_is_the_python_message() {
        let err = parse_key_hex("00").unwrap_err();
        assert_eq!(err, "key_hex must be 64 hex chars, got 2");
        // A too-long input also reports its actual length.
        let long = "0".repeat(65);
        assert_eq!(
            parse_key_hex(&long).unwrap_err(),
            "key_hex must be 64 hex chars, got 65"
        );
    }

    #[test]
    fn parse_key_hex_non_hex_is_the_python_message_prefix() {
        // 64 chars but with a non-hex digit: the message carries the "not valid
        // hex:" prefix the Python route surfaces from binascii.
        let bad = format!("0g{}", "0".repeat(62));
        let err = parse_key_hex(&bad).unwrap_err();
        assert!(
            err.starts_with("key_hex is not valid hex:"),
            "unexpected: {err}"
        );
    }

    // ── fingerprint: parity with the Python _fingerprint ──────────────────────

    #[test]
    fn fingerprint_is_first_eight_hex_of_sha256() {
        // sha256("") = e3b0c44298fc1c14... → first 8 = "e3b0c442".
        assert_eq!(fingerprint(b""), "e3b0c442");
        // A 32-zero-byte key has a fixed, stable fingerprint.
        let fp = fingerprint(&[0u8; 32]);
        assert_eq!(fp.len(), 8);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── bounded coercion: parity with the Pydantic field validators ───────────

    #[test]
    fn bounded_u8_enforces_the_range() {
        assert_eq!(bounded_u8(1, 1, 255), Ok(1));
        assert_eq!(bounded_u8(255, 1, 255), Ok(255));
        assert_eq!(bounded_u8(0, 0, 255), Ok(0));
        // Out of range (0 below a min-1, 256 above the max).
        assert_eq!(bounded_u8(0, 1, 255), Err(()));
        assert_eq!(bounded_u8(256, 0, 255), Err(()));
        assert_eq!(bounded_u8(-1, 0, 255), Err(()));
    }

    // ── timestamp: parity with the Python _initial_timestamp_10us ─────────────

    #[test]
    fn initial_timestamp_is_ten_us_since_2015() {
        // At exactly the 2015 epoch the timestamp is 0; one second later is
        // 100_000 (10-µs units), matching the Python `* 100_000` scaling.
        let epoch_2015 = OffsetDateTime::from_unix_timestamp(EPOCH_2015_UNIX_SECONDS).unwrap();
        assert_eq!(initial_timestamp_10us(epoch_2015), 0);
        let one_second_later =
            OffsetDateTime::from_unix_timestamp(EPOCH_2015_UNIX_SECONDS + 1).unwrap();
        assert_eq!(initial_timestamp_10us(one_second_later), 100_000);
    }

    // ── SETUP_SIGNING frame: the wire bytes carry the key + the identity ──────

    #[test]
    fn setup_signing_frame_round_trips_with_the_key_and_identity() {
        let key: Vec<u8> = (0u8..32).collect();
        let frame = build_setup_signing_frame(1, 1, &key, 123).unwrap();
        // A v2 frame starts with the 0xFD magic byte.
        assert_eq!(frame[0], 0xFD);
        let (header, msg) = mavlink::parse_v2(&frame).unwrap();
        // The signing source identity is on the wire (255 / 190).
        assert_eq!(header.system_id, 255);
        assert_eq!(header.component_id, 190);
        match msg {
            MavMessage::SETUP_SIGNING(d) => {
                assert_eq!(d.target_system, 1);
                assert_eq!(d.target_component, 1);
                assert_eq!(d.initial_timestamp, 123);
                assert_eq!(d.secret_key.to_vec(), key);
            }
            other => panic!("expected SETUP_SIGNING, got {other:?}"),
        }
    }

    #[test]
    fn disable_frame_is_setup_signing_all_zero() {
        let zero = [0u8; 32];
        let frame = build_setup_signing_frame(1, 1, &zero, 0).unwrap();
        let (_h, msg) = mavlink::parse_v2(&frame).unwrap();
        match msg {
            MavMessage::SETUP_SIGNING(d) => {
                assert_eq!(d.initial_timestamp, 0);
                assert!(d.secret_key.iter().all(|b| *b == 0));
            }
            other => panic!("expected SETUP_SIGNING, got {other:?}"),
        }
    }

    // ── ISO-8601 seconds precision: parity with isoformat(timespec="seconds") ─

    #[test]
    fn enrolled_at_matches_python_isoformat_seconds() {
        // A fixed UTC instant renders as the Python isoformat(timespec="seconds")
        // shape: zero-padded date + time and the explicit +00:00 offset, no
        // fractional seconds, never a `Z`.
        let dt = OffsetDateTime::from_unix_timestamp(1_780_000_000).unwrap();
        let got = iso8601_seconds_utc(dt);
        // 1780000000 → 2026-05-28T20:26:40 UTC.
        assert_eq!(got, "2026-05-28T20:26:40+00:00");
        // The shape is fixed-width with the +00:00 offset and no subsecond part.
        assert!(got.ends_with("+00:00"));
        assert!(!got.contains('.'));
        assert!(!got.contains('Z'));
    }

    // ── the write-path tests: against a mock mavlink socket ───────────────────

    /// Build an AppState whose mavlink client points at the given socket path and
    /// whose state snapshot reports the FC connected (so the gate passes). The
    /// other seams are inert defaults the signing-write routes never touch.
    fn state_with_mavlink(sock: std::path::PathBuf, fc_connected: bool) -> AppState {
        use crate::auth::PairingState;
        use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
        use crate::state::PairingPaths;
        use std::sync::Arc;

        let dir = sock.parent().unwrap().to_path_buf();
        let state_client = StateIpcClient::disconnected();
        if fc_connected {
            state_client.set_snapshot_for_test(json!({ "fc_connected": true }));
        }
        let pairing_paths = PairingPaths {
            config: dir.join("config.yaml"),
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
            profile_conf: dir.join("profile.conf"),
            mesh_role: dir.join("mesh-role"),
            relay_secret: dir.join("relay-peer-secret"),
        };
        AppState::new(
            Arc::new(PairingState::with_path(dir.join("pairing.json"))),
            state_client,
            MavlinkIpcClient::new(sock),
            LogdQueryClient::new(dir.join("logd-query.sock")),
            dir.join("board.json"),
            pairing_paths,
            std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.join("dashboard-pin.json"),
            )),
            std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.join("mcp-token.json"),
            )),
        )
    }

    /// A fake router: accepts ONE connection, reads `n` length-prefixed frames
    /// from it, broadcasts `replies` back on the same connection, then closes it.
    /// Returns the frames it read.
    fn fake_router(
        listener: UnixListener,
        n: usize,
        replies: Vec<Vec<u8>>,
    ) -> tokio::task::JoinHandle<Vec<Vec<u8>>> {
        use tokio::io::AsyncWriteExt;
        tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut frames = Vec::with_capacity(n);
            for _ in 0..n {
                frames.push(read_framed(&mut conn).await);
            }
            for reply in replies {
                let wire = ados_protocol::frame::encode_frame(
                    &reply,
                    ados_protocol::frame::MAVLINK_MAX_FRAME,
                )
                .unwrap();
                conn.write_all(&wire).await.unwrap();
            }
            frames
        })
    }

    /// Read one length-prefixed frame (the 4-byte big-endian prefix + the payload)
    /// off a connected Unix stream, returning the raw payload bytes.
    async fn read_framed(conn: &mut tokio::net::UnixStream) -> Vec<u8> {
        use ados_protocol::frame::{decode_len, HEADER_SIZE, MAVLINK_MAX_FRAME};
        let mut header = [0u8; HEADER_SIZE];
        conn.read_exact(&mut header).await.unwrap();
        let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
        let mut body = vec![0u8; len];
        conn.read_exact(&mut body).await.unwrap();
        body
    }

    /// An unsigned MAVLink 2 HEARTBEAT from the FC at `system`.
    fn fc_heartbeat(system: u8) -> Vec<u8> {
        use ados_protocol::mavlink::ardupilotmega::{
            MavAutopilot, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
        };
        let header = MavHeader {
            system_id: system,
            component_id: 1,
            sequence: 0,
        };
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        });
        mavlink::serialize_v2(header, &msg).unwrap()
    }

    /// `frame` with the signed flag set and a signature block under `key`
    /// appended. The checksum is left as it was; the route checks the signature,
    /// which is what proves the key.
    fn sign(mut frame: Vec<u8>, key: &[u8]) -> Vec<u8> {
        frame[2] |= IFLAG_SIGNED;
        frame.push(0); // link id
        frame.extend_from_slice(&[1, 0, 0, 0, 0, 0]); // timestamp
        let digest = Sha256::new()
            .chain_update(key)
            .chain_update(&frame)
            .finalize();
        frame.extend_from_slice(&digest[..V2_SIGNATURE_LEN]);
        frame
    }

    fn enroll_body(key: &[u8]) -> EnrollRequest {
        EnrollRequest {
            key_hex: key.iter().map(|b| format!("{b:02x}")).collect(),
            link_id: 0,
            target_system: 1,
            target_component: 1,
        }
    }

    #[tokio::test]
    async fn enroll_is_verified_by_a_frame_signed_with_the_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let key: Vec<u8> = (0u8..32).collect();
        let other_key = [7u8; 32];
        // Before the key lands the FC is unsigned or signs with an older key;
        // only a frame signed with THIS key proves the enrollment.
        let server = fake_router(
            listener,
            2,
            vec![
                fc_heartbeat(1),
                sign(fc_heartbeat(1), &other_key),
                sign(fc_heartbeat(1), &key),
            ],
        );

        let state = state_with_mavlink(sock.clone(), true);
        let body = enroll_body(&key);
        let handle = tokio::spawn(async move { enroll_fc(State(state), Json(body)).await });

        let frames = server.await.unwrap();
        assert_eq!(frames.len(), 2, "the enroll sends the frame twice");
        // The two frames are byte-identical (the key is encoded once, sent twice).
        assert_eq!(frames[0], frames[1]);

        let resp = handle.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let value = body_json(resp).await;
        assert_eq!(value["sent"], json!(true));
        assert_eq!(value["verified"], json!(true));
        assert!(value.get("success").is_none());
        // The fingerprint is the first 8 hex of sha256(key), never the key.
        assert_eq!(value["key_id"], json!(fingerprint(&key)));
        // enrolled_at is the ISO-8601 seconds-precision UTC string.
        let enrolled = value["enrolled_at"].as_str().unwrap();
        assert!(enrolled.contains('T'));
        assert!(enrolled.ends_with("+00:00"));

        // The frame on the wire is a SETUP_SIGNING carrying the key + identity.
        let (header, msg) = mavlink::parse_v2(&frames[0]).unwrap();
        assert_eq!(header.system_id, 255);
        assert_eq!(header.component_id, 190);
        match msg {
            MavMessage::SETUP_SIGNING(d) => {
                assert_eq!(d.secret_key.to_vec(), key);
            }
            other => panic!("expected SETUP_SIGNING, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn enroll_is_unverified_when_no_frame_carries_the_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let key: Vec<u8> = (0u8..32).collect();
        // Unsigned, signed with another key, and signed with this key by a
        // different vehicle: none of them shows THIS FC holding THIS key.
        let server = fake_router(
            listener,
            2,
            vec![
                fc_heartbeat(1),
                sign(fc_heartbeat(1), &[7u8; 32]),
                sign(fc_heartbeat(2), &key),
            ],
        );
        let state = state_with_mavlink(sock, true);
        let resp = enroll_fc(State(state), Json(enroll_body(&key))).await;
        server.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let value = body_json(resp).await;
        assert_eq!(value["sent"], json!(true));
        assert_eq!(value["verified"], json!(false));
    }

    #[tokio::test]
    async fn enroll_with_fc_disconnected_is_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let state = state_with_mavlink(sock, false);
        let body = EnrollRequest {
            key_hex: "00".repeat(32),
            link_id: 0,
            target_system: 1,
            target_component: 1,
        };
        let resp = enroll_fc(State(state), Json(body)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let value = body_json(resp).await;
        assert_eq!(value, json!({ "detail": "FC not connected" }));
    }

    #[tokio::test]
    async fn enroll_with_a_bad_key_is_a_400_with_the_parse_message() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let state = state_with_mavlink(sock, true);
        let body = EnrollRequest {
            key_hex: "deadbeef".to_string(), // too short
            link_id: 0,
            target_system: 1,
            target_component: 1,
        };
        let resp = enroll_fc(State(state), Json(body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let value = body_json(resp).await;
        assert_eq!(
            value,
            json!({ "detail": "key_hex must be 64 hex chars, got 8" })
        );
    }

    #[tokio::test]
    async fn enroll_with_an_absent_socket_is_the_link_unavailable_503() {
        let dir = tempfile::tempdir().unwrap();
        // No listener bound → the send fails on the first frame.
        let sock = dir.path().join("absent.sock");
        let state = state_with_mavlink(sock, true);
        let body = EnrollRequest {
            key_hex: "00".repeat(32),
            link_id: 0,
            target_system: 1,
            target_component: 1,
        };
        let resp = enroll_fc(State(state), Json(body)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let value = body_json(resp).await;
        assert_eq!(
            value,
            json!({ "detail": "MAVLink command link unavailable" })
        );
    }

    #[tokio::test]
    async fn disable_is_verified_by_an_unsigned_frame_from_the_fc() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let key = [3u8; 32];
        // A frame still in flight from before the clear is signed; the unsigned
        // one after it shows the store was cleared.
        let server = fake_router(
            listener,
            1,
            vec![sign(fc_heartbeat(1), &key), fc_heartbeat(1)],
        );

        let state = state_with_mavlink(sock, true);
        let resp = disable_on_fc(State(state), None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let value = body_json(resp).await;
        assert_eq!(value, json!({ "sent": true, "verified": true }));

        let frames = server.await.unwrap();
        let (_h, msg) = mavlink::parse_v2(&frames[0]).unwrap();
        match msg {
            MavMessage::SETUP_SIGNING(d) => {
                assert_eq!(d.initial_timestamp, 0);
                assert!(d.secret_key.iter().all(|b| *b == 0));
            }
            other => panic!("expected SETUP_SIGNING, got {other:?}"),
        }
    }

    /// An enrolled FC drops the unsigned clear on a non-USB link and keeps
    /// signing. The route must not report that as done.
    #[tokio::test]
    async fn disable_is_unverified_while_the_fc_still_signs() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let key = [3u8; 32];
        let server = fake_router(
            listener,
            1,
            vec![sign(fc_heartbeat(1), &key), sign(fc_heartbeat(1), &key)],
        );
        let state = state_with_mavlink(sock, true);
        let resp = disable_on_fc(State(state), None).await;
        server.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_json(resp).await,
            json!({ "sent": true, "verified": false })
        );
    }

    #[tokio::test]
    async fn disable_with_fc_disconnected_is_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink.sock");
        let state = state_with_mavlink(sock, false);
        let resp = disable_on_fc(State(state), None).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(resp).await,
            json!({ "detail": "FC not connected" })
        );
    }

    #[tokio::test]
    async fn disable_with_an_absent_socket_is_the_link_unavailable_503() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("absent.sock");
        let state = state_with_mavlink(sock, true);
        let resp = disable_on_fc(State(state), None).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(resp).await,
            json!({ "detail": "MAVLink command link unavailable" })
        );
    }

    /// Read an axum response body into a JSON Value for the parity asserts.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
