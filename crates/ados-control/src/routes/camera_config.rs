//! Camera-roster read + write routes: `GET /api/video/roster` (the roster the
//! Cameras management surface renders) and `PUT /api/video/roster` (the operator
//! write that persists the leg list).
//!
//! The roster is the one place the operator sees EVERY camera the node knows
//! about — the legs declared in `video.cameras[]`, the devices the HAL enumeration
//! discovered (the `cameras-discovered.json` sidecar), and the live stream state
//! (`video-streams.json`) — reconciled into one list. Each row carries the leg's
//! logical identity + management metadata (name / orientation / purpose / owner /
//! fov / mount) and a `state` telling the operator whether it is assigned to a
//! stream, an unassigned discovered device, plugin-owned, or offline.
//!
//! Both routes are native (RUST-FIRST): the read merges three on-disk sidecars
//! with no per-request subprocess (the Python HAL writes the discovery sidecar at
//! enumeration time), and the write dials the supervisor's video command socket
//! directly. The read degrades to `{"cameras": []}` (guaranteed 200) when the
//! sidecars are absent, the same posture the status route takes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use ados_video::config::{CameraLeg, CameraMatch, RosterVideoConfig};

use crate::routes::detail;

// ---------------------------------------------------------------------------
// Path seam.
// ---------------------------------------------------------------------------

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`) — the same override the
/// sibling sidecars resolve under and the Python HAL writes into.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The config file (`ADOS_CONFIG`, default `/etc/ados/config.yaml`) the declared
/// `video.cameras[]` + the legacy `video.camera` block are loaded from.
fn config_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The discovery sidecar the Python camera enumeration writes
/// (`/run/ados/cameras-discovered.json`). Mirrors the Python
/// `cameras_discovered_path()`.
fn discovered_path() -> PathBuf {
    run_dir().join("cameras-discovered.json")
}

/// The live-streams sidecar the video orchestrator writes when it serves more
/// than one leg (`/run/ados/video-streams.json`).
fn live_streams_path() -> PathBuf {
    run_dir().join("video-streams.json")
}

/// The supervisor's video command socket (`/run/ados/video-cmd.sock`), the same
/// socket the plugin host forwards `video.source.set` to; the operator write
/// forwards `video.cameras.set` here. The supervisor is the config-write +
/// restart authority.
fn video_cmd_sock() -> PathBuf {
    run_dir().join("video-cmd.sock")
}

/// The orientation values a leg may declare (a coarse mount enum, not full
/// extrinsics).
const ORIENTATIONS: [&str; 8] = [
    "forward", "down", "back", "left", "right", "up", "gimbal", "custom",
];

/// The purpose values a leg may declare (what a plugin binds the camera to).
const PURPOSES: [&str; 7] = [
    "feed",
    "detect",
    "navigation",
    "precision-landing",
    "thermal",
    "mapping",
    "recording",
];

// ---------------------------------------------------------------------------
// Sidecar shapes.
// ---------------------------------------------------------------------------

/// One discovered device from the `cameras-discovered.json` sidecar. Only the
/// fields the roster reconciliation needs; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
struct DiscoveredCamera {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "type")]
    cam_type: String,
    #[serde(default)]
    device_path: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default, rename = "match")]
    camera_match: Option<CameraMatch>,
}

/// The `cameras-discovered.json` payload.
#[derive(Debug, Default, Deserialize)]
struct DiscoveredSnapshot {
    #[serde(default)]
    cameras: Vec<DiscoveredCamera>,
}

/// One live stream entry from `video-streams.json` (only id + liveness matter for
/// the roster).
#[derive(Debug, Clone, Deserialize)]
struct LiveStream {
    #[serde(default)]
    id: String,
    #[serde(default)]
    live: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct LiveSnapshot {
    #[serde(default)]
    streams: Vec<LiveStream>,
}

// ---------------------------------------------------------------------------
// GET /api/video/roster
// ---------------------------------------------------------------------------

/// `GET /api/video/roster` → the reconciled camera roster.
///
/// Merges the declared legs (`video.cameras[]`, or the legacy single
/// `video.camera` block synthesised as `main`), the discovered devices, and the
/// live stream state into one list. Guaranteed 200; degrades to `{"cameras": []}`
/// when nothing is declared and no device was discovered.
pub async fn get_video_cameras() -> Json<Value> {
    Json(json!({ "cameras": load_roster() }))
}

/// Load the three sources and reconcile them. Split from the handler so the file
/// wiring is one call and the reconciliation stays a pure, path-free function.
///
/// The config load is the QUIET single-pass [`RosterVideoConfig::load_from`]: it
/// parses `config.yaml` once and writes no config-status sidecar, so a pollable
/// roster read never races the `ados-video` service's own status stamping.
fn load_roster() -> Vec<Value> {
    let cfg = RosterVideoConfig::load_from(&config_path());
    // The camera roster is a companion-node concept. A ground station carries no
    // onboard camera, so it serves an empty roster rather than a phantom `main`.
    if cfg.is_ground_station() {
        return Vec::new();
    }
    let discovered = load_discovered(&discovered_path());
    let declared = declared_legs(&cfg, !discovered.is_empty());
    let live = load_live(&live_streams_path());
    build_roster(&declared, &discovered, &live)
}

// ---------------------------------------------------------------------------
// PUT /api/video/roster
// ---------------------------------------------------------------------------

/// The `PUT /api/video/roster` body: the operator's declared leg list. Each leg
/// is a free-form object (validated below) carrying at least an `id` + `source`,
/// plus the optional management fields. The owner is stamped by the supervisor
/// (the operator write is attributed to `operator`); a client-supplied `owner`
/// on a leg is ignored.
#[derive(Debug, Deserialize)]
pub struct PutCamerasBody {
    #[serde(default)]
    cameras: Vec<Value>,
}

/// `PUT /api/video/roster` — persist the operator's camera leg list.
///
/// Validates the list (path-safe unique ids, known orientation / purpose values,
/// no non-primary leg claiming the reserved `main` id), then forwards a
/// `video.cameras.set` op to the supervisor's video command socket, which merges
/// the operator legs by owner (preserving plugin-declared legs) and restarts the
/// video pipeline. A validation failure is a 400; an unreachable supervisor a
/// 503; a saved-but-not-restarted result a 502.
pub async fn put_video_cameras(Json(body): Json<PutCamerasBody>) -> Response {
    // The camera roster is a companion-node surface; a ground station has no
    // onboard camera to manage, so the write does not apply there.
    if RosterVideoConfig::load_from(&config_path()).is_ground_station() {
        return detail(
            StatusCode::NOT_FOUND,
            "the camera roster is not available on a ground station",
        );
    }
    if let Err(msg) = validate_cameras(&body.cameras) {
        return detail(StatusCode::BAD_REQUEST, msg);
    }
    let request = json!({
        "op": "video.cameras.set",
        "owner": "operator",
        "cameras": body.cameras,
    });
    match video_cmd(&request, &video_cmd_sock()).await {
        VideoCmd::Reply(reply) => classify_video_reply(reply),
        VideoCmd::Unavailable => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "video command socket unavailable",
        ),
    }
}

/// Validate the operator's leg list. Mirrors the supervisor's own
/// `video.source.set` checks (path-safe unique ids, no non-primary leg on the
/// reserved `main` id) and adds the orientation / purpose enum checks the
/// supervisor does not perform, so a bad edit is a clear 400 before the socket
/// round-trip rather than a corrupt config.
fn validate_cameras(cameras: &[Value]) -> Result<(), String> {
    if cameras.is_empty() {
        return Err("cameras must not be empty".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    // The primary leg (the first with role "primary", else the first) always
    // resolves to the reserved "main" path, so no OTHER leg may claim "main".
    let primary_idx = cameras
        .iter()
        .position(|l| l.get("role").and_then(Value::as_str) == Some("primary"))
        .unwrap_or(0);
    for (i, leg) in cameras.iter().enumerate() {
        let Some(obj) = leg.as_object() else {
            return Err("each camera must be an object".to_string());
        };
        let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
        let source = obj.get("source").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() || source.is_empty() {
            return Err("each camera needs a non-empty id and source".to_string());
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("camera id {id:?} has an unsafe character"));
        }
        if !seen.insert(id) {
            return Err(format!("duplicate camera id {id:?}"));
        }
        if i != primary_idx && id == "main" {
            return Err("a non-primary camera cannot use the reserved id \"main\"".to_string());
        }
        if let Some(orientation) = obj.get("orientation").and_then(Value::as_str) {
            if !orientation.is_empty() && !ORIENTATIONS.contains(&orientation) {
                return Err(format!("unknown orientation {orientation:?}"));
            }
        }
        match obj.get("purpose") {
            // Absent or explicitly null → no purpose (the empty list on reload).
            None | Some(Value::Null) => {}
            Some(Value::Array(purposes)) => {
                for p in purposes {
                    let p = p.as_str().unwrap_or("");
                    if !PURPOSES.contains(&p) {
                        return Err(format!("unknown purpose {p:?}"));
                    }
                }
            }
            // A non-array purpose (a bare string, a number) would be accepted here
            // but fail the `purpose: Vec<String>` deserialization on reload, so
            // reject it up front with a clear 400.
            Some(_) => {
                return Err("camera purpose must be an array of purpose strings".to_string());
            }
        }
    }
    Ok(())
}

/// The outcome of a video-command-socket round-trip.
enum VideoCmd {
    /// The supervisor replied with a JSON object (whatever `ok`/`error` it holds).
    Reply(Map<String, Value>),
    /// The socket was unreachable / did not reply / replied unparseably.
    Unavailable,
}

/// Send one newline-terminated JSON request to the video command socket and read
/// one newline-terminated JSON reply. Mirrors the `gs_network_write` round-trip;
/// the read is bounded so a runaway reply cannot exhaust memory. `sock` is
/// injectable so a test points it at a stub.
async fn video_cmd(request: &Value, sock: &Path) -> VideoCmd {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A reply is a few small fields; bound the read to guard a runaway.
    const MAX_REPLY_BYTES: usize = 64 * 1024;

    let mut stream = match tokio::net::UnixStream::connect(sock).await {
        Ok(s) => s,
        Err(_) => return VideoCmd::Unavailable,
    };
    let mut line = match serde_json::to_vec(request) {
        Ok(b) => b,
        Err(_) => return VideoCmd::Unavailable,
    };
    line.push(b'\n');
    if stream.write_all(&line).await.is_err() || stream.flush().await.is_err() {
        return VideoCmd::Unavailable;
    }

    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return VideoCmd::Unavailable,
        };
        if n == 0 {
            break; // EOF: the server replies once then closes.
        }
        if raw.len() + n > MAX_REPLY_BYTES {
            return VideoCmd::Unavailable;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.contains(&b'\n') {
            break;
        }
    }
    let text = match String::from_utf8(raw) {
        Ok(t) => t,
        Err(_) => return VideoCmd::Unavailable,
    };
    match text.lines().next().map(serde_json::from_str::<Value>) {
        Some(Ok(Value::Object(map))) => VideoCmd::Reply(map),
        _ => VideoCmd::Unavailable,
    }
}

/// Map the supervisor's reply object to an HTTP response.
///
/// `ok:true` → 200 with the reply. An `E_ARGS` / `E_PARSE` / `E_UNKNOWN_OP`
/// (a validation the handler should have caught) → 400; `E_PERSIST` (the config
/// write failed) → 500; a persisted-but-not-restarted result (the config saved,
/// the pipeline restart failed) → 502 so the operator retries; anything else →
/// 502.
fn classify_video_reply(reply: Map<String, Value>) -> Response {
    if reply.get("ok") == Some(&Value::Bool(true)) {
        return (StatusCode::OK, Json(Value::Object(reply))).into_response();
    }
    let error = reply.get("error").and_then(Value::as_str).unwrap_or("");
    match error {
        "E_ARGS" | "E_PARSE" | "E_UNKNOWN_OP" => detail(
            StatusCode::BAD_REQUEST,
            reply
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("invalid camera list")
                .to_string(),
        ),
        "E_PERSIST" => detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "camera config write failed",
        ),
        _ => detail(
            StatusCode::BAD_GATEWAY,
            "camera config saved but the video pipeline restart failed",
        ),
    }
}

/// The declared legs the roster reconciles: the explicit `video.cameras[]` list,
/// or — when it is empty — a single synthesised `main` leg from the legacy
/// `video.camera` block (matching `AgentVideoConfig::resolve_legs`), so a
/// single-camera drone shows its camera as the assigned main stream rather than
/// an unassigned discovered device.
///
/// The synthesised `main` leg is emitted only when the operator actually declared
/// a `video.camera` block (`cfg.camera` is `Some`) OR a camera was discovered
/// (`has_discovered`). A camera-less node (a compute box, a bench install with no
/// camera and no declared block) returns no legs, so the roster is empty instead
/// of showing a phantom offline `main`. A real single-camera drone that relies on
/// the config defaults (no explicit block) still shows its camera, because its
/// device was discovered.
fn declared_legs(cfg: &RosterVideoConfig, has_discovered: bool) -> Vec<CameraLeg> {
    if !cfg.cameras.is_empty() {
        return cfg.cameras.clone();
    }
    if cfg.camera.is_none() && !has_discovered {
        return Vec::new();
    }
    let camera = cfg.camera.clone().unwrap_or_default();
    vec![CameraLeg {
        id: "main".to_string(),
        source: camera.source.clone(),
        role: Some("primary".to_string()),
        codec: camera.codec.clone(),
        width: camera.width,
        height: camera.height,
        fps: camera.fps,
        bitrate_kbps: camera.bitrate_kbps,
        name: None,
        orientation: None,
        purpose: Vec::new(),
        enabled: true,
        owner: None,
        fov_deg: None,
        mount_pitch_deg: None,
        calibration: None,
        camera_match: None,
    }]
}

/// Read + parse the discovery sidecar, or an empty list on any read/parse failure
/// (an absent sidecar on a fresh boot, a dev host with no runtime dir).
fn load_discovered(path: &Path) -> Vec<DiscoveredCamera> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<DiscoveredSnapshot>(&text)
        .map(|s| s.cameras)
        .unwrap_or_default()
}

/// Read + parse the live-streams sidecar into an `id -> live` map, or an empty map
/// on any failure (an absent sidecar — the single-stream path never writes one).
fn load_live(path: &Path) -> HashMap<String, Option<bool>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(snap) = serde_json::from_str::<LiveSnapshot>(&text) else {
        return HashMap::new();
    };
    snap.streams
        .into_iter()
        .filter(|s| !s.id.is_empty())
        .map(|s| (s.id, s.live))
        .collect()
}

// ---------------------------------------------------------------------------
// Reconciliation.
// ---------------------------------------------------------------------------

/// True when a fingerprint carries no usable field (an empty `match: {}` the HAL
/// writes when it could not read a fingerprint).
fn match_is_empty(m: Option<&CameraMatch>) -> bool {
    match m {
        None => true,
        Some(m) => m.usb.is_none() && m.csi_sensor.is_none() && m.csi_port.is_none(),
    }
}

/// True when a declared leg's fingerprint identifies the same physical device as
/// a discovered one: an equal USB `vid:pid[:serial]`, or an equal CSI sensor +
/// port. Empty fingerprints never match (they identify no device).
fn fingerprint_matches(declared: Option<&CameraMatch>, discovered: Option<&CameraMatch>) -> bool {
    let (Some(d), Some(x)) = (declared, discovered) else {
        return false;
    };
    if let (Some(a), Some(b)) = (&d.usb, &x.usb) {
        if a == b {
            return true;
        }
    }
    if d.csi_sensor.is_some() && d.csi_sensor == x.csi_sensor && d.csi_port == x.csi_port {
        return true;
    }
    false
}

/// True when a leg source is a network capture URL (a leg mediamtx pulls, not a
/// local device the discovery covers).
fn is_network_source(source: &str) -> bool {
    let s = source.trim();
    s.starts_with("rtsp://") || s.starts_with("http://")
}

/// True when a leg source is a bare device-class hint (`csi` / `usb` / `ip`)
/// rather than a concrete `/dev/videoN` path or a URL — the shape the legacy
/// single-camera block carries.
fn is_hint_source(source: &str) -> bool {
    matches!(source.trim(), "csi" | "usb" | "ip")
}

/// Find the index of an unused discovered device this local leg identifies, or
/// `None`. Matches a concrete path by `device_path == source`, else by
/// fingerprint (a hot-plug renamed the node → re-pin); matches a bare hint source
/// by device class. Never matches a network leg (no discovery covers it).
fn match_discovered(
    leg: &CameraLeg,
    discovered: &[DiscoveredCamera],
    used: &[bool],
) -> Option<usize> {
    let source = leg.source.trim();
    if is_network_source(source) {
        return None;
    }
    if is_hint_source(source) {
        // A bare device-class hint (the legacy single-camera block's `csi` / `usb`
        // / `ip`). Prefer a device of the hinted class, then fall through the
        // pipeline's CSI→USB→IP auto-assign order (`camera_mgr.auto_assign`) so a
        // `csi`-hint main leg on a node whose only camera is USB reconciles to that
        // USB device — assigned, one row — rather than showing offline while the
        // USB camera appears as a phantom unassigned row.
        for class in [source, "csi", "usb", "ip"] {
            if let Some(i) = discovered
                .iter()
                .enumerate()
                .find_map(|(i, d)| (!used[i] && d.cam_type == class).then_some(i))
            {
                return Some(i);
            }
        }
        return None;
    }
    // Concrete path: exact device-path match first.
    if let Some(i) = discovered
        .iter()
        .enumerate()
        .find_map(|(i, d)| (!used[i] && d.device_path == source).then_some(i))
    {
        return Some(i);
    }
    // Else re-pin by fingerprint (the device moved to a different node).
    discovered.iter().enumerate().find_map(|(i, d)| {
        (!used[i] && fingerprint_matches(leg.camera_match.as_ref(), d.camera_match.as_ref()))
            .then_some(i)
    })
}

/// The owner tag, treating an absent or `"operator"` owner as operator-managed and
/// anything else as a plugin id.
fn is_plugin_owned(owner: Option<&str>) -> bool {
    matches!(owner, Some(o) if !o.is_empty() && o != "operator")
}

/// A JSON string value, or `null` for an absent / empty option.
fn opt_str(value: Option<&str>) -> Value {
    match value {
        Some(s) if !s.is_empty() => Value::from(s),
        _ => Value::Null,
    }
}

/// Serialize an optional fingerprint into the roster row, treating an empty one
/// as absent (`null`).
fn match_value(m: Option<&CameraMatch>) -> Value {
    if match_is_empty(m) {
        return Value::Null;
    }
    serde_json::to_value(m.expect("checked non-empty")).unwrap_or(Value::Null)
}

/// Reconcile declared legs + discovered devices + live state into the roster rows.
///
/// Every declared leg becomes a row (state `assigned` / `plugin_owned` when its
/// device is present or it is a network leg, `offline` when a declared local
/// device is not discovered). Every discovered device not claimed by a declared
/// leg becomes a `discovered_unassigned` row so the operator can assign it. Pure
/// (no I/O) so the full reconciliation matrix is unit-tested.
fn build_roster(
    declared: &[CameraLeg],
    discovered: &[DiscoveredCamera],
    live: &HashMap<String, Option<bool>>,
) -> Vec<Value> {
    // The primary declared leg is served at the fixed `main` path, so its live
    // state is keyed on `main` in the sidecar (matching `resolve_legs`).
    let primary_idx = declared
        .iter()
        .position(|l| l.role.as_deref() == Some("primary"));
    let mut used = vec![false; discovered.len()];
    let mut rows: Vec<Value> = Vec::new();

    for (i, leg) in declared.iter().enumerate() {
        let is_primary = match primary_idx {
            Some(p) => i == p,
            None => i == 0,
        };
        let served_id = if is_primary { "main" } else { leg.id.as_str() };

        let network = is_network_source(&leg.source);
        let mut device_path = if !network && leg.source.trim().contains('/') {
            // A concrete configured device path is the leg's intended device even
            // when it is currently absent.
            Some(leg.source.trim().to_string())
        } else {
            None
        };
        let mut match_val = leg.camera_match.clone();
        let mut present = network;

        if !network {
            if let Some(di) = match_discovered(leg, discovered, &used) {
                used[di] = true;
                let d = &discovered[di];
                device_path = Some(d.device_path.clone());
                if match_is_empty(match_val.as_ref()) {
                    match_val = d.camera_match.clone();
                }
                present = true;
            }
        }

        let state = if !present {
            "offline"
        } else if is_plugin_owned(leg.owner.as_deref()) {
            "plugin_owned"
        } else {
            "assigned"
        };

        // An offline leg is not live, regardless of a stale live-stream sidecar
        // reading — only a present leg reports its sidecar liveness (so the state
        // and live fields never contradict each other).
        let live_state = if present {
            live.get(served_id)
                .copied()
                .flatten()
                .or_else(|| live.get(&leg.id).copied().flatten())
        } else {
            None
        };

        rows.push(json!({
            "id": leg.id,
            "name": opt_str(leg.name.as_deref()),
            "source": leg.source,
            "role": opt_str(leg.role.as_deref()),
            "purpose": leg.purpose,
            "orientation": opt_str(leg.orientation.as_deref()),
            "enabled": leg.enabled,
            "owner": opt_str(leg.owner.as_deref()),
            "state": state,
            "live": live_state.map(Value::from).unwrap_or(Value::Null),
            "device_path": device_path.map(Value::from).unwrap_or(Value::Null),
            "width": leg.width,
            "height": leg.height,
            "fps": leg.fps,
            "codec": leg.codec,
            "bitrate_kbps": leg.bitrate_kbps,
            "match": match_value(match_val.as_ref()),
            "fov_deg": leg.fov_deg.map(Value::from).unwrap_or(Value::Null),
            "mount_pitch_deg": leg.mount_pitch_deg.map(Value::from).unwrap_or(Value::Null),
            "calibration": opt_str(leg.calibration.as_deref()),
        }));
    }

    // Discovered devices no declared leg claimed: unassigned candidates the
    // operator can add.
    for (i, d) in discovered.iter().enumerate() {
        if used[i] {
            continue;
        }
        let id = device_handle(&d.device_path);
        rows.push(json!({
            "id": id,
            "name": opt_str(Some(d.name.as_str())),
            "source": d.device_path,
            "role": Value::Null,
            "purpose": Vec::<String>::new(),
            "orientation": Value::Null,
            "enabled": false,
            "owner": Value::Null,
            "state": "discovered_unassigned",
            "live": Value::Null,
            "device_path": opt_str(Some(d.device_path.as_str())),
            "width": (d.width > 0).then_some(d.width).map(Value::from).unwrap_or(Value::Null),
            "height": (d.height > 0).then_some(d.height).map(Value::from).unwrap_or(Value::Null),
            "fps": Value::Null,
            "codec": Value::Null,
            "bitrate_kbps": Value::Null,
            "match": match_value(d.camera_match.as_ref()),
            "fov_deg": Value::Null,
            "mount_pitch_deg": Value::Null,
            "calibration": Value::Null,
        }));
    }

    rows
}

/// A stable roster handle for an unassigned discovered device: the device-node
/// basename (`/dev/video0` → `video0`), or the whole path when it has no `/`.
fn device_handle(device_path: &str) -> String {
    device_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(device_path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(id: &str, source: &str) -> CameraLeg {
        CameraLeg {
            id: id.to_string(),
            source: source.to_string(),
            role: None,
            codec: "h264".to_string(),
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 4000,
            name: None,
            orientation: None,
            purpose: Vec::new(),
            enabled: true,
            owner: None,
            fov_deg: None,
            mount_pitch_deg: None,
            calibration: None,
            camera_match: None,
        }
    }

    fn discovered(
        name: &str,
        cam_type: &str,
        path: &str,
        m: Option<CameraMatch>,
    ) -> DiscoveredCamera {
        DiscoveredCamera {
            name: name.to_string(),
            cam_type: cam_type.to_string(),
            device_path: path.to_string(),
            width: 0,
            height: 0,
            camera_match: m,
        }
    }

    fn usb_fp(usb: &str) -> CameraMatch {
        CameraMatch {
            usb: Some(usb.to_string()),
            csi_sensor: None,
            csi_port: None,
        }
    }

    #[test]
    fn absent_declared_and_discovered_yields_empty_roster() {
        let rows = build_roster(&[], &[], &HashMap::new());
        assert!(rows.is_empty());
    }

    #[test]
    fn declared_legs_empty_when_no_camera_declared_and_none_discovered() {
        // A camera-less node (no video.cameras[], no video.camera block, no
        // discovered device) declares no leg — the roster is empty, not a phantom
        // offline `main`.
        let cfg = RosterVideoConfig::default();
        assert!(declared_legs(&cfg, false).is_empty());
    }

    #[test]
    fn declared_legs_synthesizes_main_when_a_device_was_discovered() {
        // A real single-camera drone that relies on the config defaults (no
        // explicit block) still shows its camera, because a device was discovered.
        let cfg = RosterVideoConfig::default();
        let legs = declared_legs(&cfg, true);
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].id, "main");
        assert_eq!(legs[0].role.as_deref(), Some("primary"));
    }

    #[test]
    fn declared_legs_synthesizes_main_when_the_camera_block_was_declared() {
        // An explicitly declared camera block shows its `main` leg even when the
        // device is currently absent (it resolves to offline, honestly).
        let cfg = RosterVideoConfig {
            camera: Some(ados_video::config::CameraConfig {
                source: "/dev/video3".to_string(),
                ..Default::default()
            }),
            ..RosterVideoConfig::default()
        };
        let legs = declared_legs(&cfg, false);
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].source, "/dev/video3");
    }

    #[test]
    fn declared_legs_returns_the_explicit_cameras_list() {
        let cfg = RosterVideoConfig {
            cameras: vec![leg("eo", "/dev/video0"), leg("ir", "rtsp://pod/ir")],
            ..RosterVideoConfig::default()
        };
        let legs = declared_legs(&cfg, false);
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].id, "eo");
    }

    #[test]
    fn declared_local_leg_matched_by_device_path_is_assigned() {
        let mut l = leg("belly", "/dev/video2");
        l.role = Some("primary".to_string());
        let disc = vec![discovered(
            "USB Cam",
            "usb",
            "/dev/video2",
            Some(usb_fp("046d:0825")),
        )];
        let mut live = HashMap::new();
        live.insert("main".to_string(), Some(true));
        let rows = build_roster(&[l], &disc, &live);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], "assigned");
        assert_eq!(rows[0]["device_path"], "/dev/video2");
        assert_eq!(rows[0]["live"], true);
        // The declared leg had no fingerprint → enriched from the discovered one.
        assert_eq!(rows[0]["match"]["usb"], "046d:0825");
    }

    #[test]
    fn declared_local_leg_repins_by_fingerprint_when_the_node_moved() {
        // The leg was declared on /dev/video0 but the camera now enumerates on
        // /dev/video3; the fingerprint re-pins it (still assigned, device re-pinned).
        let mut l = leg("belly", "/dev/video0");
        l.camera_match = Some(usb_fp("046d:0825:ABC"));
        let disc = vec![discovered(
            "USB Cam",
            "usb",
            "/dev/video3",
            Some(usb_fp("046d:0825:ABC")),
        )];
        let rows = build_roster(&[l], &disc, &HashMap::new());
        assert_eq!(rows[0]["state"], "assigned");
        assert_eq!(rows[0]["device_path"], "/dev/video3");
    }

    #[test]
    fn declared_local_leg_absent_device_is_offline() {
        let l = leg("belly", "/dev/video9");
        let rows = build_roster(&[l], &[], &HashMap::new());
        assert_eq!(rows[0]["state"], "offline");
        // The intended configured device path is still surfaced.
        assert_eq!(rows[0]["device_path"], "/dev/video9");
        assert_eq!(rows[0]["live"], Value::Null);
    }

    #[test]
    fn an_offline_leg_is_never_reported_live() {
        // A stale live-stream sidecar still lists the leg as live, but the device
        // is gone (not discovered) → the row is offline AND live is null; the two
        // fields must not contradict each other.
        let l = leg("belly", "/dev/video9");
        let mut live = HashMap::new();
        live.insert("belly".to_string(), Some(true));
        let rows = build_roster(&[l], &[], &live);
        assert_eq!(rows[0]["state"], "offline");
        assert_eq!(rows[0]["live"], Value::Null);
    }

    #[test]
    fn plugin_owned_network_leg_is_plugin_owned() {
        let mut l = leg("ir", "rtsp://192.168.144.25:8554/ir");
        l.owner = Some("com.altnautica.siyi-pod".to_string());
        l.role = Some("ir".to_string());
        let mut live = HashMap::new();
        live.insert("ir".to_string(), Some(true));
        let rows = build_roster(&[l], &[], &live);
        assert_eq!(rows[0]["state"], "plugin_owned");
        assert_eq!(rows[0]["owner"], "com.altnautica.siyi-pod");
        assert_eq!(rows[0]["device_path"], Value::Null);
        assert_eq!(rows[0]["live"], true);
    }

    #[test]
    fn operator_network_leg_is_assigned() {
        let l = leg("cam", "rtsp://10.0.0.9/main");
        let rows = build_roster(&[l], &[], &HashMap::new());
        assert_eq!(rows[0]["state"], "assigned");
    }

    #[test]
    fn discovered_device_not_claimed_is_unassigned() {
        let disc = vec![DiscoveredCamera {
            name: "USB Cam".to_string(),
            cam_type: "usb".to_string(),
            device_path: "/dev/video0".to_string(),
            width: 1920,
            height: 1080,
            camera_match: Some(usb_fp("046d:0825")),
        }];
        let rows = build_roster(&[], &disc, &HashMap::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], "discovered_unassigned");
        assert_eq!(rows[0]["id"], "video0");
        assert_eq!(rows[0]["enabled"], false);
        assert_eq!(rows[0]["width"], 1920);
        assert_eq!(rows[0]["match"]["usb"], "046d:0825");
    }

    #[test]
    fn legacy_hint_source_matches_a_discovered_device_by_class() {
        // The synthesised legacy `main` leg (source "csi") reconciles to the one
        // discovered CSI camera by device class, so a single-camera drone shows it
        // as the assigned main stream, not an unassigned device.
        let mut l = leg("main", "csi");
        l.role = Some("primary".to_string());
        let disc = vec![discovered(
            "CSI-0 (imx219)",
            "csi",
            "/dev/video0",
            Some(CameraMatch {
                usb: None,
                csi_sensor: Some("imx219".to_string()),
                csi_port: Some(0),
            }),
        )];
        let rows = build_roster(&[l], &disc, &HashMap::new());
        // Exactly one row (the discovered camera was claimed by the main leg).
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "main");
        assert_eq!(rows[0]["state"], "assigned");
        assert_eq!(rows[0]["device_path"], "/dev/video0");
        assert_eq!(rows[0]["match"]["csi_sensor"], "imx219");
    }

    #[test]
    fn hint_source_falls_back_through_the_auto_assign_order() {
        // A `csi`-hint main leg on a node whose only camera is USB reconciles to
        // that USB device (the pipeline's CSI→USB→IP auto-assign fallback), so it
        // is assigned — a single row, not offline + a phantom unassigned USB row.
        let mut l = leg("main", "csi");
        l.role = Some("primary".to_string());
        let disc = vec![discovered(
            "USB Cam",
            "usb",
            "/dev/video0",
            Some(usb_fp("046d:0825")),
        )];
        let rows = build_roster(&[l], &disc, &HashMap::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "main");
        assert_eq!(rows[0]["state"], "assigned");
        assert_eq!(rows[0]["device_path"], "/dev/video0");
    }

    #[test]
    fn hint_source_prefers_the_exact_class_before_falling_back() {
        // A `usb` hint on a node with both a CSI and a USB camera takes the USB
        // one (exact-class match wins), leaving the CSI camera as an unassigned
        // candidate row.
        let mut l = leg("main", "usb");
        l.role = Some("primary".to_string());
        let disc = vec![
            discovered("CSI-0 (imx219)", "csi", "/dev/video0", None),
            discovered("USB Cam", "usb", "/dev/video1", None),
        ];
        let rows = build_roster(&[l], &disc, &HashMap::new());
        let main = rows.iter().find(|r| r["id"] == "main").unwrap();
        assert_eq!(main["state"], "assigned");
        assert_eq!(main["device_path"], "/dev/video1");
        assert!(rows
            .iter()
            .any(|r| r["state"] == "discovered_unassigned" && r["source"] == "/dev/video0"));
    }

    #[test]
    fn two_declared_legs_do_not_both_claim_one_device() {
        let a = leg("a", "/dev/video0");
        let b = leg("b", "/dev/video0");
        let disc = vec![discovered("Cam", "usb", "/dev/video0", None)];
        let rows = build_roster(&[a, b], &disc, &HashMap::new());
        // First leg claims the device (assigned); the second finds nothing (offline).
        assert_eq!(rows[0]["state"], "assigned");
        assert_eq!(rows[1]["state"], "offline");
    }

    #[test]
    fn empty_fingerprint_serializes_as_null() {
        let mut l = leg("cam", "/dev/video0");
        l.camera_match = Some(CameraMatch::default());
        let rows = build_roster(&[l], &[], &HashMap::new());
        assert_eq!(rows[0]["match"], Value::Null);
    }

    #[test]
    fn management_fields_pass_through_to_the_row() {
        let mut l = leg("belly", "/dev/video2");
        l.name = Some("Belly cam".to_string());
        l.orientation = Some("down".to_string());
        l.purpose = vec!["detect".to_string(), "precision-landing".to_string()];
        l.enabled = false;
        l.owner = Some("operator".to_string());
        l.fov_deg = Some(82.5);
        l.mount_pitch_deg = Some(-45.0);
        let disc = vec![discovered("Cam", "usb", "/dev/video2", None)];
        let rows = build_roster(&[l], &disc, &HashMap::new());
        let r = &rows[0];
        assert_eq!(r["name"], "Belly cam");
        assert_eq!(r["orientation"], "down");
        assert_eq!(r["purpose"], json!(["detect", "precision-landing"]));
        assert_eq!(r["enabled"], false);
        assert_eq!(r["owner"], "operator");
        assert_eq!(r["fov_deg"], 82.5);
        assert_eq!(r["mount_pitch_deg"], -45.0);
        // owner "operator" is not a plugin → assigned, not plugin_owned.
        assert_eq!(r["state"], "assigned");
    }

    #[test]
    fn bitrate_and_calibration_round_trip_to_the_row() {
        // The roster must carry the leg's transmit bitrate and calibration
        // reference so an operator PUT reads them back and does not silently reset
        // the WFB bitrate to the compiled default.
        let mut l = leg("belly", "/dev/video2");
        l.bitrate_kbps = 6500;
        l.calibration = Some("belly-v1".to_string());
        let disc = vec![discovered("Cam", "usb", "/dev/video2", None)];
        let rows = build_roster(&[l], &disc, &HashMap::new());
        assert_eq!(rows[0]["bitrate_kbps"], 6500);
        assert_eq!(rows[0]["calibration"], "belly-v1");
        // An unassigned discovered row carries neither (it is not a configured leg).
        let disc2 = vec![discovered("Cam", "usb", "/dev/video5", None)];
        let unassigned = build_roster(&[], &disc2, &HashMap::new());
        assert_eq!(unassigned[0]["bitrate_kbps"], Value::Null);
        assert_eq!(unassigned[0]["calibration"], Value::Null);
    }

    // --- sidecar loaders ---

    #[test]
    fn load_discovered_tolerates_absent_and_garbage() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_discovered(&dir.path().join("absent.json")).is_empty());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(load_discovered(&bad).is_empty());
        let ok = dir.path().join("ok.json");
        std::fs::write(
            &ok,
            r#"{"version":1,"cameras":[{"name":"c","type":"usb","device_path":"/dev/video0","match":{"usb":"1:2"}}]}"#,
        )
        .unwrap();
        let cams = load_discovered(&ok);
        assert_eq!(cams.len(), 1);
        assert_eq!(cams[0].device_path, "/dev/video0");
        assert_eq!(
            cams[0].camera_match.as_ref().unwrap().usb.as_deref(),
            Some("1:2")
        );
    }

    #[test]
    fn load_live_maps_id_to_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("video-streams.json");
        std::fs::write(
            &path,
            r#"{"version":2,"streams":[{"id":"main","role":"eo","codec":"h265","live":true},{"id":"ir","role":"ir","codec":"h264","live":false}]}"#,
        )
        .unwrap();
        let live = load_live(&path);
        assert_eq!(live.get("main").copied().flatten(), Some(true));
        assert_eq!(live.get("ir").copied().flatten(), Some(false));
        // Absent file → empty map.
        assert!(load_live(&dir.path().join("absent.json")).is_empty());
    }

    // --- PUT validation + reply classification + socket round-trip ---

    #[test]
    fn validate_cameras_accepts_a_well_formed_list() {
        let cams = vec![
            json!({"id": "eo", "source": "/dev/video0", "role": "primary", "orientation": "forward", "purpose": ["feed", "detect"]}),
            json!({"id": "belly", "source": "/dev/video1", "orientation": "down", "purpose": ["precision-landing"]}),
        ];
        assert!(validate_cameras(&cams).is_ok());
    }

    #[test]
    fn validate_cameras_rejects_bad_lists() {
        // Empty.
        assert!(validate_cameras(&[]).is_err());
        // Missing id/source.
        assert!(validate_cameras(&[json!({"id": "eo"})]).is_err());
        // Unsafe id char.
        assert!(validate_cameras(&[json!({"id": "a b", "source": "/dev/video0"})]).is_err());
        assert!(validate_cameras(&[json!({"id": "a/b", "source": "/dev/video0"})]).is_err());
        // Duplicate ids.
        assert!(validate_cameras(&[
            json!({"id": "eo", "source": "/dev/video0"}),
            json!({"id": "eo", "source": "/dev/video1"}),
        ])
        .is_err());
        // A non-primary leg claiming the reserved "main" id.
        assert!(validate_cameras(&[
            json!({"id": "eo", "source": "/dev/video0", "role": "primary"}),
            json!({"id": "main", "source": "/dev/video1", "role": "ir"}),
        ])
        .is_err());
        // Unknown orientation / purpose.
        assert!(validate_cameras(&[
            json!({"id": "eo", "source": "/dev/video0", "orientation": "sideways"})
        ])
        .is_err());
        assert!(validate_cameras(&[
            json!({"id": "eo", "source": "/dev/video0", "purpose": ["weaponise"]})
        ])
        .is_err());
        // A non-array purpose is rejected (it would break the reload otherwise).
        assert!(validate_cameras(&[
            json!({"id": "eo", "source": "/dev/video0", "purpose": "detect"})
        ])
        .is_err());
        // An explicit null purpose is accepted (treated as no purpose).
        assert!(
            validate_cameras(&[json!({"id": "eo", "source": "/dev/video0", "purpose": null})])
                .is_ok()
        );
    }

    #[test]
    fn classify_video_reply_maps_status_codes() {
        let ok = classify_video_reply(
            json!({"ok": true, "count": 2, "persisted": true, "restarted": true})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(ok.status(), StatusCode::OK);

        let args = classify_video_reply(
            json!({"ok": false, "error": "E_ARGS", "reason": "bad"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(args.status(), StatusCode::BAD_REQUEST);

        let persist = classify_video_reply(
            json!({"ok": false, "error": "E_PERSIST"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(persist.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Saved but the restart failed (no error field, ok:false).
        let restart = classify_video_reply(
            json!({"ok": false, "count": 1, "persisted": true, "restarted": false})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(restart.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn video_cmd_round_trips_a_reply() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("video-cmd.sock");
        // A stub server: accept one connection, read the request line, reply once.
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            // The request carries the operator op + owner.
            let req: Value =
                serde_json::from_slice(buf[..n].split(|b| *b == b'\n').next().unwrap()).unwrap();
            assert_eq!(req["op"], "video.cameras.set");
            assert_eq!(req["owner"], "operator");
            stream
                .write_all(b"{\"ok\":true,\"count\":1,\"persisted\":true,\"restarted\":true}\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
        });

        let req = json!({"op": "video.cameras.set", "owner": "operator", "cameras": [{"id": "eo", "source": "/dev/video0"}]});
        match video_cmd(&req, &sock).await {
            VideoCmd::Reply(reply) => {
                assert_eq!(reply.get("ok"), Some(&Value::Bool(true)));
                assert_eq!(reply.get("count"), Some(&json!(1)));
            }
            VideoCmd::Unavailable => panic!("expected a reply"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn video_cmd_is_unavailable_when_the_socket_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("absent.sock");
        let req = json!({"op": "video.cameras.set", "cameras": []});
        assert!(matches!(
            video_cmd(&req, &sock).await,
            VideoCmd::Unavailable
        ));
    }
}
