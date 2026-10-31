//! Pairing routes: the LAN node-identity probe + the local pairing handshake.
//!
//! These are the contract-critical routes: the GCS Add-a-Node flow probes
//! `/api/pairing/info`, then POSTs `/api/pairing/claim`, and stores the returned
//! key. The wire shape is fixed down to every field name and the null-as-null
//! shape; a drift breaks pairing silently.
//!
//! - **`GET /api/pairing/info`** — the node-identity probe. Emits all 19 fields
//!   even when null (no field is ever omitted), reading device identity +
//!   profile off `/etc/ados/config.yaml`, the cloud-pair state off
//!   `pairing.json`, the radio-pair signal off the `/etc/ados/wfb` key files, the
//!   bind session off the `/run/ados/bind-state.json` sentinel, and the FC triple
//!   off the live state snapshot. Every optional read is fault-tolerant. The one
//!   exception is `pairing.json` itself: an unreadable or malformed file is a
//!   `503`, never the unpaired default, because "unpaired" is the state in which
//!   anyone on the LAN may claim a fresh key.
//! - **`GET /api/pairing/code`** — the live code while unpaired, regenerated
//!   once it is older than `pairing_store::CODE_TTL_SECONDS` (the same lifetime
//!   the Python writer applies); 409 when paired.
//! - **`POST /api/pairing/claim`** — claim the agent for a user. Writes
//!   `pairing.json` (the same document the Python `PairingManager.claim`
//!   writes) and returns the
//!   key; 409 when already paired; 503 when the pairing file cannot be read.
//!   Never served to a request that crossed the radio relay.
//! - **`POST /api/pairing/unpair`** — clear pairing + mint a fresh code; 409 when
//!   not paired. Gated by the auth middleware (it is not in the public set). An
//!   unreadable pairing file is cleared too: the gate already restricts it to the
//!   on-box operator, and this is how that operator recovers the node.
//! - **`POST /api/pairing/accept`** — accept a code Mission Control generated:
//!   register this device against it at the pairing backend's
//!   `/pairing/register`, then claim locally under the key it registered.
//!   Always `200 {ok, error, message, owner_id, paired_at, device_id}` with the
//!   outcome in `ok`; never served to a request that crossed the radio relay.
//!
//! Every write happens under `pairing_store::lock_writers`, with the
//! already-paired check re-read inside the lock, so two concurrent claims cannot
//! both mint a key.
//!
//! The pairing code is withheld from a remote caller (anything relayed through a
//! proxy or tunnel, or a public-WAN host): `info` reports it as null and `code`
//! refuses. It is a claim credential for the device's own networks only.
//!
//! `mdns_host` is the RESOLVABLE reach name — the host name avahi actually
//! publishes (including a collision rename like `<hostname>-2.local`), else the
//! system hostname's first label under `.local`, resolved through
//! [`ados_protocol::reach::mdns_hostname`]. It is
//! deliberately NOT a constructed `ados-<6hex>.local`: nothing publishes an
//! A-record for that name, so a GCS that stores it as a node's canonical reach
//! stores a name that resolves nowhere. A host with no usable hostname has no
//! mDNS reach at all and the field is emitted as `""` rather than as a name this
//! node cannot prove. The `_ados._tcp` advert this daemon publishes at boot
//! (`crate::mdns`) uses the identical name as its SRV target, so the browse
//! record and the probe response name one host.

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use ados_protocol::pairing_posture::CallerClass;

use crate::config::PairingConfig;
use crate::pairing_store::{self, PairingDoc};
use crate::profile::current_profile_and_role_at;
use crate::routes::detail;
use crate::state::{AppState, PairingPaths};

mod accept;
mod claim;

pub use accept::accept_pairing_code;
pub use accept::AcceptCodeRequest;
#[cfg(test)]
use accept::*;
pub use claim::claim_pairing;
pub use claim::unpair;
pub use claim::ClaimRequest;

/// What an operator is told when the pairing file exists but cannot be read or
/// parsed: the node refuses to act as unpaired (which would open the claim) and
/// names the recovery.
const UNREADABLE_MESSAGE: &str =
    "The pairing state on this device is unreadable. Unpair it on the device itself to recover.";

/// The `503` for a pairing file that cannot be read or parsed.
fn pairing_unreadable(reason: &str) -> Response {
    tracing::error!(reason, "pairing_state_unreadable");
    detail(StatusCode::SERVICE_UNAVAILABLE, UNREADABLE_MESSAGE)
}

/// Whether the caller may see the pairing code: anyone on the device's own
/// networks, never a remote caller. A request with no caller class (never
/// produced by the edges) is treated as remote.
fn may_see_code(caller: Option<Extension<CallerClass>>) -> bool {
    !matches!(
        caller.map_or(CallerClass::Remote, |Extension(c)| c),
        CallerClass::Remote
    )
}

/// `GET /api/pairing/info` → the 19-field node-identity probe.
///
/// Doubles as the Mission Control "probe" endpoint when an operator pastes a
/// hostname into Add-a-Node. Every field is emitted even when null (the GCS keys
/// off exact field presence), so `bind_state` and `radio` serialize as JSON
/// `null`, never omitted. Each underlying read is guarded so a partially
/// configured agent answers 200 with a usable shape rather than 500.
pub async fn get_pairing_info(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
) -> Response {
    let paths = &state.pairing_paths;

    // Device identity + profile, from the cached config snapshot (one parse per
    // change of the file; this is a probe the GCS polls).
    let cfg = PairingConfig::cached(&paths.config);
    let device_id = cfg.agent.device_id.clone();
    // Falls back to "ADOS Agent" when the config name is empty
    // (`name or "ADOS Agent"`); the config default is "my-drone", so a configured
    // agent carries a real name here.
    let name = if cfg.agent.name.is_empty() {
        "ADOS Agent".to_string()
    } else {
        cfg.agent.name.clone()
    };
    let (profile, role) =
        current_profile_and_role_at(&cfg.agent.profile, &paths.profile_conf, &paths.mesh_role);
    let radio_peer_device_id = cfg.radio_peer_device_id();

    // The name this host actually answers to. Empty when the host has no
    // usable hostname: the GCS falls back to the IPv4 it just reached us on,
    // which is a proven reach, where a constructed name is not.
    let mdns_host = ados_protocol::reach::mdns_hostname().unwrap_or_default();

    // Cloud-pair state off pairing.json. Absent is unpaired; unreadable is not.
    let doc = match PairingDoc::read(&paths.pairing_json) {
        Ok(doc) => doc,
        Err(reason) => return pairing_unreadable(&reason),
    };

    // Radio-pair signal: the same predicate `GET /api/wfb/pair` answers from —
    // this role's own key file, exactly 64 bytes, with a readable fingerprint.
    let radio_paired = crate::wfb_pair_state::paired_key_fingerprint(
        &paths.wfb_key_dir,
        crate::wfb_pair_state::bind_role_for(&profile),
    )
    .is_some();

    // The folded bind-session snapshot from the cross-process sentinel.
    let bind_state = read_bind_state(paths);

    // FC presence from the live state snapshot's runtime extras.
    let (fc_connected, fc_port, fc_baud) = fc_from_snapshot(state.state.snapshot().as_ref());

    // The same live code `/code` serves. One that cannot be minted or persisted
    // is reported as absent here rather than failing the whole identity probe;
    // `/code` says why.
    let pairing_code = if doc.is_paired() {
        None
    } else {
        code_for_caller(paths, caller).await
    };

    Json(json!({
        "device_id": device_id,
        "name": name,
        "version": state.agent_version(),
        // The native surface has no in-process HAL detect, so the board name is
        // read live off the `name` field of the board sidecar
        // (`/run/ados/board.json`) the detector persists — the same on-disk source
        // the status route's board block reads. Defaults to "unknown" when the
        // sidecar is absent (a fresh boot before the first status write).
        "board": crate::state::board_name(&state.board_path),
        "paired": doc.is_paired(),
        "radio_paired": radio_paired,
        "radio_peer_device_id": radio_peer_device_id,
        "pairing_code": pairing_code,
        "owner_id": doc.info_owner_id(),
        "paired_at": doc.info_paired_at(),
        "mdns_host": mdns_host,
        "profile": profile,
        "role": role,
        // Native-vs-packaged badge, computed in process from the installed
        // binaries for this profile (cached for a few seconds), so it stays
        // true with no Python process running.
        "runtime_mode": crate::runtime_mode::current_in(&profile, &state.bin_dir),
        "bind_state": bind_state,
        // Reserved for a future in-process radio reader; always null today (the
        // GCS falls back to radio_paired).
        "radio": Value::Null,
        "fc_connected": fc_connected,
        "fc_port": fc_port,
        "fc_baud": fc_baud,
    }))
    .into_response()
}

/// `GET /api/pairing/code` → `{"code": <code>}` while unpaired; 409
/// `{"detail":"Already paired"}` while paired.
///
/// Returns the persisted code while it is live, and mints then persists a new
/// one when there is none or it has outlived `pairing_store::CODE_TTL_SECONDS`,
/// the same rule the Python `get_or_create_code` applies, so every surface shows
/// one code. Paired agents 409.
pub async fn get_pairing_code(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
) -> Response {
    if !may_see_code(caller) {
        return detail(
            StatusCode::FORBIDDEN,
            "The pairing code is only served on the device's own networks.",
        );
    }
    match live_code(&state.pairing_paths).await {
        Ok(Some(code)) => (StatusCode::OK, Json(json!({ "code": code }))).into_response(),
        Ok(None) => detail(StatusCode::CONFLICT, "Already paired"),
        Err(CodeError::Unreadable(reason)) => pairing_unreadable(&reason),
        // A code that could not be persisted is refused: an in-memory one is
        // not what the device holds and would change on every call.
        Err(CodeError::Persist(e)) => {
            tracing::warn!(error = %e, "pairing code persist failed");
            detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "Failed to persist a pairing code",
            )
        }
    }
}

/// Why no live code could be produced.
enum CodeError {
    Unreadable(String),
    Persist(std::io::Error),
}

/// The live pairing code of an unpaired node, minted under the writer lock
/// when absent or expired; `None` when paired.
async fn live_code(paths: &PairingPaths) -> Result<Option<String>, CodeError> {
    let now = now_unix_seconds();
    let doc = PairingDoc::read(&paths.pairing_json).map_err(CodeError::Unreadable)?;
    if doc.is_paired() {
        return Ok(None);
    }
    // The code and the pending key travel together; a live code with no key
    // beside it still needs a write.
    let has_pending_key = doc
        .pending_api_key
        .as_deref()
        .is_some_and(|k| !k.is_empty());
    if let Some(code) = doc.live_code(now).filter(|_| has_pending_key) {
        return Ok(Some(code));
    }
    let _lock = pairing_store::lock_writers(&paths.pairing_json)
        .await
        .map_err(CodeError::Persist)?;
    // Re-read under the lock: another writer may have paired the node or
    // minted a code meanwhile, and either must win over a second mint.
    let doc = PairingDoc::read(&paths.pairing_json).map_err(CodeError::Unreadable)?;
    if doc.is_paired() {
        return Ok(None);
    }
    pairing_store::ensure_code(&paths.pairing_json, now)
        .map(Some)
        .map_err(CodeError::Persist)
}

/// The pairing code `caller` may be shown: the live code of an unpaired node,
/// for a caller on the device's own networks. `None` for a remote caller, a
/// paired node, or a code that could not be minted or persisted (`/code` says
/// why), so a read that merely displays the code never fails on it.
pub(crate) async fn code_for_caller(
    paths: &PairingPaths,
    caller: Option<Extension<CallerClass>>,
) -> Option<String> {
    if !may_see_code(caller) {
        return None;
    }
    live_code(paths).await.ok().flatten()
}

/// JSON truthiness as the backend's reply is read: false, null, zero and empty
/// strings, arrays and objects are false.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// --- helpers ---

/// Fold the WFB bind-session snapshot from the cross-process sentinel. Absent
/// file (no bind has run) or a sentinel with no `state` → `null`. Each field is
/// read by key with a missing field tolerated. Only six fields are emitted, each
/// as JSON null when absent in the sentinel.
fn read_bind_state(paths: &PairingPaths) -> Value {
    let Ok(text) = std::fs::read_to_string(&paths.bind_state) else {
        return Value::Null;
    };
    let Ok(sess) = serde_json::from_str::<Value>(&text) else {
        return Value::Null;
    };
    let Some(obj) = sess.as_object() else {
        return Value::Null;
    };
    // Best-effort schema-drift signal (never reject): warn when the sentinel was
    // written by an agent with a different schema version, then read anyway. The
    // writer const lives in the supervisor crate, so compare against the shared
    // registry.
    let got = obj.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("bind-state") {
        ados_protocol::sidecar::check_sidecar_version("bind-state", got, ours);
    }
    // Only fold when the sentinel's `state` is truthy.
    let state_truthy = obj
        .get("state")
        .map(|v| !v.is_null() && v != &json!("") && v != &json!(false))
        .unwrap_or(false);
    if !state_truthy {
        return Value::Null;
    }
    json!({
        "state": obj.get("state").cloned().unwrap_or(Value::Null),
        "phase": obj.get("phase").cloned().unwrap_or(Value::Null),
        // `bool(sess.get("active", False))` → a missing/falsey active is false.
        "active": obj.get("active").and_then(Value::as_bool).unwrap_or(false),
        "error": obj.get("error").cloned().unwrap_or(Value::Null),
        "finished_at": obj.get("finished_at").cloned().unwrap_or(Value::Null),
        "fingerprint": obj.get("fingerprint").cloned().unwrap_or(Value::Null),
    })
}

/// Read the FC connection triple out of the live state snapshot's runtime extras.
/// Returns `(fc_connected, fc_port, fc_baud)` as JSON values: a connected FC reports a string port + int baud, an
/// absent / disconnected one reports `false` + JSON `null` + JSON `null` (the
/// pairing-info defaults are `None`, unlike the status route's `""`/`0`).
pub(crate) fn fc_from_snapshot(snapshot: Option<&Value>) -> (Value, Value, Value) {
    let obj = snapshot.and_then(Value::as_object);
    let connected = obj
        .and_then(|m| m.get("fc_connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // A missing, null, or falsey port / baud → null.
    let port = obj
        .and_then(|m| m.get("fc_port"))
        .filter(|v| v.is_string() && !v.as_str().unwrap_or("").is_empty())
        .cloned()
        .unwrap_or(Value::Null);
    let baud = obj
        .and_then(|m| m.get("fc_baud"))
        .filter(|v| v.as_i64().map(|n| n != 0).unwrap_or(false))
        .cloned()
        .unwrap_or(Value::Null);
    (json!(connected), port, baud)
}

/// Wall-clock unix seconds (fractional), matching the Python `time.time()` the
/// claim/unpair/code writers stamp.
fn now_unix_seconds() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests;
