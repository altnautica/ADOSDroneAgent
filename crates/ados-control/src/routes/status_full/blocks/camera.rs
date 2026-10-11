//! Camera status: the LOCAL camera-presence + USB-recovery surface.

use super::super::*;
use ados_protocol::paths;
use serde_json::json;
use serde_json::Value;
use std::path::Path;

/// The freshness window (seconds) for the camera-state sidecar: the registry
/// window for `camera-state`. A reading older than this is dropped.
pub(crate) const CAMERA_STATE_FRESH_S: f64 = paths::CAMERA_STATE_MAX_AGE_MS as f64 / 1000.0;

/// The freshness window (seconds) for the USB-recovery sidecar: the registry
/// window for `camera-usb-recovery`.
pub(crate) const CAMERA_RECOVERY_FRESH_S: f64 =
    paths::CAMERA_USB_RECOVERY_MAX_AGE_MS as f64 / 1000.0;

/// Camera presence + USB-recovery state for the LOCAL status surface, folded into
/// the payload only when the sidecars are fresh + valid (absent otherwise).
///
/// Reads `/run/ados/camera-state.json` (the `cameraState` enum) and
/// `/run/ados/camera-usb-recovery.json` (the recovery block), staleness-gated on each
/// sidecar's `updated_at_unix`. Shared with the `/api/status` route, which folds the
/// same camera keys into its body.
pub(crate) fn read_camera_status(run_dir: &Path) -> Vec<(String, Value)> {
    read_camera_status_in(run_dir, now_unix_secs())
}

/// The path-injectable core of [`read_camera_status`]: read the two camera sidecars
/// under `run_dir`, staleness-gated against `now`. Pure (no env reads), so a test
/// drives it with a tempdir without racing the process-global `ADOS_RUN_DIR`.
pub(crate) fn read_camera_status_in(run_dir: &Path, now: f64) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = Vec::new();

    if let Some(camera) = read_sidecar_object(&run_dir.join("camera-state.json")) {
        // Best-effort schema-drift signal (never reject): warn on a producer/reader
        // version mismatch, then read anyway. The writer const lives in the
        // ados-video crate, so compare against the shared registry.
        let got = camera.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
        if let Some(ours) = ados_protocol::contracts::sidecar_version("camera-state") {
            ados_protocol::sidecar::check_sidecar_version("camera-state", got, ours);
        }
        // Discovery alone is not health: a detected camera over a failed
        // pipeline reads as `error`, so the cockpit warns instead of showing a
        // confident camera pill over a dead pane.
        let doc = Value::Object(camera);
        if let Some(state) =
            ados_video::camera_state::effective_state(&doc, now, CAMERA_STATE_FRESH_S)
        {
            out.push(("cameraState".to_string(), json!(state.as_str())));
        }
    }

    if let Some(rec) = read_sidecar_object(&run_dir.join("camera-usb-recovery.json")) {
        // Best-effort schema-drift signal (never reject): warn on a producer/reader
        // version mismatch, then read anyway. The writer const lives in the
        // ados-supervisor crate, so compare against the shared registry.
        let got = rec.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
        if let Some(ours) = ados_protocol::contracts::sidecar_version("camera-usb-recovery") {
            ados_protocol::sidecar::check_sidecar_version("camera-usb-recovery", got, ours);
        }
        if sidecar_fresh(&rec, now, CAMERA_RECOVERY_FRESH_S) {
            let state = rec.get("camera_usb_recovery_state").and_then(Value::as_str);
            if let Some(state) = state {
                if matches!(
                    state,
                    "idle"
                        | "monitoring"
                        | "rebinding"
                        | "port_cycling"
                        | "hub_resetting"
                        | "needs_hub_reset"
                        | "guard_blocked"
                        | "retrying"
                ) {
                    out.push((
                        "cameraUsbRecovery".to_string(),
                        json!({
                            "state": state,
                            "case": rec.get("case").cloned().unwrap_or(Value::Null),
                            "attempts": rec.get("attempts").cloned().unwrap_or(json!(0)),
                            "cooldownSeconds": rec.get("cooldown_s").cloned().unwrap_or(json!(0)),
                            "cameraPresent": json_truthy_default_false(&rec, "camera_present"),
                            "expected": json_truthy_default_false(&rec, "expected"),
                            "pppsCapable": json_truthy_default_false(&rec, "ppps_capable"),
                            "powerContention": json_truthy_default_false(&rec, "power_contention"),
                            "contentionPeer": rec.get("contention_peer").cloned().unwrap_or(Value::Null),
                        }),
                    ));
                }
            }
        }
    }
    out
}
