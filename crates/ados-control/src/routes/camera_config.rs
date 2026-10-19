//! Camera-roster read + write routes: `GET /api/video/roster` (the roster the
//! Cameras management surface renders) and `PUT /api/video/roster` (the operator
//! write that persists the leg list).
//!
//! The roster is the one place the operator sees EVERY camera the node knows
//! about — the legs declared in the top-level `cameras[]`, the devices the HAL enumeration
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

use ados_protocol::paths;
use ados_video::config::{CameraLeg, CameraMatch, RosterVideoConfig};

use crate::routes::detail;

mod reconcile;

use reconcile::*;

// ---------------------------------------------------------------------------
// Path seam.
// ---------------------------------------------------------------------------

/// The config file (`ADOS_CONFIG`, default `/etc/ados/config.yaml`) the declared
/// `cameras[]` + the legacy `video.camera` block are loaded from.
fn config_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The discovery sidecar the Python camera enumeration writes
/// (`/run/ados/cameras-discovered.json`). Mirrors the Python
/// `cameras_discovered_path()`.
fn discovered_path() -> PathBuf {
    paths::run_path("cameras-discovered")
}

/// The live-streams sidecar the video orchestrator writes when it serves more
/// than one leg (`/run/ados/video-streams.json`).
fn live_streams_path() -> PathBuf {
    paths::run_path("video-streams")
}

/// The supervisor's video command socket (`/run/ados/video-cmd.sock`), the same
/// socket the plugin host forwards `video.source.set` to; the operator write
/// forwards `video.cameras.set` here. The supervisor is the config-write +
/// restart authority.
fn video_cmd_sock() -> PathBuf {
    paths::run_path("video-cmd")
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
/// Merges the declared legs (`cameras[]`, or the legacy single
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
    let text = std::fs::read_to_string(config_path()).unwrap_or_default();
    let cfg = RosterVideoConfig::from_text(&text);
    // The camera roster is a companion-node concept. A ground station carries no
    // onboard camera, so it serves an empty roster rather than a phantom `main`.
    if cfg.is_ground_station() {
        return Vec::new();
    }
    let discovered = load_discovered(&discovered_path());
    let declared = declared_legs(&cfg, !discovered.is_empty());
    let live = load_live(&live_streams_path());
    let mut rows = build_roster(&declared, &discovered, &live);
    attach_entry_keys(&mut rows, &entries_from(&text));
    rows
}

/// The valid `cameras[]` entries of the config text as raw JSON objects keyed
/// by id, so the roster echoes every key exactly as configured.
fn entries_from(text: &str) -> HashMap<String, Value> {
    ados_config::cameras::camera_entries(text)
        .valid
        .into_iter()
        .filter_map(|(_, raw, c)| serde_json::to_value(&raw).ok().map(|v| (c.id, v)))
        .collect()
}

/// Keys a roster row carries that are runtime state, not `cameras[]` config:
/// a GET row echoed back as a PUT entry must not persist them.
const ROSTER_ONLY_KEYS: [&str; 3] = ["state", "live", "device_path"];

/// Drop [`ROSTER_ONLY_KEYS`] from every PUT entry.
fn strip_roster_only(cameras: &mut [Value]) {
    for obj in cameras.iter_mut().filter_map(Value::as_object_mut) {
        for key in ROSTER_ONLY_KEYS {
            obj.remove(key);
        }
    }
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
/// no non-primary leg claiming the reserved `main` id, and the shared per-entry
/// `cameras[]` check), then forwards a `video.cameras.set` op to the
/// supervisor's video command socket. The supervisor merges each operator leg
/// onto the existing entry of the same id (keys the body omits are kept, an
/// explicit `null` clears a key), preserves plugin-declared legs, and restarts
/// the video pipeline. A validation failure is a 400; an unreachable supervisor
/// a 503; a saved-but-not-restarted result a 502.
pub async fn put_video_cameras(Json(mut body): Json<PutCamerasBody>) -> Response {
    // The camera roster is a companion-node surface; a ground station has no
    // onboard camera to manage, so the write does not apply there.
    if RosterVideoConfig::load_from(&config_path()).is_ground_station() {
        return detail(
            StatusCode::NOT_FOUND,
            "the camera roster is not available on a ground station",
        );
    }
    strip_roster_only(&mut body.cameras);
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
        // The pipeline always streams the primary leg (it carries the radio
        // stream), so a disabled primary would be saved and then ignored.
        if i == primary_idx && obj.get("enabled") == Some(&Value::Bool(false)) {
            return Err(
                "the primary camera cannot be disabled; it carries the radio stream".to_string(),
            );
        }
        if let Some(orientation) = obj.get("orientation").and_then(Value::as_str) {
            if !orientation.is_empty() && !ORIENTATIONS.contains(&orientation) {
                return Err(format!("unknown orientation {orientation:?}"));
            }
        }
        // The image-transform `rotation` (distinct from the coarse-mount
        // `orientation` metadata) must be one of the four supported clockwise
        // values; mirrors the encoder-config schema so a bad value is a clear
        // 400 before the socket round-trip.
        if let Some(rot) = obj.get("rotation") {
            let bad = match rot {
                Value::Number(n) => !matches!(n.as_i64(), Some(0 | 90 | 180 | 270)),
                _ => true, // absent is fine, but any non-integer value is not
            };
            if bad {
                return Err("camera rotation must be one of 0, 90, 180, 270".to_string());
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
        if let Err(e) = ados_config::cameras::check_entry_json(leg) {
            return Err(format!("camera {id:?} is invalid: {e}"));
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

/// Send one newline-terminated JSON request to the video command socket and read one
/// newline-terminated JSON reply. `sock` is injectable so a test points it at a stub.
async fn video_cmd(request: &Value, sock: &Path) -> VideoCmd {
    /// The supervisor persists the leg list and restarts the pipeline before it
    /// replies, so the deadline covers a pipeline restart.
    const VIDEO_CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    match crate::ipc::cmd::roundtrip_object(sock, request, VIDEO_CMD_TIMEOUT).await {
        Ok(map) => VideoCmd::Reply(map),
        Err(_) => VideoCmd::Unavailable,
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
        // "Saved" only when the supervisor says it persisted the list; any other
        // unrecognised failure says nothing about the config, so none is claimed.
        _ if reply.get("persisted") == Some(&Value::Bool(true)) => detail(
            StatusCode::BAD_GATEWAY,
            "camera config saved but the video pipeline restart failed",
        ),
        other => detail(
            StatusCode::BAD_GATEWAY,
            format!(
                "the video supervisor refused the camera config ({})",
                if other.is_empty() {
                    "no error code"
                } else {
                    other
                }
            ),
        ),
    }
}

/// The declared legs the roster reconciles: the explicit `cameras[]` list,
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
        rotation: 0,
        hflip: false,
        vflip: false,
        encoder: "auto".to_string(),
        keyframe_interval: 0,
        capture: false,
        format: Default::default(),
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

/// The discovery sidecar's camera entries exactly as the HAL enumeration wrote
/// them (one object per device: `name`, `type`, `device_path`, `width`,
/// `height`, `capabilities`, `hardware_role`, `match`), for the routes that list
/// what is plugged in rather than reconcile it into the roster. Empty on any
/// read/parse failure, like [`load_discovered`].
pub(crate) fn discovered_camera_entries() -> Vec<Map<String, Value>> {
    let Ok(text) = std::fs::read_to_string(discovered_path()) else {
        return Vec::new();
    };
    let Ok(Value::Object(mut doc)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    match doc.remove("cameras") {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod entry_tests;
