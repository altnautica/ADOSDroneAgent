//! Video-source command socket.
//!
//! The plugin host forwards `video.source.set` here (the gpio / radio command-
//! socket precedent). The supervisor is the config-write + restart authority: it
//! persists `video.cameras` under the config flock (0600, euid-0) and restarts
//! `ados-video.service` so the pipeline resolves + serves the new source list.
//! One newline-JSON request → one newline-JSON response per connection.
//!
//! A driver plugin (e.g. a smart-camera / optical-pod driver) calls the plugin
//! `ctx.video.set_source([...])` facade after it negotiates its feeds, so the
//! operator never hand-types an RTSP URL (Rule 26). The host never writes the
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

/// Cap a request so a peer that never sends a newline cannot grow the buffer
/// unbounded. A camera list is small; 64 KiB is generous.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Serve the video-source command socket until `shutdown` fires or the listener
/// bind fails. Best-effort: a bind failure logs and returns (the feature is
/// simply unavailable on this host), never aborts the supervisor.
pub async fn run(mut shutdown: watch::Receiver<bool>) {
    let pm = process_manager::select();
    let listener = match bind_command_socket(VIDEO_CMD_SOCK, 0o660) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "video command socket bind failed");
            return;
        }
    };
    // bind_command_socket already applied 0o660; group-own it to `ados` so the
    // sandboxed plugin host (running as the `ados` group) can write it.
    set_socket_perms(Path::new(VIDEO_CMD_SOCK));
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
    // operator surface) defaults to `operator`; a `video.source.set` (a driver
    // plugin) defaults to the generic `plugin` bucket when the caller did not
    // stamp its plugin id, so a plugin write never collapses into the operator's
    // legs. A caller-supplied `owner` (the plugin host stamps its plugin id, the
    // operator route stamps `operator`) always wins.
    let default_owner = match op {
        "video.cameras.set" => "operator",
        "video.source.set" => "plugin",
        _ => return json!({"ok": false, "error": "E_UNKNOWN_OP", "op": op}),
    };
    let owner = parsed
        .get("owner")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_owner);
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
    // whole list rather than write a half-usable config (Rule 44 — never
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

    // Persist video.cameras under the config flock (0600, euid-0), merging by
    // owner so this write preserves the other party's legs, then restart the
    // video pipeline so it resolves + serves the new source list.
    let persisted = bind::keys::persist_video_cameras(
        Path::new(bind::CONFIG_YAML),
        Path::new(bind::CONFIG_LOCK_PATH),
        cameras,
        owner,
    );
    if !persisted {
        return json!({"ok": false, "error": "E_PERSIST"});
    }
    // The new sources are only LIVE once the pipeline restarts, so `ok` reflects
    // the actual restart (not just the config write). A restart failure is a
    // real failure the caller logs + retries — the config is saved (persisted:
    // true) and will apply on the next start, but the streams are not live yet.
    let restarted = pm.restart(ADOS_VIDEO_UNIT).await;
    json!({"ok": restarted, "count": legs.len(), "persisted": true, "restarted": restarted})
}

/// 0o660 + group-own to `ados` so the sandboxed plugin host in that group can
/// reach the trusted local plane. Best-effort; an absent group (a dev host) is a
/// quiet no-op. Linux-only.
#[cfg(target_os = "linux")]
fn set_socket_perms(sock_path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o660));
    match nix::unistd::Group::from_name("ados") {
        Ok(Some(g)) => {
            if let Err(err) = nix::unistd::chown(sock_path, None, Some(g.gid)) {
                tracing::debug!(error = %err, "chgrp video command socket failed");
            }
        }
        Ok(None) => tracing::debug!("ados group not present; leaving socket group as-is"),
        Err(err) => tracing::debug!(error = %err, "resolving ados group failed"),
    }
}

#[cfg(not(target_os = "linux"))]
fn set_socket_perms(_sock_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_manager::NullManager;

    /// A process manager that records the units it was asked to restart, so a
    /// dispatch test can assert the video service was (or was not) restarted
    /// without touching systemd.
    struct RecordingPm {
        restarted: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl ProcessManager for RecordingPm {
        async fn start(&self, _unit: &str) -> bool {
            true
        }
        async fn stop(&self, _unit: &str) -> bool {
            true
        }
        async fn restart(&self, unit: &str) -> bool {
            self.restarted.lock().unwrap().push(unit.to_string());
            true
        }
        async fn reset_failed(&self, _unit: &str) {}
        async fn is_active(&self, _unit: &str) -> bool {
            true
        }
        async fn mask(&self, _unit: &str) {}
        async fn unmask(&self, _unit: &str) {}
    }

    #[tokio::test]
    async fn unknown_op_is_rejected_without_a_restart() {
        let pm = RecordingPm {
            restarted: std::sync::Mutex::new(Vec::new()),
        };
        let resp = dispatch(br#"{"op":"gpio.output.set"}"#, &pm).await;
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"], "E_UNKNOWN_OP");
        assert!(pm.restarted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_json_is_rejected() {
        let resp = dispatch(b"not json", &NullManager).await;
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"], "E_PARSE");
    }

    #[tokio::test]
    async fn video_cameras_set_op_is_routed_not_unknown() {
        // The operator-surface op is accepted (reaches validation, an empty list
        // is an E_ARGS not E_UNKNOWN_OP), proving it is routed alongside
        // video.source.set without touching the real config.
        let resp = dispatch(
            br#"{"op":"video.cameras.set","owner":"operator","cameras":[]}"#,
            &NullManager,
        )
        .await;
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"], "E_ARGS");
    }

    #[tokio::test]
    async fn non_array_and_empty_and_incomplete_lists_are_rejected() {
        for body in [
            br#"{"op":"video.source.set","cameras":"main"}"#.as_slice(),
            br#"{"op":"video.source.set","cameras":[]}"#.as_slice(),
            br#"{"op":"video.source.set","cameras":[{"id":"main"}]}"#.as_slice(),
            // A leg id with a mediamtx-unsafe character (a space / slash).
            br#"{"op":"video.source.set","cameras":[{"id":"bad id","source":"rtsp://x/y"}]}"#
                .as_slice(),
            br#"{"op":"video.source.set","cameras":[{"id":"a/b","source":"rtsp://x/y"}]}"#
                .as_slice(),
            // Duplicate leg ids would collapse in the mediamtx path map.
            br#"{"op":"video.source.set","cameras":[{"id":"main","source":"rtsp://x/1"},{"id":"main","source":"rtsp://x/2"}]}"#
                .as_slice(),
            // A non-primary leg named "main" collides with the primary, which
            // always resolves to the reserved "main" path — the submitted ids
            // ("eo" vs "main") differ so the uniqueness check above misses it.
            br#"{"op":"video.source.set","cameras":[{"id":"eo","role":"primary","source":"rtsp://x/zoom"},{"id":"main","role":"ir","source":"rtsp://x/ir"}]}"#
                .as_slice(),
        ] {
            let resp = dispatch(body, &NullManager).await;
            assert_eq!(resp["ok"], false, "body should reject: {body:?}");
            assert_eq!(resp["error"], "E_ARGS");
        }
    }

    // The valid-list path (persist + restart) writes the real /etc/ados config
    // and is exercised on-rig: the config merge is unit-tested in bind::keys and
    // the restart is a single call after a successful write. A dispatch test of
    // it here would touch the host's real config path, so it stays bench-gated.
}
