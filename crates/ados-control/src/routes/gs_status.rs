//! Ground-station profile status + distributed-receive read routes.
//!
//! Five read-only routes the GCS Hardware tab + the OLED dashboard poll, all
//! gated on the node resolving to the ground-station profile. On a drone-profile
//! node every one answers `404` with the body `{"detail": {"error": {"code":
//! "E_PROFILE_MISMATCH"}}}` — the same shape the FastAPI `_require_ground_profile`
//! gate raises, so the GCS distinguishes "wrong profile" from "endpoint missing".
//!
//! - **`GET /api/v1/ground-station/status`** — the OLED-aligned composite
//!   snapshot: profile, the paired-drone identity (device id + key fingerprint),
//!   the live radio link view, an empty GCS-client block, the AP network view
//!   (`ap_ssid` resolved off config, `ap_ip` the `192.168.4.1` gateway while the
//!   hostapd unit is active), the system snapshot (CPU/RAM/temp/uptime/version),
//!   the recorder flag mirrored into a `video` block, the role block, and (for a
//!   relay/receiver) the mesh block.
//! - **`GET /api/v1/ground-station/wfb`** — the live radio view
//!   `{channel, tx_power_dbm}` from the radio sidecar, each `null` when the
//!   radio is not reporting.
//! - **`GET /api/v1/ground-station/wfb/relay/status`** — relay-role fragment
//!   counters, store-first off the `gs.relay_state` event, sidecar-fallback off
//!   `/run/ados/wfb-relay.json`. `404` `E_WRONG_ROLE` off a relay node.
//! - **`GET /api/v1/ground-station/wfb/receiver/relays`** — receiver-role per-relay
//!   counters, store-first off `gs.receiver_state`, sidecar-fallback off
//!   `/run/ados/wfb-receiver.json`, projected to `{relays}`. `404` off a receiver.
//! - **`GET /api/v1/ground-station/wfb/receiver/combined`** — receiver-role
//!   combined FEC output stats `{fragments_after_dedup, fec_repaired, output_kbps,
//!   up}`, same store-first/sidecar-fallback. `404` off a receiver.
//!
//! Every read is fault-tolerant: an absent store / sidecar / key file degrades to
//! the same empty/default shape the FastAPI route returns when its own source is
//! unavailable, never a 500. The native front runs the radio in sibling processes
//! (no in-process manager to call), so the live radio legs read the durable store
//! and the on-disk sidecars the sibling services write — exactly the seams the
//! FastAPI handlers fall back to. The RECORDER is the exception: it is held
//! in-process by [`crate::routes::gs_recording`] (start and stop arrive as
//! separate requests), so the recording legs read that singleton directly.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use ados_protocol::paths;
use ados_protocol::sidecar::{file_age, read_fresh_at};

use crate::state::AppState;
use crate::wfb_pair_state::read_public_fingerprint;

mod link;
mod network;
mod relay;
mod system;

use link::*;
use network::*;
pub use relay::get_wfb;
pub use relay::get_wfb_receiver_combined;
pub use relay::get_wfb_receiver_relays;
pub use relay::get_wfb_relay_status;
use relay::*;
use system::*;

// ---------------------------------------------------------------------------
// Profile gate.
// ---------------------------------------------------------------------------

/// The resolved-profile gate. Returns `Some(role)` (the ground-station mesh role:
/// `"direct" | "relay" | "receiver"`) when the node resolves to a ground station,
/// else `None` (the caller answers the FastAPI `404` profile-mismatch body).
/// Resolves through `current_profile_and_role` (the same source of truth the node
/// advertises on the wire), so a `profile: auto` node that resolves to a ground
/// station via `profile.conf` passes the gate, matching the Python
/// `_require_ground_profile`.
fn ground_station_role(state: &AppState) -> Option<String> {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    if profile == "ground-station" {
        // A ground station always carries Some(role); the `unwrap_or` keeps the
        // resolution total without a panic, defaulting to the Python `"direct"`.
        Some(role.unwrap_or_else(|| "direct".to_string()))
    } else {
        None
    }
}

/// The `404` profile-mismatch response, byte-identical to the FastAPI
/// `HTTPException(status_code=404, detail={"error": {"code": "E_PROFILE_MISMATCH"}})`
/// (FastAPI wraps the `detail` dict under a top-level `"detail"` key).
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

/// The `404` wrong-role response, byte-identical to the FastAPI
/// `HTTPException(status_code=404, detail={"error": {"code": "E_WRONG_ROLE",
/// "required": <role>}})`. Used by the relay/receiver routes when the node's role
/// is not the one they serve.
fn wrong_role(required: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_WRONG_ROLE", "required": required}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Runtime-dir + on-disk seams.
// ---------------------------------------------------------------------------

/// The live wfb stats sidecar (`<run dir>/wfb-stats.json`) the radio writes ~once
/// per second; the source of the `link` view.
fn wfb_stats_path(run_dir: &Path) -> PathBuf {
    run_dir.join(paths::WFB_STATS_JSON)
}

/// The mesh-state sidecar (`<run dir>/mesh-state.json`) the relay/receiver poll
/// loop writes; the sidecar fallback for the `/status` `mesh` block.
fn mesh_state_path(run_dir: &Path) -> PathBuf {
    run_dir.join(paths::MESH_STATE_JSON)
}

/// The uplink router's active-uplink sentinel (`<run dir>/uplink-active`): present
/// with the selected uplink while one is up, unlinked when there is none.
fn uplink_active_flag_path(run_dir: &Path) -> PathBuf {
    run_dir.join("uplink-active")
}

/// The unit that runs the uplink router and so owns the sentinel above.
const UPLINK_ROUTER_UNIT: &str = "ados-uplink-router.service";

/// The relay-state sidecar (`<run dir>/wfb-relay.json`) the relay loop writes;
/// the sidecar fallback for `/wfb/relay/status`.
fn wfb_relay_path(run_dir: &Path) -> PathBuf {
    run_dir.join(paths::WFB_RELAY_JSON)
}

/// The receiver-state sidecar (`<run dir>/wfb-receiver.json`) the receiver loop
/// writes; the sidecar fallback for the two `/wfb/receiver/*` routes.
fn wfb_receiver_path(run_dir: &Path) -> PathBuf {
    run_dir.join(paths::WFB_RECEIVER_JSON)
}

/// How fresh a ground-station snapshot must be to be served as a live reading:
/// the registry window the relay, receiver, mesh, failover and wfb-stats
/// sidecars share (10 s).
///
/// The producing loops (relay, receiver, mesh, failover) write at roughly 1 Hz,
/// so 10 s is ten missed writes — comfortably past jitter, well inside an
/// operator's reaction window.
///
/// ONE threshold for every snapshot surface in this module, including the `link`
/// block in [`link_view_from`] which carried the only such ceiling before (as its
/// own literal). Sibling surfaces with independently-maintained staleness
/// thresholds drift apart, and then two blocks of the same `/status` response
/// disagree about whether the node is stale.
pub(crate) const SNAPSHOT_FRESH_S: f64 = paths::WFB_RELAY_MAX_AGE_MS as f64 / 1000.0;

/// Read a JSON object sidecar, returning the empty map on absence / a read error / a parse error /
/// a non-object body.
///
/// Carries NO freshness judgement — use [`read_fresh_json`] for anything a client
/// reads as a current measurement.
fn read_json_or_empty(path: &Path) -> Map<String, Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        },
        Err(_) => Map::new(),
    }
}

/// Read a JSON object sidecar only when its mtime is within
/// [`SNAPSHOT_FRESH_S`] of `now`; `None` when the file is absent, unreadable,
/// not an object, empty, older than that, or future-dated.
///
/// mtime is the right clock here: every one of these sidecars is rewritten whole
/// on each poll tick, so its mtime IS the age of the reading inside it. A
/// service that died leaves its last file on a tmpfs that survives until reboot,
/// so without this gate the sidecar fallback re-served that file's `up: true`
/// forever. A future mtime (a clock that stepped backwards) makes the age
/// unprovable and reads as stale.
///
/// `now` is threaded in (the same shape `ados_hid::pic_view::read_pic_view`
/// uses) so a test drives the aged case by advancing the clock instead of
/// back-dating a file.
fn read_fresh_json(path: &Path, now: SystemTime) -> Option<Map<String, Value>> {
    let window = Some(Duration::from_secs_f64(SNAPSHOT_FRESH_S));
    read_fresh_at::<Map<String, Value>>(path, window, now)
        .fresh()
        .filter(|map| !map.is_empty())
}

/// A file's mtime age in seconds relative to `now`, or `None` when the file is
/// absent, its mtime is unreadable, or its mtime is AFTER `now` (an unprovable
/// age, which must not read as a fresh measurement).
fn file_age_s(path: &Path, now: SystemTime) -> Option<f64> {
    file_age(path, now).map(|d| d.as_secs_f64())
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/status — the OLED-aligned composite snapshot.
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/status` → the full ground-station snapshot.
///
/// Composes the OLED-aligned blocks the GS UI + the GCS Hardware tab poll at 1 Hz.
/// Each leg is fault-tolerant and degrades to the FastAPI fallback shape rather
/// than failing: the pair read defaults to no peer, the link view to the
/// disconnected base, the AP view to the not-running shape (resolved SSID,
/// `ap_ip` null) when the hostapd unit is down, the system snapshot to the zero
/// default, the recorder to inactive, and the mesh block to `{}`. Guaranteed 200
/// on a ground-station node, `404` on a drone.
pub async fn get_status(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };

    // Paired-drone identity. `paired_drone_id` is surfaced only when the pair read
    // reports paired; `key_fingerprint` is always the pair read's fingerprint
    // (null when unpaired or the key is unreadable).
    let (paired_drone_id, key_fingerprint) = pair_identity(&state);

    // Role block. `current` reads the live `/etc/ados/mesh/role` sentinel (the
    // role resolved by the gate above), `configured` reads the config value, and
    // `mesh_capable` is detected live from the attached radios. They diverge
    // briefly during a role transition; clients that drive state decisions
    // prefer `current`.
    let configured_role = ground_station_config_role(&state.pairing_paths.config);
    let mesh_capable = tokio::task::spawn_blocking(ados_protocol::netif::mesh_capable)
        .await
        .unwrap_or(false);
    let role_block = json!({
        "current": role,
        "configured": configured_role,
        "supported": ["direct", "relay", "receiver"],
        "mesh_capable": mesh_capable,
    });

    // Mesh block. Populated only for a relay/receiver node with an active mesh,
    // store-first off the `mesh.state` event, sidecar-fallback off
    // `mesh-state.json`, both age-gated. A direct node gets `{}` so the OLED +
    // GCS feature-detect without a round-trip; a relay/receiver with nothing
    // current gets the nulled `stale: true` block.
    let mesh_block: Value = if role == "relay" || role == "receiver" {
        match latest_status_mesh_block(&state).await {
            Some(stored) => stored,
            None => mesh_block_from_sidecar(&mesh_state_path(&state.run_dir), SystemTime::now()),
        }
    } else {
        json!({})
    };

    // Recorder state, read off the process-wide recorder this same binary owns
    // (`gs_recording::recorder`), through the derivation `gs_recording` owns so
    // this surface and the `/recording/list` envelope cannot drift. Both used to
    // hardcode `false` on the stated grounds that "the native front has no
    // in-process recorder (recording runs in a sibling service)" — which
    // `gs_recording`'s own module docs contradict: the start and stop are separate
    // requests, so the front holds the one recorder behind a `OnceLock` for the
    // life of the process. The surface therefore reported a known-false value
    // while a capture was running.
    let (recording_active, recording_filename) =
        crate::routes::gs_recording::recording_view(&crate::routes::gs_recording::recorder()).await;

    let body = json!({
        // The canonical hyphen form — the spelling `profile::current_profile_and_role_at`
        // resolves, the relay-proxy profile gate compares against
        // (`gs_relay_proxy::is_ground_station`) and the plugin target-profile
        // vocabulary uses. This route was the one producer spelling it
        // `ground_station`, so the first consumer to compare a value read here
        // against that vocabulary would silently never match, which is how a
        // plugin gets filtered off every ground station. (The setup wizard's
        // `/api/v1/setup/profile` keeps `ground_station`: that is a different
        // enum — an operator's install-time choice, not the runtime profile.)
        "profile": "ground-station",
        "paired_drone": {
            "device_id": paired_drone_id,
            "key_fingerprint": key_fingerprint,
            "fc_mode": Value::Null,
            "battery_pct": Value::Null,
            "gps_sats": Value::Null,
        },
        "link": link_view(&state.run_dir),
        "gcs": {"clients": [], "pic_id": Value::Null},
        "network": network_view(&state).await,
        "system": system_snapshot(&state).await,
        "recording": recording_active,
        "video": {
            "recording": recording_active,
            "recording_filename": recording_filename,
        },
        "role": role_block,
        "mesh": mesh_block,
        // Relay-proxy lane health. Null on a node that never built a proxy
        // (no aux egress), which is itself the answer an operator needs.
        "relay_proxy": match &state.aux_rpc_proxy {
            Some(p) => serde_json::to_value(p.stats()).unwrap_or(Value::Null),
            None => Value::Null,
        },
    });

    Json(body).into_response()
}

/// The `(paired_drone_id, key_fingerprint)` pair the `/status` route surfaces.
///
/// Reads the GS pair state off the `rx.key` file the same way the WFB pair read
/// does: paired := the file exists AND is exactly 64 bytes AND its fingerprint is
/// readable. `key_fingerprint` is the file's blake2b-8 public-key fingerprint (or
/// `null`); `device_id` is the config peer, surfaced ONLY when paired (matching the
/// Python `if pair_status.get("paired"): paired_drone_id = ...`). Either read
/// failing degrades to `(null, null)`.
fn pair_identity(state: &AppState) -> (Value, Value) {
    let key_path = state.pairing_paths.wfb_key_dir.join("rx.key");

    // paired := present AND exactly 64 bytes; a readable fingerprint is then
    // required (a 64-byte file with an unreadable fingerprint reverts paired to
    // false), mirroring the pair manager's `except: paired = False`.
    let mut paired = std::fs::metadata(&key_path)
        .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES as u64)
        .unwrap_or(false);
    let mut fingerprint = Value::Null;
    if paired {
        match read_public_fingerprint(&key_path) {
            Some(fp) => fingerprint = json!(fp),
            None => paired = false,
        }
    }

    // The config peer (`video.wfb.paired_with_device_id`, with the legacy
    // `ground_station.paired_drone_id` fallback), surfaced only when paired.
    let device_id = if paired {
        config_gs_peer(&state.pairing_paths.config)
    } else {
        Value::Null
    };

    (device_id, fingerprint)
}

/// The GS peer device id from the config, preferring `video.wfb.paired_with_device_id`
/// and falling back to the legacy `ground_station.paired_drone_id`. A non-string /
/// absent value reads as `null`.
fn config_gs_peer(config_path: &Path) -> Value {
    let raw = crate::config::load_config_object(config_path);
    let peer = raw
        .get("video")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("wfb"))
        .filter(|v| v.is_object())
        .and_then(|w| w.get("paired_with_device_id"))
        .filter(|v| v.is_string())
        .cloned();
    if let Some(p) = peer {
        return p;
    }
    raw.get("ground_station")
        .filter(|v| v.is_object())
        .and_then(|g| g.get("paired_drone_id"))
        .filter(|v| v.is_string())
        .cloned()
        .unwrap_or(Value::Null)
}

/// The configured ground-station role (`ground_station.role`), defaulting to
/// `"direct"` when the section / field is absent or non-string. Mirrors the Python
/// `getattr(app.config.ground_station, "role", "direct")`.
fn ground_station_config_role(config_path: &Path) -> String {
    crate::config::load_config_object(config_path)
        .get("ground_station")
        .filter(|v| v.is_object())
        .and_then(|g| g.get("role"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "direct".to_string())
}

// ---------------------------------------------------------------------------
// logd query seam: HTTP-over-UDS reads of the store's /v1 events API.
// ---------------------------------------------------------------------------

/// The newest event's non-empty `detail` map for `event_kind`, or `None` when the
/// store is unreachable / holds no such event / the detail is absent / non-object
/// / empty — **or when the row is older than [`SNAPSHOT_FRESH_S`]**.
///
/// The age gate is the load-bearing part. These rows are written by poll loops
/// (relay, receiver, mesh) at roughly 1 Hz; the store keeps them for days. Every
/// route below served the newest row with no age bound, so once the producing
/// loop or its whole service died the last row it ever wrote kept being served as
/// the current state: `up: true`, a peer count, a bitrate. The `link` view in
/// this same file has applied a 10 s ceiling all along, which is the pattern.
///
/// A rejected row is indistinguishable from no row here, on purpose: both mean
/// "nothing current", and the caller's stale branch says so explicitly rather
/// than falling through to a number.
async fn latest_event_detail(state: &AppState, event_kind: &str) -> Option<Map<String, Value>> {
    state
        .logd
        .fresh_event_detail(event_kind, Duration::from_secs_f64(SNAPSHOT_FRESH_S))
        .await
}

// ---------------------------------------------------------------------------
// Small shared helpers.
// ---------------------------------------------------------------------------

/// The exact 64-byte size a complete WFB-ng key file is.
const WFB_KEY_FILE_BYTES: usize = 64;

/// A numeric signal value, or `None` if absent / non-numeric. A JSON `bool` is not
/// a `Number`, so it is excluded naturally.
fn signal_num(signals: &Map<String, Value>, key: &str) -> Option<f64> {
    match signals.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// Coerce a JSON number value to `i64`, accepting an integer or a float. `None` for
/// a non-number.
fn json_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Python `bool(x)` truthiness over a JSON value: `null`/`false`/`0`/`0.0`/`""`/
/// `[]`/`{}` are falsey, everything else truthy.
fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Round to two decimal places, matching the Python `round(x, 2)` the
/// `bitrate_mbps` derivation in `_link_view` uses.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests;
