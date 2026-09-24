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
//! - **`GET /api/v1/ground-station/wfb`** — the stored radio config
//!   `{channel, bitrate_profile, fec}` from `video.wfb` (Python defaults
//!   `0`/`"default"`/`"8/12"` when unset).
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

use crate::state::AppState;
use crate::wfb_pair_state::read_public_fingerprint;

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

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets + sidecars resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The live wfb stats sidecar (`/run/ados/wfb-stats.json`) the radio writes ~once
/// per second; the source of the `link` view.
fn wfb_stats_path() -> PathBuf {
    run_dir().join("wfb-stats.json")
}

/// The mesh-state sidecar (`/run/ados/mesh-state.json`) the relay/receiver poll
/// loop writes; the sidecar fallback for the `/status` `mesh` block.
fn mesh_state_path() -> PathBuf {
    run_dir().join("mesh-state.json")
}

/// The uplink router's active-uplink sentinel (`/run/ados/uplink-active`): present
/// with the selected uplink while one is up, unlinked when there is none.
fn uplink_active_flag_path() -> PathBuf {
    run_dir().join("uplink-active")
}

/// The unit that runs the uplink router and so owns the sentinel above.
const UPLINK_ROUTER_UNIT: &str = "ados-uplink-router.service";

/// The relay-state sidecar (`/run/ados/wfb-relay.json`) the relay loop writes; the
/// sidecar fallback for `/wfb/relay/status`.
fn wfb_relay_path() -> PathBuf {
    run_dir().join("wfb-relay.json")
}

/// The Atlas aux-lane relay sidecar (`/run/ados/atlas-relay.json`) the relay loop
/// writes; the sidecar fallback for `/wfb/atlas-relay/status`.
fn atlas_relay_path() -> PathBuf {
    run_dir().join("atlas-relay.json")
}

/// The receiver-state sidecar (`/run/ados/wfb-receiver.json`) the receiver loop
/// writes; the sidecar fallback for the two `/wfb/receiver/*` routes.
fn wfb_receiver_path() -> PathBuf {
    run_dir().join("wfb-receiver.json")
}

/// How fresh a ground-station snapshot must be to be served as a live reading:
/// 10 seconds.
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
pub(crate) const SNAPSHOT_FRESH_S: f64 = 10.0;

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
/// not an object, empty, or older than that.
///
/// mtime is the right clock here: every one of these sidecars is rewritten whole
/// on each poll tick, so its mtime IS the age of the reading inside it. A
/// service that died leaves its last file on a tmpfs that survives until reboot,
/// so without this gate the sidecar fallback re-served that file's `up: true`
/// forever.
///
/// `now` is threaded in (the same shape `ados_hid::pic_view::read_pic_view`
/// uses) so a test drives the aged case by advancing the clock instead of
/// back-dating a file.
fn read_fresh_json(path: &Path, now: SystemTime) -> Option<Map<String, Value>> {
    let age_s = file_age_s(path, now)?;
    if age_s > SNAPSHOT_FRESH_S {
        return None;
    }
    match serde_json::from_str::<Value>(&std::fs::read_to_string(path).ok()?) {
        Ok(Value::Object(map)) if !map.is_empty() => Some(map),
        _ => None,
    }
}

/// A file's mtime age in seconds relative to `now`, or `None` when the file is
/// absent, its mtime is unreadable, or its mtime is AFTER `now`.
///
/// Fails closed on a future mtime rather than treating it as age zero: a clock
/// that stepped backwards (an RTC-less SBC correcting after boot) makes the age
/// unprovable, and an unprovable age must not read as a fresh measurement. Same
/// rule `pic_view` applies to the PIC sidecar.
fn file_age_s(path: &Path, now: SystemTime) -> Option<f64> {
    crate::freshness::file_age(path, now).map(|d| d.as_secs_f64())
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
    // (null when unpaired or the key is unreadable). Mirrors the Python
    // `_pair_manager().status("gs")` read on the GS profile.
    let (paired_drone_id, key_fingerprint) = pair_identity(&state);

    // Role block. `current` reads the live `/etc/ados/mesh/role` sentinel (the
    // role resolved by the gate above), `configured` reads the config value, and
    // `mesh_capable` reads the `profile.conf` flag. They diverge briefly during a
    // role transition; clients that drive state decisions prefer `current`.
    let configured_role = ground_station_config_role(&state.pairing_paths.config);
    let mesh_capable = profile_conf_mesh_capable(&ados_config::profile_conf_path());
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
            None => mesh_block_from_sidecar(&mesh_state_path(), SystemTime::now()),
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
        "link": link_view(),
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
/// absent value reads as `null`. Mirrors the Python `pair_manager.status("gs")`
/// peer resolution.
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

/// The `mesh_capable` flag from `profile.conf`, defaulting to `false` when the file
/// / field is absent or falsey. Mirrors the Python
/// `bool(profile_conf.get("mesh_capable", False))`.
fn profile_conf_mesh_capable(path: &Path) -> bool {
    // `profile.conf` is YAML; read the whole doc and project the one flag.
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let doc: Value = match serde_norway::from_str(&text) {
        Ok(v) => v,
        Err(_) => return false,
    };
    doc.get("mesh_capable").map(json_truthy).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The `link` sub-block (live radio link, off wfb-stats.json).
// ---------------------------------------------------------------------------

/// The reduced live radio link view the `/status` route carries, sourced from
/// `/run/ados/wfb-stats.json`.
///
/// The ground-station config object has no root `wfb` section (the radio config lives
/// at `video.wfb`), so the Python `getattr(app.config, "wfb", None)` is `None` and the
/// config-channel / tx-power seeds are `None` — this view starts from the same zero
/// base and merges the live snapshot over it. An absent / unparseable / non-object file
/// degrades to the base; a snapshot older than 10 s flips `state` to `"stale"`.
fn link_view() -> Value {
    link_view_from(&wfb_stats_path())
}

/// The path-injectable core of [`link_view`], reading the live snapshot from an
/// explicit `wfb-stats.json` path. Split out so tests drive it with their own file
/// without mutating the process-global `ADOS_RUN_DIR`.
fn link_view_from(path: &Path) -> Value {
    // The base block (what the dashboard renders before the radio writes its first
    // snapshot). config_channel + tx_power are `None` because the GS config carries
    // no root `wfb` section.
    let mut base = Map::new();
    base.insert("rssi_dbm".to_string(), Value::Null);
    base.insert("bitrate_mbps".to_string(), Value::Null);
    base.insert("bitrate_kbps".to_string(), Value::Null);
    base.insert("fec_recovered".to_string(), json!(0));
    base.insert("fec_lost".to_string(), json!(0));
    base.insert("fec_failed".to_string(), json!(0));
    base.insert("channel".to_string(), Value::Null);
    base.insert("snr_db".to_string(), Value::Null);
    base.insert("noise_dbm".to_string(), Value::Null);
    base.insert("packets_received".to_string(), json!(0));
    base.insert("packets_lost".to_string(), json!(0));
    base.insert("loss_percent".to_string(), Value::Null);
    base.insert("tx_power_dbm".to_string(), Value::Null);
    // The radio's declared power path (`host_vbus` / `powered_hub` /
    // `external_5v`), which the panel's brownout warning keys on. Null until the
    // radio writes a snapshot, so no reader guesses a supply topology.
    base.insert("topology".to_string(), Value::Null);
    base.insert("state".to_string(), json!("connecting"));
    // The one-glance link diagnosis (deaf / mis_keyed / jammed / healthy /
    // searching) + the RX counters that separate the failure modes a bare "0
    // received" hides, so the always-on cockpit link bar reads a legible CAUSE.
    base.insert("link_diag".to_string(), Value::Null);
    base.insert("packets_all".to_string(), json!(0));
    base.insert("decrypt_errors".to_string(), json!(0));

    // Live modulation rung + the adaptive ladder's configured cap. `null` until
    // the radio writes its first snapshot — never a confident 0, which would
    // read as MCS 0 (the slowest rung) rather than "not measured yet". The
    // Mission Control Radio settings page renders these beside `snr_db` as the
    // read-only "auto (MCS N at X dB)" row: the ACTIVE rung, not the commanded
    // one, and the cap so a policy-limited rung is not misread as a bad link.
    base.insert("mcs_index".to_string(), Value::Null);
    base.insert("mcs_ladder_cap".to_string(), Value::Null);

    // A future mtime is an unprovable age, not age zero: same rule as
    // [`file_age_s`].
    let Some(age_s) = file_age_s(path, SystemTime::now()) else {
        return Value::Object(base);
    };
    let payload = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            // A well-formed-but-non-object body returns the base, matching the
            // Python `if not isinstance(payload, dict): return base`.
            Ok(_) => return Value::Object(base),
            Err(_) => return Value::Object(base),
        },
        Err(_) => return Value::Object(base),
    };

    // Best-effort schema-drift signal (never reject): warn when the sidecar was
    // written by an agent with a different schema version, then read anyway. The
    // writer const lives in the radio crate, so compare against the shared registry.
    let got = payload.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("wfb-stats") {
        ados_protocol::sidecar::check_sidecar_version("wfb-stats", got, ours);
    }

    // Live snapshot wins. `bitrate_mbps` is derived from `bitrate_kbps`;
    // `fec_lost`/`fec_failed` both mirror the payload `fec_failed`; the channel
    // from the file wins over the (null) config. Mirrors the Python merge exactly.
    let rssi = payload.get("rssi_dbm").filter(|v| v.is_number()).cloned();
    let bitrate_kbps = payload.get("bitrate_kbps").and_then(Value::as_f64);
    let bitrate_mbps = bitrate_kbps.map(|k| round2(k / 1000.0));
    let fec_failed = payload.get("fec_failed").and_then(json_to_i64).unwrap_or(0);

    let mut merged = base;
    merged.insert("rssi_dbm".to_string(), rssi.unwrap_or(Value::Null));
    merged.insert(
        "bitrate_mbps".to_string(),
        bitrate_mbps.map(Value::from).unwrap_or(Value::Null),
    );
    merged.insert(
        "bitrate_kbps".to_string(),
        match bitrate_kbps {
            Some(k) => json!(k as i64),
            None => Value::Null,
        },
    );
    merged.insert(
        "fec_recovered".to_string(),
        json!(payload
            .get("fec_recovered")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    merged.insert("fec_lost".to_string(), json!(fec_failed));
    merged.insert("fec_failed".to_string(), json!(fec_failed));
    // Channel: the payload value when truthy, else the (null) config channel.
    let payload_channel = payload.get("channel").cloned().unwrap_or(Value::Null);
    merged.insert(
        "channel".to_string(),
        if json_truthy(&payload_channel) {
            payload_channel
        } else {
            Value::Null
        },
    );
    merged.insert(
        "snr_db".to_string(),
        payload.get("snr_db").cloned().unwrap_or(Value::Null),
    );
    merged.insert(
        "noise_dbm".to_string(),
        payload.get("noise_dbm").cloned().unwrap_or(Value::Null),
    );
    merged.insert(
        "packets_received".to_string(),
        json!(payload
            .get("packets_received")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    merged.insert(
        "packets_lost".to_string(),
        json!(payload
            .get("packets_lost")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    merged.insert(
        "loss_percent".to_string(),
        payload.get("loss_percent").cloned().unwrap_or(Value::Null),
    );
    // Diagnostic trio from the sidecar: the verdict (a string, null when the
    // writer has not classified yet) + the RX counters (0 when absent).
    merged.insert(
        "link_diag".to_string(),
        payload
            .get("link_diag")
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or(Value::Null),
    );
    merged.insert(
        "packets_all".to_string(),
        json!(payload
            .get("packets_all")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    merged.insert(
        "decrypt_errors".to_string(),
        json!(payload
            .get("decrypt_errors")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    // Modulation: pass the sidecar's numbers through, `null` when absent.
    merged.insert(
        "mcs_index".to_string(),
        payload
            .get("mcs_index")
            .filter(|v| v.is_number())
            .cloned()
            .unwrap_or(Value::Null),
    );
    merged.insert(
        "mcs_ladder_cap".to_string(),
        payload
            .get("mcs_ladder_cap")
            .filter(|v| v.is_number())
            .cloned()
            .unwrap_or(Value::Null),
    );
    // tx_power_dbm: the payload value when present (not null), else the (null) base.
    let payload_tx = payload.get("tx_power_dbm").cloned();
    merged.insert(
        "tx_power_dbm".to_string(),
        match payload_tx {
            Some(v) if !v.is_null() => v,
            _ => Value::Null,
        },
    );
    merged.insert(
        "topology".to_string(),
        payload
            .get("topology")
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            .cloned()
            .unwrap_or(Value::Null),
    );
    // state: the payload value when truthy, else "connecting".
    let payload_state = payload.get("state").cloned().unwrap_or(Value::Null);
    let state = if json_truthy(&payload_state) {
        payload_state
    } else {
        json!("connecting")
    };
    merged.insert("state".to_string(), state);

    // Over the shared freshness ceiling the snapshot is suspect; flip to "stale".
    // This used to be its own `10.0` literal. Two independently-maintained
    // thresholds on sibling surfaces drift, and then two blocks on the same page
    // disagree about whether the same node is stale.
    if age_s > SNAPSHOT_FRESH_S {
        merged.insert("state".to_string(), json!("stale"));
    }
    Value::Object(merged)
}

// ---------------------------------------------------------------------------
// The `network` sub-block (AP-only view).
// ---------------------------------------------------------------------------

/// The hostapd systemd unit the live AP runs as (`_HOSTAPD_UNIT`), the
/// `systemctl is-active` source for the AP `running` state.
const HOSTAPD_UNIT: &str = "ados-hostapd.service";

/// The AP gateway address the hostapd manager assigns (`_AP_ADDR`), reported as
/// `ap_ip` while the AP is running.
const AP_GATEWAY_IP: &str = "192.168.4.1";

/// The AP-only network view the `/status` route carries, reproducing the Python
/// `_network_view` over the live hostapd state.
///
/// The Python `_network_view` reads the live hostapd manager's `status()`:
/// `ap_ssid` from `status()["ssid"]` (the manager's resolved SSID, present
/// regardless of the unit state) and `ap_ip` from `status()["gateway"]`. The front
/// has no in-process manager but reads the same live seams: the resolved SSID off
/// config (the way `_hostapd_manager` + `_build_ssid` resolve it) and the
/// `192.168.4.1` gateway while the AP unit is active. `usb_ip` stays the static
/// null leg. `uplink_type` / `uplink_reachable` come from the uplink router's
/// active-uplink sentinel (see [`uplink_view`]). When the AP unit is down, `ap_ip`
/// is null (the manager's status reports the gateway only while up), while
/// `ap_ssid` still resolves off config.
///
/// This is the I/O shell, so it is the frame that goes `async`: the composition
/// stays pure in [`network_view_compose`], which already takes the running flag
/// injected, so there is nothing to hoist out of here and no reason to grow
/// `get_status` a parameter.
async fn network_view(state: &AppState) -> Value {
    let cfg = crate::config::load_config_object(&state.pairing_paths.config);
    // The one definition of this liveness probe now lives in
    // `crate::probe::unit_is_active`. It used to be a byte-identical local
    // `hostapd_running()` here AND another in `routes::gs_network`, which is
    // exactly how two surfaces come to give an operator two different answers
    // about the same unit; keep it centralised so it cannot drift again.
    let running = crate::probe::unit_is_active(HOSTAPD_UNIT).await;
    let router_running = crate::probe::unit_is_active(UPLINK_ROUTER_UNIT).await;
    let uplink = uplink_view(&uplink_active_flag_path(), router_running);
    network_view_compose(&ap_ssid_from_config(&cfg), running, uplink)
}

/// `(uplink_type, uplink_reachable)` from the uplink router's sentinel.
///
/// A present sentinel names the selected uplink, mapped to its kind (`eth`,
/// `wifi`, `cellular`, `usb`, else the interface name), with the router's last
/// reachability verdict. An absent sentinel while the router runs is the
/// router's own "no uplink" (`"none"`, unreachable). Anything else (the router
/// not running, an unreadable or malformed file) is unknown: both null, which a
/// reader renders as "—" rather than as offline.
fn uplink_view(flag: &Path, router_running: bool) -> (Value, Value) {
    #[derive(Deserialize)]
    struct Flag {
        active_uplink: String,
        internet_reachable: bool,
    }
    match std::fs::read_to_string(flag) {
        Ok(text) => match serde_json::from_str::<Flag>(&text) {
            Ok(f) => {
                let name = f.active_uplink.trim();
                let kind = if name.starts_with("eth") {
                    "eth"
                } else if name.starts_with("wlan") {
                    "wifi"
                } else if name.starts_with("wwan") {
                    "cellular"
                } else if name.starts_with("usb") {
                    "usb"
                } else {
                    name
                };
                (json!(kind), json!(f.internet_reachable))
            }
            Err(_) => (Value::Null, Value::Null),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && router_running => {
            (json!("none"), json!(false))
        }
        Err(_) => (Value::Null, Value::Null),
    }
}

/// Compose the `_network_view` body from the resolved SSID, the live running
/// flag and the uplink verdict. Split out so the shape + the running-vs-not-running
/// gating are unit tested without the `systemctl` IO. `ap_ip` is the gateway while
/// running, else null; `ap_ssid` is the resolved SSID either way.
fn network_view_compose(ap_ssid: &str, running: bool, uplink: (Value, Value)) -> Value {
    let (uplink_type, uplink_reachable) = uplink;
    json!({
        "ap_ssid": ap_ssid,
        "ap_ip": if running { Value::String(AP_GATEWAY_IP.to_string()) } else { Value::Null },
        "usb_ip": Value::Null,
        "uplink_type": uplink_type,
        "uplink_reachable": uplink_reachable,
    })
}

/// The resolved AP SSID from config, the way `_hostapd_manager` + `_build_ssid`
/// resolve it: honour a configured `network.hotspot.ssid` only when it is
/// non-empty, carries no `{device_id}` placeholder, and already starts with
/// `ADOS-GS-`; otherwise build `ADOS-GS-<first 4 hex of device_id, uppercased,
/// zero-padded>`.
fn ap_ssid_from_config(cfg: &Value) -> String {
    let configured = cfg
        .get("network")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("hotspot"))
        .filter(|v| v.is_object())
        .and_then(|h| h.get("ssid"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let device_id = cfg
        .get("agent")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("device_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    resolve_ap_ssid(configured, device_id)
}

/// Resolve the AP SSID exactly as the live hostapd manager does (the
/// `_hostapd_manager` `ssid_override` gate + `_build_ssid`): a configured SSID is
/// honoured only when it is non-empty, has no `{device_id}` placeholder, and
/// already starts with `ADOS-GS-`; otherwise build `ADOS-GS-<short_id>`.
fn resolve_ap_ssid(configured: &str, device_id: &str) -> String {
    if !configured.is_empty()
        && !configured.contains("{device_id}")
        && configured.starts_with("ADOS-GS-")
    {
        return configured.to_string();
    }
    format!("ADOS-GS-{}", short_id(device_id))
}

/// The first 4 hex chars of the device id, uppercased, zero-padded to 4 when the
/// id has fewer than 4 hex chars after stripping non-hex characters. Mirrors the
/// Python `_short_id`.
fn short_id(device_id: &str) -> String {
    let hex_only: String = device_id.chars().filter(char::is_ascii_hexdigit).collect();
    let padded = if hex_only.len() >= 4 {
        hex_only
    } else {
        format!("{hex_only}0000")
    };
    padded.chars().take(4).collect::<String>().to_uppercase()
}

// ---------------------------------------------------------------------------
// The `system` sub-block (CPU/RAM/temp/uptime/version).
// ---------------------------------------------------------------------------

/// The system snapshot the `/status` route carries: `{cpu_pct, ram_used_mb,
/// ram_total_mb, temp_c, uptime_seconds, agent_version}`.
///
/// CPU / RAM / temperature come from the most-recent hardware snapshots in the
/// logging store (the continuous collector samples them) with a direct host read
/// (`crate::hw_local`) behind them, `uptime_seconds` from `/proc/uptime`, and
/// `agent_version` from the resolved app version.
///
/// A leg neither source supplies is `null`. It used to be `0.0` / `0` — and the
/// durable store ships OFF, so on a stock ground station every one of these read
/// zero: the OLED and the GCS Hardware tab rendered an idle CPU and a 0 MB RAM
/// total as measurements. Same reasoning as `status::derive_health`; the keys
/// stay present so a client cannot mistake an unreported leg for an absent one.
async fn system_snapshot(state: &AppState) -> Value {
    let signals = match state.logd.latest_hw_signals().await {
        Some(s) => Some(s),
        None => {
            let local = crate::hw_local::collect_signals();
            (!local.is_empty()).then_some(local)
        }
    };
    system_block(
        signals.as_ref(),
        proc_uptime_seconds(),
        &state.agent_version(),
    )
}

/// Compose the `system` block from an optional signal map. Pure, so the
/// unreported-leg behaviour is asserted directly instead of through a
/// re-implementation of it.
fn system_block(signals: Option<&Map<String, Value>>, uptime_s: i64, version: &str) -> Value {
    let cpu_pct = signals
        .and_then(|s| signal_num(s, "cpu.util.all"))
        .map(Value::from)
        .unwrap_or(Value::Null);
    let (ram_used_mb, ram_total_mb) = match signals.and_then(ram_mb) {
        Some((used, total)) => (Value::from(used), Value::from(total)),
        None => (Value::Null, Value::Null),
    };
    let temp_c = signals
        .and_then(|s| signal_num(s, "thermal.primary_c"))
        .map(Value::from)
        .unwrap_or(Value::Null);
    let disk_pct = signals
        .and_then(root_disk_pct)
        .map(Value::from)
        .unwrap_or(Value::Null);

    json!({
        "cpu_pct": cpu_pct,
        "ram_used_mb": ram_used_mb,
        "ram_total_mb": ram_total_mb,
        "temp_c": temp_c,
        "disk_pct": disk_pct,
        "uptime_seconds": uptime_s,
        "agent_version": version,
    })
}

/// Root-filesystem usage in percent (one decimal) from the used + total byte
/// signals. `None` when either signal is absent or the total is not positive.
fn root_disk_pct(signals: &Map<String, Value>) -> Option<f64> {
    let total = signal_num(signals, "disk.fs_total_bytes")?;
    let used = signal_num(signals, "disk.fs_used_bytes")?;
    (total > 0.0).then(|| (used.max(0.0) / total * 1000.0).round() / 10.0)
}

/// Used + total RAM in MiB from the total + available byte signals, mirroring the
/// Python `_system_snapshot` arithmetic (`(total - available) / 1MiB` used,
/// `total / 1MiB` total). `None` when either byte signal is absent.
fn ram_mb(signals: &Map<String, Value>) -> Option<(i64, i64)> {
    let total = signal_num(signals, "mem.total_bytes")?;
    let avail = signal_num(signals, "mem.avail_bytes")?;
    let used_bytes = (total - avail).max(0.0);
    let mib = 1024.0 * 1024.0;
    Some(((used_bytes / mib) as i64, (total / mib) as i64))
}

/// System uptime in whole seconds from `/proc/uptime`, the kernel's own counter.
/// Mirrors the Python `int(time.time() - psutil.boot_time())` — `/proc/uptime`'s
/// first field is exactly that delta. Degrades to `0` when the file is absent
/// (a non-Linux dev host) or unparseable, matching the psutil-failure default.
fn proc_uptime_seconds() -> i64 {
    match std::fs::read_to_string("/proc/uptime") {
        Ok(text) => text
            .split_whitespace()
            .next()
            .and_then(|f| f.parse::<f64>().ok())
            .map(|s| s as i64)
            .unwrap_or(0),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// The `mesh` sub-block + the relay/receiver routes (store-first, sidecar-fallback).
// ---------------------------------------------------------------------------

/// The `/status` `mesh` sub-block from the store's `mesh.state` event, projecting
/// the five fields the live route reads off `mesh-state.json` (`up`, `peer_count`
/// = neighbor count, `selected_gateway`, `partition`, `mesh_id`). `None` when the
/// store is unreachable, holds no such event, or holds only a stale one, so the
/// caller falls back to the sidecar.
async fn latest_status_mesh_block(state: &AppState) -> Option<Value> {
    let detail = latest_event_detail(state, "mesh.state").await?;
    Some(mesh_block_from_snapshot(&detail))
}

/// Project the `/status` `mesh` block from a FRESH mesh snapshot body (the stored
/// event detail or the sidecar). Mirrors the Python `bool(...)` / `len(...)`
/// coercions exactly, and carries `stale: false` so a client always reads the
/// verdict off the same key instead of inferring it from a key's absence.
fn mesh_block_from_snapshot(snap: &Map<String, Value>) -> Value {
    let peer_count = snap
        .get("neighbors")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    json!({
        "up": snap.get("up").map(json_truthy).unwrap_or(false),
        "peer_count": peer_count,
        "selected_gateway": snap.get("selected_gateway").cloned().unwrap_or(Value::Null),
        "partition": snap.get("partition").map(json_truthy).unwrap_or(false),
        "mesh_id": snap.get("mesh_id").cloned().unwrap_or(Value::Null),
        "stale": false,
    })
}

/// The `mesh` block when no current snapshot exists: the same five keys, every
/// value `null`, `stale: true`.
///
/// The keys stay so a consumer reading `up` gets `null` (unknown) rather than
/// finding the key gone and coercing it — and `up: false` is NOT used, because
/// "the mesh is down" is a measurement this node cannot make when nothing is
/// reporting.
fn mesh_block_stale() -> Value {
    json!({
        "up": Value::Null,
        "peer_count": Value::Null,
        "selected_gateway": Value::Null,
        "partition": Value::Null,
        "mesh_id": Value::Null,
        "stale": true,
    })
}

/// The `/status` `mesh` block from the `mesh-state.json` sidecar, projecting the
/// same five fields. An absent / unparseable / non-object / **stale** file yields
/// the `stale: true` block: the mesh poll loop rewrites this file each tick, so a
/// file older than the freshness window is the last thing a dead loop wrote, and
/// it used to be served verbatim as the current mesh state.
fn mesh_block_from_sidecar(path: &Path, now: SystemTime) -> Value {
    let Some(snap) = read_fresh_json(path, now) else {
        return mesh_block_stale();
    };
    // Best-effort schema-drift signal (never reject): warn when the mesh-state
    // sidecar was written by an agent with a different schema version, then read
    // anyway. The writer const lives in the groundlink crate, so compare against
    // the shared registry.
    let got = snap.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("mesh-state") {
        ados_protocol::sidecar::check_sidecar_version("mesh-state", got, ours);
    }
    mesh_block_from_snapshot(&snap)
}

/// A snapshot body served as live, stamped `stale: false`.
fn fresh_snapshot_body(mut detail: Map<String, Value>) -> Value {
    detail.insert("stale".to_string(), Value::Bool(false));
    Value::Object(detail)
}

/// The reply for a relay/receiver counter route with no current snapshot: every
/// key the last known snapshot carried, nulled, plus `stale: true`.
///
/// Nulling the stale snapshot's own keys rather than emitting a bare
/// `{"stale": true}` keeps the key set a client already reads intact — a key that
/// vanishes reads as "not reported" and gets coerced to 0 or false downstream,
/// which is the same failure in a different costume. With no snapshot at all the
/// key set is genuinely unknown, so the body is just the marker.
fn stale_snapshot_body(last_known: Map<String, Value>) -> Value {
    let mut out: Map<String, Value> = last_known
        .into_iter()
        .map(|(k, _v)| (k, Value::Null))
        .collect();
    out.insert("stale".to_string(), Value::Bool(true));
    Value::Object(out)
}

/// `GET /api/v1/ground-station/wfb/relay/status` → relay-side fragment counters.
///
/// `404` `E_WRONG_ROLE` off a relay node. On a relay, reads the store's
/// most-recent `gs.relay_state` event (the relay loop ships the same body it
/// writes to the sidecar), falling back to the `/run/ados/wfb-relay.json`
/// sidecar. Both reads are age-gated to [`SNAPSHOT_FRESH_S`]; with neither
/// current the body is the nulled `stale: true` shape, never the counters a dead
/// relay loop last wrote.
pub async fn get_wfb_relay_status(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };
    if role != "relay" {
        return wrong_role("relay");
    }
    if let Some(detail) = latest_event_detail(&state, "gs.relay_state").await {
        return Json(fresh_snapshot_body(detail)).into_response();
    }
    let path = wfb_relay_path();
    match read_fresh_json(&path, SystemTime::now()) {
        Some(snap) => Json(fresh_snapshot_body(snap)).into_response(),
        None => Json(stale_snapshot_body(read_json_or_empty(&path))).into_response(),
    }
}

/// `GET /api/v1/ground-station/wfb/atlas-relay/status` → the Atlas aux-lane relay's
/// forward counters.
///
/// `404` `E_WRONG_ROLE` off a relay node. On a relay, reads the store's most-recent
/// `gs.atlas_relay` event (the relay loop ships the same body it writes to the
/// sidecar), falling back to the `/run/ados/atlas-relay.json` sidecar. The body is
/// the relay's `{up, datagrams_seen, forwarded, malformed, forward_failed,
/// compute_url, listen_port, generated_at_ms}` snapshot; both sources are
/// age-gated, and with neither current the keys are nulled under `stale: true`.
pub async fn get_atlas_relay_status(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };
    if role != "relay" {
        return wrong_role("relay");
    }
    if let Some(detail) = latest_event_detail(&state, "gs.atlas_relay").await {
        return Json(fresh_snapshot_body(detail)).into_response();
    }
    let path = atlas_relay_path();
    match read_fresh_json(&path, SystemTime::now()) {
        Some(snap) => Json(fresh_snapshot_body(snap)).into_response(),
        None => Json(stale_snapshot_body(read_json_or_empty(&path))).into_response(),
    }
}

/// `GET /api/v1/ground-station/wfb/receiver/relays` → per-relay fragment counters.
///
/// `404` `E_WRONG_ROLE` off a receiver node. On a receiver, reads the store's
/// most-recent `gs.receiver_state` event projected to `{relays}`, falling back to
/// the `/run/ados/wfb-receiver.json` sidecar. Both age-gated; with neither
/// current, `relays` is `null` under `stale: true` — NOT the empty list, which
/// reads as "looked, found no relays".
pub async fn get_wfb_receiver_relays(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };
    if role != "receiver" {
        return wrong_role("receiver");
    }
    if let Some(detail) = latest_event_detail(&state, "gs.receiver_state").await {
        return Json(slice_receiver_relays(&detail)).into_response();
    }
    match read_fresh_json(&wfb_receiver_path(), SystemTime::now()) {
        Some(snap) => Json(slice_receiver_relays(&snap)).into_response(),
        None => Json(json!({"relays": Value::Null, "stale": true})).into_response(),
    }
}

/// `GET /api/v1/ground-station/wfb/receiver/combined` → combined FEC output stats.
///
/// `404` `E_WRONG_ROLE` off a receiver node. On a receiver, reads the store's
/// `gs.receiver_state` event projected to `{fragments_after_dedup, fec_repaired,
/// output_kbps, up}`, falling back to the `/run/ados/wfb-receiver.json` sidecar
/// (same projection + per-key defaults). Both age-gated; with neither current
/// every counter is `null` under `stale: true`, because a zeroed counter and a
/// `up: false` are readings this node cannot make when nothing is reporting.
pub async fn get_wfb_receiver_combined(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };
    if role != "receiver" {
        return wrong_role("receiver");
    }
    if let Some(detail) = latest_event_detail(&state, "gs.receiver_state").await {
        return Json(slice_receiver_combined(&detail)).into_response();
    }
    match read_fresh_json(&wfb_receiver_path(), SystemTime::now()) {
        Some(snap) => Json(slice_receiver_combined(&snap)).into_response(),
        None => Json(receiver_combined_stale()).into_response(),
    }
}

/// Project the `/wfb/receiver/relays` shape from a FRESH receiver-state body:
/// `{relays}` (the key defaulting to the empty list when the snapshot itself
/// omits it — that IS a current reading of "no relays") plus `stale: false`.
fn slice_receiver_relays(detail: &Map<String, Value>) -> Value {
    json!({
        "relays": detail.get("relays").cloned().unwrap_or_else(|| json!([])),
        "stale": false,
    })
}

/// Project the `/wfb/receiver/combined` shape from a FRESH receiver-state body,
/// applying the same per-key defaults the live route applies so an omitted key
/// coalesces identically whether it is absent from the stored detail or the
/// sidecar.
fn slice_receiver_combined(detail: &Map<String, Value>) -> Value {
    json!({
        "fragments_after_dedup": detail.get("fragments_after_dedup").cloned().unwrap_or_else(|| json!(0)),
        "fec_repaired": detail.get("fec_repaired").cloned().unwrap_or_else(|| json!(0)),
        "output_kbps": detail.get("output_kbps").cloned().unwrap_or_else(|| json!(0)),
        "up": detail.get("up").cloned().unwrap_or(Value::Bool(false)),
        "stale": false,
    })
}

/// The `/wfb/receiver/combined` shape with nothing current: the same four keys,
/// nulled, under `stale: true`.
fn receiver_combined_stale() -> Value {
    json!({
        "fragments_after_dedup": Value::Null,
        "fec_repaired": Value::Null,
        "output_kbps": Value::Null,
        "up": Value::Null,
        "stale": true,
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/wfb — the stored radio config.
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/wfb` → the stored radio config `{channel,
/// bitrate_profile, fec}` from `video.wfb`, defaulting to `channel: 0`,
/// `bitrate_profile: "default"`, `fec: "8/12"` when the section or a field is
/// absent. `404` `E_PROFILE_MISMATCH` off a ground-station node.
pub async fn get_wfb(State(state): State<AppState>) -> Response {
    if ground_station_role(&state).is_none() {
        return profile_mismatch();
    }
    let cfg = WfbViewConfig::load_from(&state.pairing_paths.config);
    let wfb = &cfg.video.wfb;
    Json(json!({
        "channel": wfb.channel.unwrap_or(0),
        "bitrate_profile": wfb
            .bitrate_profile
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        "fec": wfb.fec.clone().unwrap_or_else(|| "8/12".to_string()),
    }))
    .into_response()
}

/// The `video.wfb` slice the `/wfb` view reads. Each field is optional so an absent
/// section reads the Python field default (`channel: 0`, `bitrate_profile:
/// "default"`, `fec: "8/12"`), applied at projection time above.
#[derive(Debug, Clone, Default, Deserialize)]
struct WfbViewSection {
    #[serde(default)]
    channel: Option<i64>,
    #[serde(default)]
    bitrate_profile: Option<String>,
    #[serde(default)]
    fec: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WfbViewVideo {
    #[serde(default)]
    wfb: WfbViewSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WfbViewConfig {
    #[serde(default)]
    video: WfbViewVideo,
}

impl WfbViewConfig {
    /// Load the `video.wfb` slice from the config path. A missing / unparseable
    /// file yields the all-defaults slice, so the route still answers a usable body.
    fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_norway::from_str(&text).unwrap_or_default(),
            Err(_) => WfbViewConfig::default(),
        }
    }
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
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn signals(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn profile_mismatch_body_is_the_fastapi_404_shape() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // The body shape is the contract; build it independently and compare.
        let want = json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}});
        assert_eq!(
            want,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    #[test]
    fn wrong_role_body_carries_the_required_role() {
        let want = json!({"detail": {"error": {"code": "E_WRONG_ROLE", "required": "relay"}}});
        assert_eq!(
            want,
            json!({"detail": {"error": {"code": "E_WRONG_ROLE", "required": "relay"}}})
        );
    }

    #[test]
    fn link_view_of_an_absent_sidecar_is_the_disconnected_base() {
        // With no wfb-stats.json the view is the zero base with state "connecting".
        // The GS config carries no root wfb, so channel + tx_power are null.
        let dir = tempfile::tempdir().unwrap();
        let view = link_view_from(&dir.path().join("wfb-stats.json"));
        let want = json!({
            "rssi_dbm": null,
            "bitrate_mbps": null,
            "bitrate_kbps": null,
            "fec_recovered": 0,
            "fec_lost": 0,
            "fec_failed": 0,
            "channel": null,
            "snr_db": null,
            "noise_dbm": null,
            "packets_received": 0,
            "packets_lost": 0,
            "loss_percent": null,
            "tx_power_dbm": null,
            "topology": null,
            "state": "connecting",
            "link_diag": null,
            "packets_all": 0,
            "decrypt_errors": 0,
            // Never a confident 0 before the radio has written a snapshot: MCS 0
            // is a real (slowest) rung, so 0 would be a false reading.
            "mcs_index": null,
            "mcs_ladder_cap": null,
        });
        assert_eq!(view, want);
    }

    #[test]
    fn link_view_merges_the_live_snapshot_over_the_base() {
        // A fresh wfb-stats.json with live values must merge over the base, derive
        // bitrate_mbps, mirror fec_failed into fec_lost, and keep state from the
        // payload. The file is fresh (< 10 s) so state is NOT flipped to stale.
        let dir = tempfile::tempdir().unwrap();
        let stats = dir.path().join("wfb-stats.json");
        let payload = json!({
            "rssi_dbm": -51,
            "bitrate_kbps": 5740,
            "fec_recovered": 3,
            "fec_failed": 1,
            "channel": 149,
            "snr_db": 28.0,
            "noise_dbm": -95.0,
            "packets_received": 598,
            "packets_lost": 2,
            "loss_percent": 0.3,
            "tx_power_dbm": 20,
            "state": "connected",
            "link_diag": "healthy",
            "packets_all": 640,
            "decrypt_errors": 0,
            "mcs_index": 3,
            "mcs_ladder_cap": 3,
            "topology": "powered_hub",
        });
        std::fs::write(&stats, serde_json::to_string(&payload).unwrap()).unwrap();
        let view = link_view_from(&stats);
        assert_eq!(view["rssi_dbm"], json!(-51));
        assert_eq!(view["bitrate_kbps"], json!(5740));
        assert_eq!(view["bitrate_mbps"], json!(5.74));
        assert_eq!(view["fec_recovered"], json!(3));
        assert_eq!(view["fec_lost"], json!(1));
        assert_eq!(view["fec_failed"], json!(1));
        assert_eq!(view["channel"], json!(149));
        assert_eq!(view["packets_received"], json!(598));
        assert_eq!(view["tx_power_dbm"], json!(20));
        assert_eq!(view["state"], json!("connected"));
        // The diagnostic trio pulls from the sidecar payload.
        assert_eq!(view["link_diag"], json!("healthy"));
        assert_eq!(view["packets_all"], json!(640));
        assert_eq!(view["decrypt_errors"], json!(0));
        // Modulation: the live rung and the ladder cap the Radio settings page
        // reads beside snr_db.
        assert_eq!(view["mcs_index"], json!(3));
        assert_eq!(view["mcs_ladder_cap"], json!(3));
        assert_eq!(view["snr_db"], json!(28.0));
        // The declared power path rides through, so the panel's brownout warning
        // reads the radio's own topology instead of assuming host VBUS.
        assert_eq!(view["topology"], json!("powered_hub"));
    }

    #[test]
    fn network_view_compose_running_carries_ssid_and_gateway() {
        // A running AP reports the resolved SSID + the 192.168.4.1 gateway,
        // beside the uplink verdict it was handed.
        let want = json!({
            "ap_ssid": "ADOS-GS-D9DB",
            "ap_ip": "192.168.4.1",
            "usb_ip": null,
            "uplink_type": "eth",
            "uplink_reachable": true,
        });
        assert_eq!(
            network_view_compose("ADOS-GS-D9DB", true, (json!("eth"), json!(true))),
            want
        );
    }

    #[test]
    fn network_view_compose_not_running_gates_the_gateway() {
        // A down AP keeps the resolved SSID but reports ap_ip null (the manager's
        // status reports the gateway only while up).
        let v = network_view_compose("ADOS-GS-0000", false, (Value::Null, Value::Null));
        assert_eq!(v["ap_ssid"], json!("ADOS-GS-0000"));
        assert_eq!(v["ap_ip"], Value::Null);
        assert_eq!(v["usb_ip"], Value::Null);
        assert_eq!(v["uplink_type"], Value::Null);
        assert_eq!(v["uplink_reachable"], Value::Null);
    }

    /// The uplink comes from the router's sentinel. Its absence is "no uplink"
    /// only while the router runs; otherwise nothing is known, and unknown must
    /// not read as offline.
    #[test]
    fn uplink_view_reads_the_router_sentinel_and_keeps_unknown_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        let write = |name: &str, reachable: bool| {
            let body = json!({
                "active_uplink": name,
                "internet_reachable": reachable,
                "timestamp_ms": 1,
                "data_cap_state": "ok",
            });
            std::fs::write(&flag, body.to_string()).unwrap();
        };
        for (name, kind) in [
            ("eth0", "eth"),
            ("wlan0_client", "wifi"),
            ("wwan0", "cellular"),
            ("usb0", "usb"),
        ] {
            write(name, true);
            assert_eq!(
                uplink_view(&flag, true),
                (json!(kind), json!(true)),
                "{name}"
            );
        }
        write("eth0", false);
        assert_eq!(uplink_view(&flag, false), (json!("eth"), json!(false)));

        std::fs::remove_file(&flag).unwrap();
        assert_eq!(uplink_view(&flag, true), (json!("none"), json!(false)));
        assert_eq!(uplink_view(&flag, false), (Value::Null, Value::Null));

        std::fs::write(&flag, "not json").unwrap();
        assert_eq!(uplink_view(&flag, true), (Value::Null, Value::Null));
    }

    #[test]
    fn ap_ssid_from_config_resolves_the_template_and_honours_an_explicit_name() {
        // The default hotspot SSID carries the `{device_id}` template, which
        // resolves to the built ADOS-GS-<short id> name off the device id.
        let cfg = json!({
            "agent": {"device_id": "d9dbcafe"},
            "network": {"hotspot": {"ssid": "ADOS-{device_id}"}},
        });
        assert_eq!(ap_ssid_from_config(&cfg), "ADOS-GS-D9DB");
        // An explicit ADOS-GS- name is honoured verbatim.
        let cfg2 = json!({"network": {"hotspot": {"ssid": "ADOS-GS-ABCD"}}});
        assert_eq!(ap_ssid_from_config(&cfg2), "ADOS-GS-ABCD");
        // No config at all → the zero-padded short id.
        assert_eq!(ap_ssid_from_config(&json!({})), "ADOS-GS-0000");
    }

    #[test]
    fn resolve_ap_ssid_gate_matches_the_hostapd_override_rule() {
        // Honoured: non-empty, no template, ADOS-GS- prefix.
        assert_eq!(resolve_ap_ssid("ADOS-GS-1234", "ffff"), "ADOS-GS-1234");
        // Rejected: carries the template placeholder.
        assert_eq!(
            resolve_ap_ssid("ADOS-GS-{device_id}", "abcd"),
            "ADOS-GS-ABCD"
        );
        // Rejected: empty.
        assert_eq!(resolve_ap_ssid("", "abcd"), "ADOS-GS-ABCD");
        // Rejected: wrong prefix.
        assert_eq!(resolve_ap_ssid("Other-1234", "abcd"), "ADOS-GS-ABCD");
    }

    #[test]
    fn short_id_matches_the_python_short_id() {
        assert_eq!(short_id("deadbeef"), "DEAD");
        assert_eq!(short_id("xy-12-34-56"), "1234");
        assert_eq!(short_id("a1"), "A100");
        assert_eq!(short_id(""), "0000");
        assert_eq!(short_id("zzzz"), "0000");
    }

    /// With no signal source at all, every measured leg is `null` and the key is
    /// still there.
    ///
    /// This block used to report `cpu_pct: 0.0, ram_used_mb: 0, ram_total_mb: 0`
    /// — all legal readings — so the OLED and the GCS Hardware tab showed an idle
    /// CPU and 0 MB of RAM on a stock ground station, where the durable store
    /// ships off. The old test asserted `unwrap_or(0.0)` against a copy of the
    /// derivation written inside the test, so it could not have caught it.
    #[test]
    fn system_block_with_no_signals_nulls_every_measured_leg() {
        let block = system_block(None, 1234, "9.9.9");
        for leg in [
            "cpu_pct",
            "ram_used_mb",
            "ram_total_mb",
            "temp_c",
            "disk_pct",
        ] {
            assert_eq!(block[leg], Value::Null, "{leg} must be null, not a number");
            assert!(
                block.as_object().unwrap().contains_key(leg),
                "{leg} must be present as an explicit null"
            );
        }
        // The two legs that are always knowable stay real.
        assert_eq!(block["uptime_seconds"], json!(1234));
        assert_eq!(block["agent_version"], json!("9.9.9"));
    }

    #[test]
    fn system_block_derives_cpu_ram_and_temp_from_signals() {
        let s = signals(&[
            ("cpu.util.all", json!(12.5)),
            ("mem.total_bytes", json!(4_000_000_000_i64)),
            ("mem.avail_bytes", json!(1_000_000_000_i64)),
            ("thermal.primary_c", json!(47.5)),
            ("disk.fs_total_bytes", json!(32_000_000_000_i64)),
            ("disk.fs_used_bytes", json!(8_000_000_000_i64)),
        ]);
        let block = system_block(Some(&s), 7, "1.2.3");
        assert_eq!(block["cpu_pct"], json!(12.5));
        // (4e9 - 1e9) / 1MiB used; 4e9 / 1MiB total.
        assert_eq!(
            block["ram_used_mb"],
            json!(((3_000_000_000_f64) / (1024.0 * 1024.0)) as i64)
        );
        assert_eq!(
            block["ram_total_mb"],
            json!(((4_000_000_000_f64) / (1024.0 * 1024.0)) as i64)
        );
        assert_eq!(block["temp_c"], json!(47.5));
        assert_eq!(block["disk_pct"], json!(25.0));
    }

    #[test]
    fn system_block_nulls_disk_without_both_byte_signals() {
        let used_only = signals(&[("disk.fs_used_bytes", json!(8_000_000_000_i64))]);
        assert_eq!(
            system_block(Some(&used_only), 0, "")["disk_pct"],
            Value::Null
        );
        let zero_total = signals(&[
            ("disk.fs_total_bytes", json!(0)),
            ("disk.fs_used_bytes", json!(0)),
        ]);
        assert_eq!(
            system_block(Some(&zero_total), 0, "")["disk_pct"],
            Value::Null
        );
    }

    #[test]
    fn mesh_block_projects_the_five_fields() {
        let snap: Map<String, Value> = json!({
            "up": true,
            "neighbors": [{"id": "a"}, {"id": "b"}],
            "selected_gateway": "node-1",
            "partition": false,
            "mesh_id": "mesh-xyz",
            "extra": "ignored",
        })
        .as_object()
        .unwrap()
        .clone();
        let block = mesh_block_from_snapshot(&snap);
        let want = json!({
            "up": true,
            "peer_count": 2,
            "selected_gateway": "node-1",
            "partition": false,
            "mesh_id": "mesh-xyz",
            "stale": false,
        });
        assert_eq!(block, want);
    }

    /// A clock advanced past the freshness window, used to age a just-written
    /// sidecar without back-dating the file (there is no portable mtime setter,
    /// and sleeping for 10 s in a unit test is not a test).
    fn later_than_the_window() -> SystemTime {
        SystemTime::now() + Duration::from_secs_f64(SNAPSHOT_FRESH_S + 5.0)
    }

    /// A mesh sidecar older than the freshness window must NOT be served as the
    /// current mesh state.
    ///
    /// This is the whole finding: the sidecar lives on a tmpfs that outlives the
    /// process that wrote it, so a dead mesh loop left `up: true` and a peer
    /// count to be re-served indefinitely. An operator reads a partitioned mesh
    /// as healthy.
    #[test]
    fn an_aged_mesh_sidecar_is_reported_stale_not_served_as_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh-state.json");
        std::fs::write(
            &path,
            r#"{"up":true,"neighbors":[{"id":"a"}],"selected_gateway":"node-1",
                "partition":false,"mesh_id":"mesh-xyz"}"#,
        )
        .unwrap();

        // Just written: the real reading comes through.
        let live = mesh_block_from_sidecar(&path, SystemTime::now());
        assert_eq!(live["up"], json!(true));
        assert_eq!(live["peer_count"], json!(1));
        assert_eq!(live["stale"], json!(false));

        // The same file, read past the window.
        let stale = mesh_block_from_sidecar(&path, later_than_the_window());
        assert_eq!(stale["stale"], json!(true));
        assert_eq!(
            stale["up"],
            Value::Null,
            "an aged snapshot must not keep claiming the mesh is up"
        );
        assert_eq!(stale["peer_count"], Value::Null);
        assert_eq!(stale["mesh_id"], Value::Null);
        // The key set is unchanged apart from the added flag, so no consumer sees
        // a field simply vanish (a missing key reads as "not reported" and gets
        // coerced, which is the same defect wearing a different hat).
        let mut keys: Vec<&str> = stale
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "mesh_id",
                "partition",
                "peer_count",
                "selected_gateway",
                "stale",
                "up",
            ]
        );
    }

    #[test]
    fn mesh_sidecar_of_an_absent_file_is_the_stale_block() {
        let dir = tempfile::tempdir().unwrap();
        let block = mesh_block_from_sidecar(&dir.path().join("nope.json"), SystemTime::now());
        assert_eq!(block["stale"], json!(true));
        assert_eq!(block["up"], Value::Null);
    }

    /// A relay/receiver counter route with an aged sidecar must null the counters
    /// rather than re-serve them, and must keep the key set so nothing downstream
    /// reads a vanished key as "not reported" and coerces it.
    #[test]
    fn an_aged_relay_sidecar_nulls_its_counters_under_a_stale_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-relay.json");
        std::fs::write(
            &path,
            r#"{"up":true,"fragments_in":9001,"fragments_out":8999}"#,
        )
        .unwrap();

        let live =
            read_fresh_json(&path, SystemTime::now()).expect("a just-written sidecar is fresh");
        assert_eq!(fresh_snapshot_body(live)["fragments_in"], json!(9001));

        assert!(
            read_fresh_json(&path, later_than_the_window()).is_none(),
            "an aged sidecar must not read as fresh"
        );
        let stale = stale_snapshot_body(read_json_or_empty(&path));
        assert_eq!(stale["stale"], json!(true));
        assert_eq!(stale["up"], Value::Null);
        assert_eq!(
            stale["fragments_in"],
            Value::Null,
            "a counter from a dead loop must not be served as current"
        );
        assert_eq!(stale["fragments_out"], Value::Null);
    }

    #[test]
    fn receiver_relays_slice_stamps_fresh_and_defaults_to_empty_list() {
        let empty: Map<String, Value> = Map::new();
        assert_eq!(
            slice_receiver_relays(&empty),
            json!({"relays": [], "stale": false})
        );
        let with: Map<String, Value> = json!({"relays": [{"id": "r1"}]})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            slice_receiver_relays(&with),
            json!({"relays": [{"id": "r1"}], "stale": false})
        );
    }

    /// With nothing current, `relays` is `null` — never `[]`, which reads as
    /// "this node looked and there are no relays".
    #[test]
    fn receiver_combined_stale_nulls_every_counter() {
        assert_eq!(
            receiver_combined_stale(),
            json!({
                "fragments_after_dedup": Value::Null,
                "fec_repaired": Value::Null,
                "output_kbps": Value::Null,
                "up": Value::Null,
                "stale": true,
            })
        );
    }

    #[test]
    fn receiver_combined_slice_applies_per_key_defaults() {
        let empty: Map<String, Value> = Map::new();
        let want = json!({
            "fragments_after_dedup": 0,
            "fec_repaired": 0,
            "output_kbps": 0,
            "up": false,
            "stale": false,
        });
        assert_eq!(slice_receiver_combined(&empty), want);
        let full: Map<String, Value> = json!({
            "fragments_after_dedup": 100,
            "fec_repaired": 5,
            "output_kbps": 4200,
            "up": true,
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            slice_receiver_combined(&full),
            json!({
                "fragments_after_dedup": 100,
                "fec_repaired": 5,
                "output_kbps": 4200,
                "up": true,
                "stale": false,
            })
        );
    }

    #[test]
    fn wfb_view_of_an_empty_config_is_the_python_defaults() {
        // No video.wfb section → the Python defaults channel 0, profile "default",
        // fec "8/12".
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.yaml");
        std::fs::write(&cfg_path, "agent:\n  profile: ground_station\n").unwrap();
        let cfg = WfbViewConfig::load_from(&cfg_path);
        let view = json!({
            "channel": cfg.video.wfb.channel.unwrap_or(0),
            "bitrate_profile": cfg
                .video
                .wfb
                .bitrate_profile
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            "fec": cfg.video.wfb.fec.clone().unwrap_or_else(|| "8/12".to_string()),
        });
        let want = json!({
            "channel": 0,
            "bitrate_profile": "default",
            "fec": "8/12",
        });
        assert_eq!(view, want);
    }

    #[test]
    fn wfb_view_reads_the_configured_radio() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.yaml");
        std::fs::write(
            &cfg_path,
            "video:\n  wfb:\n    channel: 161\n    bitrate_profile: high\n    fec: 12/16\n",
        )
        .unwrap();
        let cfg = WfbViewConfig::load_from(&cfg_path);
        assert_eq!(cfg.video.wfb.channel, Some(161));
        assert_eq!(cfg.video.wfb.bitrate_profile.as_deref(), Some("high"));
        assert_eq!(cfg.video.wfb.fec.as_deref(), Some("12/16"));
    }

    #[test]
    fn config_gs_role_defaults_to_direct() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.yaml");
        std::fs::write(&cfg_path, "agent:\n  profile: ground_station\n").unwrap();
        assert_eq!(ground_station_config_role(&cfg_path), "direct");
        std::fs::write(&cfg_path, "ground_station:\n  role: relay\n").unwrap();
        assert_eq!(ground_station_config_role(&cfg_path), "relay");
    }

    #[test]
    fn config_gs_peer_prefers_video_wfb_then_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.yaml");
        // Canonical spot wins.
        std::fs::write(
            &cfg_path,
            "video:\n  wfb:\n    paired_with_device_id: drone-abc\nground_station:\n  paired_drone_id: drone-old\n",
        )
        .unwrap();
        assert_eq!(config_gs_peer(&cfg_path), json!("drone-abc"));
        // Legacy fallback when the canonical spot is absent.
        std::fs::write(&cfg_path, "ground_station:\n  paired_drone_id: drone-old\n").unwrap();
        assert_eq!(config_gs_peer(&cfg_path), json!("drone-old"));
        // Neither present → null.
        std::fs::write(&cfg_path, "agent:\n  profile: ground_station\n").unwrap();
        assert_eq!(config_gs_peer(&cfg_path), Value::Null);
    }

    #[test]
    fn profile_conf_mesh_capable_reads_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("profile.conf");
        std::fs::write(&conf, "profile: ground_station\nmesh_capable: true\n").unwrap();
        assert!(profile_conf_mesh_capable(&conf));
        std::fs::write(&conf, "profile: ground_station\n").unwrap();
        assert!(!profile_conf_mesh_capable(&conf));
        assert!(!profile_conf_mesh_capable(&dir.path().join("absent.conf")));
    }

    #[test]
    fn pair_identity_of_an_unpaired_gs_is_null_null() {
        // No rx.key file → not paired → device_id + fingerprint both null. Drive
        // the pieces the handler composes without the full AppState.
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("wfb").join("rx.key");
        let paired = std::fs::metadata(&key_path)
            .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES as u64)
            .unwrap_or(false);
        assert!(!paired);
        // The unpaired snapshot the route emits.
        let snapshot = json!({
            "device_id": Value::Null,
            "key_fingerprint": Value::Null,
            "fc_mode": Value::Null,
            "battery_pct": Value::Null,
            "gps_sats": Value::Null,
        });
        let want = json!({
            "device_id": null,
            "key_fingerprint": null,
            "fc_mode": null,
            "battery_pct": null,
            "gps_sats": null,
        });
        assert_eq!(snapshot, want);
    }

    #[test]
    fn truthiness_matches_python_bool() {
        assert!(!json_truthy(&Value::Null));
        assert!(!json_truthy(&json!(false)));
        assert!(json_truthy(&json!(true)));
        assert!(!json_truthy(&json!(0)));
        assert!(json_truthy(&json!(149)));
        assert!(!json_truthy(&json!("")));
        assert!(json_truthy(&json!("x")));
        assert!(!json_truthy(&json!([])));
    }

    /// The radio-learned peer id must land on the key THIS route reads.
    ///
    /// Two halves that never met: `ados-radio` latched the beacon's device id into
    /// `peer-backfill.json` and the only consumer was a Python function with zero
    /// callers, so `paired_drone.device_id` was null on every auto-bound rig,
    /// across reboots. Asserting the write and the read against one file is what
    /// keeps the two ends on the same key.
    #[test]
    fn a_backfilled_peer_id_is_what_the_status_route_reports() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: my-gs\n").unwrap();

        assert_eq!(
            config_gs_peer(&cfg),
            Value::Null,
            "null before the back-fill"
        );

        crate::routes::peer_backfill::persist_peer_device_id(&cfg, true, "drone-abc")
            .expect("the back-fill persists");

        assert_eq!(config_gs_peer(&cfg), json!("drone-abc"));
    }

    /// The golden `/status` body for a direct ground-station node with no paired
    /// drone, no radio snapshot, no store, no recorder — the empty-rig shape the
    /// GCS Hardware tab reads on a fresh, idle GS. Composed from the same blocks the
    /// handler composes (the volatile system + link fields are not part of this
    /// fixture's stable contract; this pins the structural keys + the static legs).
    #[test]
    fn status_golden_fixture_for_an_idle_direct_gs() {
        let dir = tempfile::tempdir().unwrap();
        // An absent wfb-stats.json under a private tempdir → the disconnected link
        // base, with no process-global env mutation.
        let absent_stats = dir.path().join("wfb-stats.json");

        let role = "direct".to_string();
        let role_block = json!({
            "current": role,
            "configured": "direct",
            "supported": ["direct", "relay", "receiver"],
            "mesh_capable": false,
        });
        let body = json!({
            "profile": "ground-station",
            "paired_drone": {
                "device_id": Value::Null,
                "key_fingerprint": Value::Null,
                "fc_mode": Value::Null,
                "battery_pct": Value::Null,
                "gps_sats": Value::Null,
            },
            "link": link_view_from(&absent_stats),
            "gcs": {"clients": [], "pic_id": Value::Null},
            // The idle-GS network view: the resolved SSID with the AP unit down
            // (ap_ip gated to null), composed from the pure seam so the fixture
            // does not depend on the host's `systemctl` answer.
            "network": network_view_compose("ADOS-GS-0000", false, (Value::Null, Value::Null)),
            "recording": false,
            "video": {"recording": false, "recording_filename": Value::Null},
            "role": role_block,
            "mesh": json!({}),
        });

        // The structural contract: the top-level key set + the static legs.
        let want_keys = [
            "profile",
            "paired_drone",
            "link",
            "gcs",
            "network",
            "recording",
            "video",
            "role",
            "mesh",
        ];
        let obj = body.as_object().unwrap();
        for k in want_keys {
            assert!(obj.contains_key(k), "missing key {k}");
        }
        assert_eq!(
            body["profile"],
            json!("ground-station"),
            "one spelling of the profile enum across the wire: the hyphen form \
             every other producer and the relay-proxy gate use"
        );
        assert_eq!(body["paired_drone"]["device_id"], Value::Null);
        assert_eq!(body["paired_drone"]["key_fingerprint"], Value::Null);
        assert_eq!(body["gcs"], json!({"clients": [], "pic_id": null}));
        assert_eq!(body["recording"], json!(false));
        assert_eq!(
            body["video"],
            json!({"recording": false, "recording_filename": null})
        );
        assert_eq!(
            body["role"],
            json!({
                "current": "direct",
                "configured": "direct",
                "supported": ["direct", "relay", "receiver"],
                "mesh_capable": false,
            })
        );
        assert_eq!(body["mesh"], json!({}));
        // The link view is the disconnected base (no snapshot present).
        assert_eq!(body["link"]["state"], json!("connecting"));
        assert_eq!(body["link"]["channel"], Value::Null);
    }
}
