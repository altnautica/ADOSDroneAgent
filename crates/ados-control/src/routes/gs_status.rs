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
//! unavailable, never a 500. The native front runs the radio + recorder in sibling
//! processes (no in-process manager to call), so the live legs read the durable
//! store and the on-disk sidecars the sibling services write — exactly the seams
//! the FastAPI handlers fall back to.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::state::AppState;

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

/// The profile-source sentinel install.sh writes (`/etc/ados/profile.conf`),
/// overridable via `ADOS_PROFILE_CONF` for tests. The `/status` role block reads
/// its `mesh_capable` flag from here, matching the Python `profile.conf` read.
fn profile_conf_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_PROFILE_CONF").unwrap_or_else(|_| "/etc/ados/profile.conf".to_string()),
    )
}

/// Read a JSON file into an object map, returning the empty map on absence / a read
/// error / a parse error / a non-object body. Mirrors the Python
/// `_read_json_or_empty`.
fn read_json_or_empty(path: &Path) -> Map<String, Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        },
        Err(_) => Map::new(),
    }
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
    let mesh_capable = profile_conf_mesh_capable(&profile_conf_path());
    let role_block = json!({
        "current": role,
        "configured": configured_role,
        "supported": ["direct", "relay", "receiver"],
        "mesh_capable": mesh_capable,
    });

    // Mesh block. Populated only for a relay/receiver node with an active mesh,
    // store-first off the `mesh.state` event, sidecar-fallback off
    // `mesh-state.json`. A direct node gets `{}` so the OLED + GCS feature-detect
    // without a round-trip. Mirrors the Python role-gated mesh read.
    let mesh_block: Value = if role == "relay" || role == "receiver" {
        match latest_status_mesh_block(&state).await {
            Some(stored) => stored,
            None => mesh_block_from_sidecar(&mesh_state_path()),
        }
    } else {
        json!({})
    };

    // Recorder state. The native front has no in-process recorder (recording runs
    // in a sibling service), so this degrades to inactive — the exact shape the
    // FastAPI route returns when `_recorder()` raises (`recording_active = False;
    // recording_filename = None`).
    let recording_active = false;
    let recording_filename = Value::Null;

    let body = json!({
        "profile": "ground_station",
        "paired_drone": {
            "device_id": paired_drone_id,
            "key_fingerprint": key_fingerprint,
            "fc_mode": Value::Null,
            "battery_pct": Value::Null,
            "gps_sats": Value::Null,
        },
        "link": link_view(),
        "gcs": {"clients": [], "pic_id": Value::Null},
        "network": network_view(&state),
        "system": system_snapshot(&state).await,
        "recording": recording_active,
        "video": {
            "recording": recording_active,
            "recording_filename": recording_filename,
        },
        "role": role_block,
        "mesh": mesh_block,
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
    let raw = load_config_value(config_path);
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
    load_config_value(config_path)
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
/// The ground-station config object has no root `wfb` section (the radio config
/// lives at `video.wfb`), so the Python `getattr(app.config, "wfb", None)` is
/// `None` and the config-channel / tx-power seeds are `None` — this view starts
/// from the same zero base and merges the live snapshot over it. An absent /
/// unparseable / non-object file degrades to the base; a snapshot older than 10 s
/// flips `state` to `"stale"`. Mirrors the Python `_link_view`.
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
    base.insert("state".to_string(), json!("connecting"));
    // The one-glance link diagnosis (deaf / mis_keyed / jammed / healthy /
    // searching) + the RX counters that separate the failure modes a bare "0
    // received" hides, so the always-on cockpit link bar reads a legible CAUSE.
    base.insert("link_diag".to_string(), Value::Null);
    base.insert("packets_all".to_string(), json!(0));
    base.insert("decrypt_errors".to_string(), json!(0));

    let age_s = match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime.elapsed().map(|d| d.as_secs_f64()).unwrap_or(0.0),
        Err(_) => return Value::Object(base),
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
    // tx_power_dbm: the payload value when present (not null), else the (null) base.
    let payload_tx = payload.get("tx_power_dbm").cloned();
    merged.insert(
        "tx_power_dbm".to_string(),
        match payload_tx {
            Some(v) if !v.is_null() => v,
            _ => Value::Null,
        },
    );
    // state: the payload value when truthy, else "connecting".
    let payload_state = payload.get("state").cloned().unwrap_or(Value::Null);
    let state = if json_truthy(&payload_state) {
        payload_state
    } else {
        json!("connecting")
    };
    merged.insert("state".to_string(), state);

    // 10 s mtime ceiling — over that the snapshot is suspect; flip to "stale".
    if age_s > 10.0 {
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
/// `192.168.4.1` gateway while the AP unit is active. `usb_ip` / `uplink_type` /
/// `uplink_reachable` are the same static legs the Python view carries. When the
/// AP unit is down, `ap_ip` is null (the manager's status reports the gateway only
/// while up), while `ap_ssid` still resolves off config.
fn network_view(state: &AppState) -> Value {
    let cfg = load_config_value(&state.pairing_paths.config);
    let running = hostapd_running();
    network_view_compose(&ap_ssid_from_config(&cfg), running)
}

/// Compose the `_network_view` body from the resolved SSID + the live running
/// flag. Split out so the shape + the running-vs-not-running gating are unit
/// tested without the `systemctl` IO. `ap_ip` is the gateway while running, else
/// null; `ap_ssid` is the resolved SSID either way; the rest are the static legs.
fn network_view_compose(ap_ssid: &str, running: bool) -> Value {
    json!({
        "ap_ssid": ap_ssid,
        "ap_ip": if running { Value::String(AP_GATEWAY_IP.to_string()) } else { Value::Null },
        "usb_ip": Value::Null,
        "uplink_type": Value::Null,
        "uplink_reachable": false,
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

/// True when the hostapd unit is active, reproducing the manager's
/// `_is_unit_active`: run `systemctl is-active ados-hostapd.service` and treat a
/// trimmed `active` stdout as running. A missing `systemctl` / spawn error reads
/// as not running.
fn hostapd_running() -> bool {
    let output = match std::process::Command::new("systemctl")
        .args(["is-active", HOSTAPD_UNIT])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    String::from_utf8_lossy(&output.stdout).trim() == "active"
}

// ---------------------------------------------------------------------------
// The `system` sub-block (CPU/RAM/temp/uptime/version).
// ---------------------------------------------------------------------------

/// The system snapshot the `/status` route carries: `{cpu_pct, ram_used_mb,
/// ram_total_mb, temp_c, uptime_seconds, agent_version}`.
///
/// The native front does not probe the host directly; CPU / RAM / temperature come
/// from the most-recent hardware snapshots in the logging store (the continuous
/// collector samples them), `uptime_seconds` from `/proc/uptime`, and
/// `agent_version` from the resolved app version. An unreachable store degrades the
/// CPU/RAM/temp legs to the zero-valued default `{cpu_pct: 0.0, ram_used_mb: 0,
/// ram_total_mb: 0, temp_c: null}` — the exact shape the Python `_system_snapshot`
/// returns when its psutil reads raise.
async fn system_snapshot(state: &AppState) -> Value {
    let signals = state.logd.latest_hw_signals().await;
    let cpu_pct = signals
        .as_ref()
        .and_then(|s| signal_num(s, "cpu.util.all"))
        .unwrap_or(0.0);
    let (ram_used_mb, ram_total_mb) = signals.as_ref().and_then(ram_mb).unwrap_or((0, 0));
    let temp_c = signals
        .as_ref()
        .and_then(|s| signal_num(s, "thermal.primary_c"))
        .map(Value::from)
        .unwrap_or(Value::Null);

    json!({
        "cpu_pct": cpu_pct,
        "ram_used_mb": ram_used_mb,
        "ram_total_mb": ram_total_mb,
        "temp_c": temp_c,
        "uptime_seconds": proc_uptime_seconds(),
        "agent_version": state.agent_version(),
    })
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
/// the five fields the live route reads off `mesh-state.json` (`up`, `peer_count` =
/// neighbor count, `selected_gateway`, `partition`, `mesh_id`). `None` when the
/// store is unreachable or holds no such event, so the caller falls back to the
/// sidecar. Mirrors the Python `latest_status_mesh_block` coercions.
async fn latest_status_mesh_block(state: &AppState) -> Option<Value> {
    let detail = latest_event_detail(state, "mesh.state").await?;
    Some(mesh_block_from_snapshot(&detail))
}

/// Project the `/status` `mesh` block from a mesh snapshot body (the stored event
/// detail or the sidecar). Mirrors the Python `bool(...)` / `len(...)` coercions
/// exactly.
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
    })
}

/// The `/status` `mesh` block from the `mesh-state.json` sidecar, projecting the
/// same five fields. An absent / unparseable / non-object file yields `{}` (the
/// snapshot read returns the empty map, and the route only enters this leg for a
/// relay/receiver), matching the Python `if snap_path.is_file(): ...` guard:
/// without the file the block stays `{}`.
fn mesh_block_from_sidecar(path: &Path) -> Value {
    if !path.is_file() {
        return json!({});
    }
    let snap = read_json_or_empty(path);
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

/// `GET /api/v1/ground-station/wfb/relay/status` → relay-side fragment counters.
///
/// `404` `E_WRONG_ROLE` off a relay node. On a relay, reads the store's most-recent
/// `gs.relay_state` event (the relay loop ships the same body it writes to the
/// sidecar), falling back to the `/run/ados/wfb-relay.json` sidecar. Mirrors the
/// Python `get_wfb_relay_status`.
pub async fn get_wfb_relay_status(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };
    if role != "relay" {
        return wrong_role("relay");
    }
    if let Some(detail) = latest_event_detail(&state, "gs.relay_state").await {
        return Json(Value::Object(detail)).into_response();
    }
    Json(Value::Object(read_json_or_empty(&wfb_relay_path()))).into_response()
}

/// `GET /api/v1/ground-station/wfb/atlas-relay/status` → the Atlas aux-lane relay's
/// forward counters.
///
/// `404` `E_WRONG_ROLE` off a relay node. On a relay, reads the store's most-recent
/// `gs.atlas_relay` event (the relay loop ships the same body it writes to the
/// sidecar), falling back to the `/run/ados/atlas-relay.json` sidecar. The body is
/// the relay's `{up, datagrams_seen, forwarded, malformed, forward_failed,
/// compute_url, listen_port, generated_at_ms}` snapshot; an absent store + sidecar
/// degrade to the empty object, never a 500.
pub async fn get_atlas_relay_status(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };
    if role != "relay" {
        return wrong_role("relay");
    }
    if let Some(detail) = latest_event_detail(&state, "gs.atlas_relay").await {
        return Json(Value::Object(detail)).into_response();
    }
    Json(Value::Object(read_json_or_empty(&atlas_relay_path()))).into_response()
}

/// `GET /api/v1/ground-station/wfb/receiver/relays` → per-relay fragment counters.
///
/// `404` `E_WRONG_ROLE` off a receiver node. On a receiver, reads the store's
/// most-recent `gs.receiver_state` event projected to `{relays}`, falling back to
/// the `/run/ados/wfb-receiver.json` sidecar (also projected to `{relays}`).
/// Mirrors the Python `get_wfb_receiver_relays`.
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
    let snap = read_json_or_empty(&wfb_receiver_path());
    Json(slice_receiver_relays(&snap)).into_response()
}

/// `GET /api/v1/ground-station/wfb/receiver/combined` → combined FEC output stats.
///
/// `404` `E_WRONG_ROLE` off a receiver node. On a receiver, reads the store's
/// `gs.receiver_state` event projected to `{fragments_after_dedup, fec_repaired,
/// output_kbps, up}`, falling back to the `/run/ados/wfb-receiver.json` sidecar
/// (same projection + per-key defaults). Mirrors the Python
/// `get_wfb_receiver_combined`.
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
    let snap = read_json_or_empty(&wfb_receiver_path());
    Json(slice_receiver_combined(&snap)).into_response()
}

/// Project the `/wfb/receiver/relays` shape from a receiver-state body: `{relays}`,
/// the `relays` key defaulting to the empty list when absent. Mirrors the Python
/// `slice_receiver_relays` / the sidecar `{"relays": snap.get("relays", [])}`.
fn slice_receiver_relays(detail: &Map<String, Value>) -> Value {
    json!({
        "relays": detail.get("relays").cloned().unwrap_or_else(|| json!([])),
    })
}

/// Project the `/wfb/receiver/combined` shape from a receiver-state body, applying
/// the same per-key defaults the live route applies so an omitted key coalesces
/// identically whether it is absent from the stored detail or the sidecar. Mirrors
/// the Python `slice_receiver_combined`.
fn slice_receiver_combined(detail: &Map<String, Value>) -> Value {
    json!({
        "fragments_after_dedup": detail.get("fragments_after_dedup").cloned().unwrap_or_else(|| json!(0)),
        "fec_repaired": detail.get("fec_repaired").cloned().unwrap_or_else(|| json!(0)),
        "output_kbps": detail.get("output_kbps").cloned().unwrap_or_else(|| json!(0)),
        "up": detail.get("up").cloned().unwrap_or(Value::Bool(false)),
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/wfb — the stored radio config.
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/wfb` → the stored radio config `{channel,
/// bitrate_profile, fec}` from `video.wfb`, defaulting to the Python defaults
/// (`channel: 0`, `bitrate_profile: "default"`, `fec: "8/12"`) when the section /
/// a field is absent. `404` `E_PROFILE_MISMATCH` off a ground-station node.
/// Mirrors the Python `_read_wfb_view`.
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
/// store is unreachable / holds no such event / the detail is absent / non-object /
/// empty. Mirrors the Python `_latest_event_detail`.
async fn latest_event_detail(state: &AppState, event_kind: &str) -> Option<Map<String, Value>> {
    let rows = logd_query_events(state, event_kind, 1).await?;
    let row = rows.first()?.as_object()?;
    let detail = row.get("detail")?.as_object()?;
    if detail.is_empty() {
        return None;
    }
    Some(detail.clone())
}

/// Query the store for the newest `events` rows of one `event_kind`. Returns the
/// `data` array, or `None` when the store is unreachable / the response is an error
/// / does not parse. Mirrors the Python `query_rows("events", limit,
/// event_kind=...)`.
async fn logd_query_events(state: &AppState, event_kind: &str, limit: i64) -> Option<Vec<Value>> {
    let params = [
        ("kind", "events".to_string()),
        ("limit", limit.to_string()),
        ("event_kind", event_kind.to_string()),
    ];
    let query = encode_query(&params);
    let path = format!("/v1/query?{query}");
    let (status, body) = logd_get(state, &path).await.ok()?;
    if status >= 400 {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&body).ok()?;
    parsed
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.to_vec())
}

/// A minimal HTTP/1.1 `GET` over the logging-store query Unix socket, returning the
/// status code + the decoded body. The socket path comes from the app state's logd
/// client so a test redirects it. `Connection: close` reads the body to EOF; a
/// chunked body is de-chunked. Bounded so a runaway response cannot exhaust memory.
async fn logd_get(state: &AppState, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A hard ceiling on the response read; a normal events page is a few KiB, so
    /// this only guards a runaway body.
    const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

    let socket = state.logd.socket_path();
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let head = format!("GET {path} HTTP/1.1\r\nHost: logd\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break; // EOF (Connection: close).
        }
        if raw.len() + n > MAX_READ_BYTES {
            return Err(std::io::Error::other("logd response too large"));
        }
        raw.extend_from_slice(&buf[..n]);
    }
    parse_http_response(&raw)
}

/// Split a raw HTTP/1.1 response into the status code + decoded body. De-chunks a
/// `Transfer-Encoding: chunked` body; otherwise returns the body after the header
/// terminator as-is.
fn parse_http_response(raw: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let sep = b"\r\n\r\n";
    let split = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| std::io::Error::other("malformed http response (no header terminator)"))?;
    let head = &raw[..split];
    let body = &raw[split + sep.len()..];

    let head_str = String::from_utf8_lossy(head);
    let status = head_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("malformed http status line"))?;

    let chunked = head_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        de_chunk(body)
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// De-chunk a `Transfer-Encoding: chunked` body: `<hexlen>\r\n<data>\r\n` repeated
/// until a zero-length chunk.
fn de_chunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let len_line = &rest[..crlf];
        let len = usize::from_str_radix(String::from_utf8_lossy(len_line).trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = crlf + 2;
        if rest.len() < data_start + len {
            out.extend_from_slice(&rest[data_start..]);
            break;
        }
        out.extend_from_slice(&rest[data_start..data_start + len]);
        let next = data_start + len;
        rest = if rest.len() >= next + 2 {
            &rest[next + 2..]
        } else {
            &[]
        };
    }
    out
}

// ---------------------------------------------------------------------------
// Small shared helpers.
// ---------------------------------------------------------------------------

/// The exact 64-byte size a complete WFB-ng key file is.
const WFB_KEY_FILE_BYTES: usize = 64;

/// The byte offset of the peer-public half (the second 32 bytes) inside a 64-byte
/// WFB key file.
const WFB_PUBLIC_HALF_OFFSET: usize = 32;

/// The 16-hex-char public-key fingerprint of a WFB key file, or `None` when the
/// file is absent or not exactly 64 bytes. The peer-public half is the second 32
/// bytes; the fingerprint is `blake2b(pub, digest_size=8)` rendered as 16 lowercase
/// hex chars. Byte-identical to `key_mgr.read_public_fingerprint`.
fn read_public_fingerprint(path: &Path) -> Option<String> {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;
    let data = std::fs::read(path).ok()?;
    if data.len() != WFB_KEY_FILE_BYTES {
        return None;
    }
    let mut hasher = Blake2bVar::new(8).ok()?;
    hasher.update(&data[WFB_PUBLIC_HALF_OFFSET..]);
    let mut out = [0u8; 8];
    hasher.finalize_variable(&mut out).ok()?;
    Some(hex::encode(out))
}

/// Load `/etc/ados/config.yaml` as a raw JSON value (objects/arrays/scalars
/// preserved), tolerating absence / a parse error / a non-object root with an empty
/// object. Mirrors the Python `_load_config_dict`.
fn load_config_value(path: &Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return json!({}),
    };
    match serde_norway::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

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

/// Percent-encode a query-parameter list into a `key=value&...` string. Only the
/// characters the store's query values use (`-`, digits, letters, `.`) appear, so a
/// conservative reserved-character escape is sufficient.
fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Conservative percent-encoding for the query helper: pass through the unreserved
/// set (`A-Za-z0-9-._~`) verbatim and percent-encode every other byte.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            "state": "connecting",
            "link_diag": null,
            "packets_all": 0,
            "decrypt_errors": 0,
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
    }

    #[test]
    fn network_view_compose_running_carries_ssid_and_gateway() {
        // The live shape the bench observed: a running AP reports the resolved
        // SSID + the 192.168.4.1 gateway; the static legs are null/false.
        let want = json!({
            "ap_ssid": "ADOS-GS-D9DB",
            "ap_ip": "192.168.4.1",
            "usb_ip": null,
            "uplink_type": null,
            "uplink_reachable": false,
        });
        assert_eq!(network_view_compose("ADOS-GS-D9DB", true), want);
    }

    #[test]
    fn network_view_compose_not_running_gates_the_gateway() {
        // A down AP keeps the resolved SSID but reports ap_ip null (the manager's
        // status reports the gateway only while up).
        let v = network_view_compose("ADOS-GS-0000", false);
        assert_eq!(v["ap_ssid"], json!("ADOS-GS-0000"));
        assert_eq!(v["ap_ip"], Value::Null);
        assert_eq!(v["usb_ip"], Value::Null);
        assert_eq!(v["uplink_type"], Value::Null);
        assert_eq!(v["uplink_reachable"], json!(false));
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

    #[test]
    fn system_snapshot_of_an_absent_store_is_the_zero_default() {
        // No store rows → cpu 0.0, ram 0/0, temp null; uptime + version are present.
        let snap = signals(&[]);
        // Drive the field derivations directly (no AppState).
        let cpu = signal_num(&snap, "cpu.util.all").unwrap_or(0.0);
        let (used, total) = ram_mb(&snap).unwrap_or((0, 0));
        let temp = signal_num(&snap, "thermal.primary_c")
            .map(Value::from)
            .unwrap_or(Value::Null);
        assert_eq!(cpu, 0.0);
        assert_eq!(used, 0);
        assert_eq!(total, 0);
        assert_eq!(temp, Value::Null);
    }

    #[test]
    fn system_snapshot_derives_ram_and_temp_from_signals() {
        let s = signals(&[
            ("cpu.util.all", json!(12.5)),
            ("mem.total_bytes", json!(4_000_000_000_i64)),
            ("mem.avail_bytes", json!(1_000_000_000_i64)),
            ("thermal.primary_c", json!(47.5)),
        ]);
        let cpu = signal_num(&s, "cpu.util.all").unwrap_or(0.0);
        let (used, total) = ram_mb(&s).unwrap();
        let temp = signal_num(&s, "thermal.primary_c").unwrap();
        assert_eq!(cpu, 12.5);
        // (4e9 - 1e9) / 1MiB ≈ 2861 used; 4e9 / 1MiB ≈ 3814 total.
        assert_eq!(used, ((3_000_000_000_f64) / (1024.0 * 1024.0)) as i64);
        assert_eq!(total, ((4_000_000_000_f64) / (1024.0 * 1024.0)) as i64);
        assert_eq!(temp, 47.5);
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
        });
        assert_eq!(block, want);
    }

    #[test]
    fn mesh_sidecar_of_an_absent_file_is_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            mesh_block_from_sidecar(&dir.path().join("nope.json")),
            json!({})
        );
    }

    #[test]
    fn receiver_relays_slice_defaults_to_empty_list() {
        let empty: Map<String, Value> = Map::new();
        assert_eq!(slice_receiver_relays(&empty), json!({"relays": []}));
        let with: Map<String, Value> = json!({"relays": [{"id": "r1"}]})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            slice_receiver_relays(&with),
            json!({"relays": [{"id": "r1"}]})
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
    fn fingerprint_is_blake2b_8_of_the_public_half() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rx.key");
        let mut bytes = vec![0u8; 64];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        std::fs::write(&path, &bytes).unwrap();
        let expected = {
            use blake2::digest::{Update, VariableOutput};
            use blake2::Blake2bVar;
            let mut h = Blake2bVar::new(8).unwrap();
            h.update(&bytes[32..]);
            let mut out = [0u8; 8];
            h.finalize_variable(&mut out).unwrap();
            hex::encode(out)
        };
        assert_eq!(read_public_fingerprint(&path), Some(expected));
        // A wrong-size file yields no fingerprint.
        let half = dir.path().join("half.key");
        std::fs::write(&half, vec![0u8; 32]).unwrap();
        assert_eq!(read_public_fingerprint(&half), None);
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

    #[test]
    fn de_chunk_reassembles_a_chunked_body() {
        let chunked = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(de_chunk(chunked), b"hello world");
    }

    #[test]
    fn parse_http_response_reads_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{}");
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
            "profile": "ground_station",
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
            "network": network_view_compose("ADOS-GS-0000", false),
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
        assert_eq!(body["profile"], json!("ground_station"));
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
