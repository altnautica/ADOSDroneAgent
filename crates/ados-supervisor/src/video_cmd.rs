//! Video-source command socket.
//!
//! The plugin host forwards `video.source.set` here (the gpio / radio command-
//! socket precedent). The supervisor is the config-write + restart authority: it
//! persists the top-level `cameras` list under the config flock (0600, euid-0) and restarts
//! `ados-video.service` (and a running `ados-vision.service`, which owns the
//! captured cameras) so both resolve + serve the new source list.
//! One newline-JSON request → one newline-JSON response per connection.
//!
//! A driver plugin (e.g. a smart-camera / optical-pod driver) calls the plugin
//! `ctx.video.set_source([...])` facade after it negotiates its feeds, so the
//! operator never hand-types an RTSP URL. The host never writes the
//! config itself (it runs sandboxed); the privileged write lives here.

use std::path::Path;

use ados_protocol::ipc::{bind_command_socket, serve_rpc};
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::bind;
use crate::process_manager::{self, ProcessManager};

/// Command socket the supervisor serves and the plugin host forwards
/// `video.source.set` to. Hardcoded to the canonical run dir (mirrors the gpio
/// command socket) — the video pipeline only runs on a root install under
/// `/run/ados`; a rootless dev host has no pipeline to reconfigure.
pub const VIDEO_CMD_SOCK: &str = "/run/ados/video-cmd.sock";

/// The video pipeline unit the supervisor restarts after a source-list change.
/// ados-video resolves its leg list once at startup, so a *restart* (not a
/// reload) is what makes a new source list take effect.
const ADOS_VIDEO_UNIT: &str = "ados-video.service";

/// The vision engine unit. It owns the cameras marked `capture: true` and
/// resolves them once at startup too, so it is restarted with the pipeline.
const ADOS_VISION_UNIT: &str = "ados-vision.service";

/// Restart both camera consumers after a camera-list write, the engine first.
///
/// A camera that changed owner must be released before its new owner opens
/// it: when capture turns off, the engine still holds the device, and a video
/// encoder started first would find it busy. So the engine goes first. With
/// captured cameras declared it is (re)started whatever its state; without any
/// it is restarted only if running, and a restarted engine with no work left
/// exits, releasing every device before the pipeline comes back.
///
/// The reply's `ok` follows the video pipeline, which serves every leg; the
/// engine's restart is reported beside it.
async fn restart_camera_consumers(pm: &dyn ProcessManager, captured: usize) -> (bool, bool) {
    let vision = if captured > 0 {
        pm.restart(ADOS_VISION_UNIT).await
    } else {
        pm.try_restart(ADOS_VISION_UNIT).await
    };
    let video = pm.restart(ADOS_VIDEO_UNIT).await;
    (video, vision)
}

/// Cap a request so a peer that never sends a newline cannot grow the buffer
/// unbounded. A camera list is small; 64 KiB is generous.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Serve the video-source command socket until `shutdown` fires. A bind that
/// fails is retried on the fixed [`bind::control::SOCKET_BIND_RETRY`] interval
/// until it succeeds (or the supervisor shuts down): a socket given up on at
/// boot would leave every camera-driver plugin unable to configure its feeds
/// until someone restarted the supervisor.
pub async fn run(mut shutdown: watch::Receiver<bool>) {
    let pm = process_manager::select();
    let listener = loop {
        match bind_command_socket(VIDEO_CMD_SOCK, 0o660) {
            Ok(l) => break l,
            Err(e) => {
                tracing::warn!(error = %e, "video command socket bind failed; retrying");
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(bind::control::SOCKET_BIND_RETRY) => {}
                }
            }
        }
    };
    // The shared helper applied 0o660, group-owned the socket to `ados-operator`
    // and admits only root and operator-group peers. The plugin host runs as
    // root and forwards `video.source.set` behind its capability gate; a plugin
    // process itself cannot open this socket.
    tracing::info!(path = VIDEO_CMD_SOCK, "video command socket listening");

    let serve = serve_rpc(listener, MAX_REQUEST_BYTES, move |req: Vec<u8>| {
        let pm = pm.clone();
        async move {
            let resp = dispatch(&req, pm.as_ref()).await;
            serde_json::to_vec(&resp)
                .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
        }
    });

    // serve_rpc loops forever; the shutdown watch drops it on teardown.
    tokio::select! {
        _ = serve => {}
        _ = shutdown.changed() => {}
    }
    let _ = std::fs::remove_file(VIDEO_CMD_SOCK);
    tracing::info!("video command socket stopped");
}

/// Parse + route one request. Two ops persist the camera list and restart the
/// video pipeline: `video.source.set` (a driver plugin declaring its own feeds,
/// attributed to the plugin) and `video.cameras.set` (the operator's Cameras
/// surface write, attributed to `operator`). Both merge by owner so an operator
/// write preserves plugin legs and a plugin write preserves operator legs.
/// Pure-parse / validation errors return a structured error, never panic. Kept
/// small + testable: the config write + restart are the only side effects, both
/// driven through the injected [`ProcessManager`] and the config path constants.
async fn dispatch(req: &[u8], pm: &dyn ProcessManager) -> Value {
    let parsed: Value = match serde_json::from_slice(req) {
        Ok(v) => v,
        Err(_) => return json!({"ok": false, "error": "E_PARSE"}),
    };
    let op = parsed.get("op").and_then(Value::as_str).unwrap_or("");
    // The owner attributed to the incoming legs. A `video.cameras.set` (the
    // operator surface) is always the operator. A `video.source.set` (a driver
    // plugin) carries the plugin id the plugin host stamps, or the generic
    // `plugin` bucket when none was stamped, and can never write as the operator:
    // the merge lets only the operator replace the operator's legs.
    let owner = match op {
        "video.cameras.set" => bind::keys::OPERATOR_OWNER,
        "video.source.set" => parsed
            .get("owner")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("plugin"),
        _ => return json!({"ok": false, "error": "E_UNKNOWN_OP", "op": op}),
    };
    if op == "video.source.set" && owner == bind::keys::OPERATOR_OWNER {
        return json!({
            "ok": false, "error": "E_ARGS",
            "reason": "owner_conflict: a plugin source write cannot claim the operator owner",
        });
    }
    let Some(cameras) = parsed.get("cameras").filter(|c| c.is_array()) else {
        return json!({"ok": false, "error": "E_ARGS", "reason": "cameras must be an array"});
    };
    let legs = cameras.as_array().expect("filtered on is_array");
    if legs.is_empty() {
        return json!({"ok": false, "error": "E_ARGS", "reason": "cameras must not be empty"});
    }
    // Validate every leg. A leg with no id/source cannot be served; a leg id
    // becomes a mediamtx path + a WHEP URL segment, so it must be path-safe
    // (alphanumeric / dash / underscore) and unique — a bad char or a duplicate
    // would corrupt the mediamtx config and wedge the whole pipeline. Reject the
    // whole list rather than write a half-usable config (never
    // advertise a stream the pipeline cannot actually serve).
    let mut seen = std::collections::HashSet::new();
    for leg in legs {
        let id = leg.get("id").and_then(Value::as_str).unwrap_or("");
        let source = leg.get("source").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() || source.is_empty() {
            return json!({
                "ok": false,
                "error": "E_ARGS",
                "reason": "each camera needs a non-empty id and source",
            });
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return json!({
                "ok": false, "error": "E_ARGS",
                "reason": format!("camera id {id:?} has an unsafe character"),
            });
        }
        if !seen.insert(id) {
            return json!({
                "ok": false, "error": "E_ARGS",
                "reason": format!("duplicate camera id {id:?}"),
            });
        }
    }
    // The primary leg (the first with role "primary", else the first leg) is
    // always resolved to the reserved "main" path. A NON-primary leg that also
    // declares id "main" would resolve to a SECOND leg at "main" and overwrite
    // the primary path in the mediamtx config — wedging the whole primary
    // pipeline (video, WFB, cloud relay, and the vision tap all key on "main").
    // The submitted-id uniqueness check above cannot catch this because the two
    // ids differ BEFORE resolution renames the primary. Reject the collision.
    let primary_idx = legs
        .iter()
        .position(|l| l.get("role").and_then(Value::as_str) == Some("primary"))
        .unwrap_or(0);
    for (i, leg) in legs.iter().enumerate() {
        let id = leg.get("id").and_then(Value::as_str).unwrap_or("");
        if i != primary_idx && id == "main" {
            return json!({
                "ok": false, "error": "E_ARGS",
                "reason": "a non-primary camera cannot use the reserved id \"main\"",
            });
        }
    }

    // Persist `cameras` under the config flock (0600, euid-0), merging by
    // owner so this write preserves the other owners' legs, then restart the
    // video pipeline so it resolves + serves the new source list. A write that
    // names another owner's leg, or a plugin primary over the operator's, is
    // refused whole with nothing written.
    match bind::keys::persist_video_cameras(
        Path::new(bind::CONFIG_YAML),
        Path::new(bind::CONFIG_LOCK_PATH),
        cameras,
        owner,
    ) {
        bind::keys::CamerasPersist::Written => {}
        bind::keys::CamerasPersist::Refused(reason) => {
            return json!({"ok": false, "error": "E_ARGS", "reason": reason});
        }
        bind::keys::CamerasPersist::Failed => {
            return json!({"ok": false, "error": "E_PERSIST"});
        }
    }
    // The new sources are only LIVE once the pipeline restarts, so `ok` reflects
    // the actual restart (not just the config write). A restart failure is a
    // real failure the caller logs + retries — the config is saved (persisted:
    // true) and will apply on the next start, but the streams are not live yet.
    let captured = std::fs::read_to_string(bind::CONFIG_YAML)
        .map(|t| ados_config::cameras::declared_cameras_from(&t).captured)
        .unwrap_or(0);
    let (restarted, vision_restarted) = restart_camera_consumers(pm, captured).await;
    json!({"ok": restarted, "count": legs.len(), "persisted": true, "restarted": restarted, "vision_restarted": vision_restarted})
}

#[cfg(test)]
mod tests;
