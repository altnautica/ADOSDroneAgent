//! Video pipeline status reads: **`GET /api/video`** and **`GET /api/video/cameras`**.
//!
//! - **`GET /api/video`** reports what this node can observe of its pipeline:
//!   the cameras the HAL enumeration found, whether mediamtx has a live
//!   publisher on the primary `main` path, which media tools are installed, and
//!   the WHEP/HLS URLs. The URLs address `main`, so they are advertised only when
//!   `main` itself is live (`ready` with a `source`). A different path being
//!   ready never stands in for it, and an absent `main` is not ready.
//! - **`GET /api/video/cameras`** lists the same discovered devices in the
//!   compact enumeration shape.
//!
//! Both read the discovery sidecar the HAL enumeration writes, the same source
//! the camera roster reconciles. `assignments` is always `{}`: role bindings
//! live in the roster (`GET /api/video/roster`), not here. Guaranteed 200.

use ados_video::mediamtx::DEFAULT_WEBRTC_PORT;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::mediamtx_probe::{main_path, path_ready, read_paths_list};
use crate::routes::camera_config::discovered_camera_entries;
use crate::routes::status::{binary_path, VIDEO_DEPENDENCIES};
use crate::routes::status_full::mediamtx_whep_serving;

/// The primary stream's WHEP endpoint on this front (addresses mediamtx `main`).
const PRIMARY_WHEP_URL: &str = "/whep";
/// The primary stream's HLS playlist on this front.
const PRIMARY_HLS_URL: &str = "/hls/main/index.m3u8";

/// `GET /api/video` → `{state, cameras, mediamtx, whep_url, hls_url, dependencies}`.
pub async fn get_video_status() -> Json<Value> {
    let list = read_paths_list().await;
    // An answering management API proves mediamtx is up. When it does not answer
    // (down, or the ground-station mediamtx credentialing it), a bound WHEP
    // endpoint is the credential-free liveness signal. Neither proves frames flow.
    let running = list.is_some() || mediamtx_whep_serving().await;
    Json(build_video_status(
        list.as_ref(),
        running,
        discovered_camera_entries(),
        dependencies(),
    ))
}

/// The `GET /api/video` body from the probe results. Pure, so the readiness rule
/// is testable without a live mediamtx.
fn build_video_status(
    paths_list: Option<&Value>,
    mediamtx_running: bool,
    cameras: Vec<Map<String, Value>>,
    dependencies: Value,
) -> Value {
    let cameras = json!({"cameras": cameras, "assignments": {}});
    match paths_list.and_then(main_path).filter(|p| path_ready(p)) {
        Some(main) => json!({
            "state": "running",
            "cameras": cameras,
            "mediamtx": {
                "running": true,
                "stream_name": "main",
                "ready": true,
                "tracks": main.get("tracks").cloned().unwrap_or_else(|| json!([])),
                "readers": main.get("readers").and_then(Value::as_array).map_or(0, Vec::len),
                "webrtc_port": DEFAULT_WEBRTC_PORT,
            },
            "whep_url": PRIMARY_WHEP_URL,
            "hls_url": PRIMARY_HLS_URL,
            "dependencies": dependencies,
        }),
        None => json!({
            "state": "not_initialized",
            "cameras": cameras,
            "mediamtx": {"running": mediamtx_running, "webrtc_port": DEFAULT_WEBRTC_PORT},
            "whep_url": Value::Null,
            "hls_url": Value::Null,
            "dependencies": dependencies,
        }),
    }
}

/// `{name: {found, path}}` for each media tool, resolved on `PATH`.
fn dependencies() -> Value {
    let mut out = Map::new();
    for name in VIDEO_DEPENDENCIES {
        let path = binary_path(name);
        out.insert(
            name.to_string(),
            json!({
                "found": path.is_some(),
                "path": path.map(|p| p.to_string_lossy().into_owned()),
            }),
        );
    }
    Value::Object(out)
}

/// `GET /api/video/cameras` →
/// `{cameras: [{device_path, type, label, width, height}], assignments: {}}`.
pub async fn get_video_camera_list() -> Json<Value> {
    Json(camera_list(&discovered_camera_entries()))
}

/// Project discovered devices into the compact enumeration rows. A field the
/// sidecar lacks reads as the HAL default (`""` / `0`).
fn camera_list(entries: &[Map<String, Value>]) -> Value {
    let text = |c: &Map<String, Value>, key: &str| {
        c.get(key)
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or_else(|| json!(""))
    };
    let number = |c: &Map<String, Value>, key: &str| {
        c.get(key)
            .filter(|v| v.is_number())
            .cloned()
            .unwrap_or_else(|| json!(0))
    };
    let cameras: Vec<Value> = entries
        .iter()
        .map(|c| {
            json!({
                "device_path": text(c, "device_path"),
                "type": text(c, "type"),
                "label": text(c, "name"),
                "width": number(c, "width"),
                "height": number(c, "height"),
            })
        })
        .collect();
    json!({"cameras": cameras, "assignments": {}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ready_secondary_path_does_not_make_the_primary_stream_running() {
        let list = json!({"items": [
            {"name": "eo_wide", "ready": true, "source": {"type": "rpiCameraSource"},
             "tracks": ["H264"], "readers": []},
        ]});
        let body = build_video_status(Some(&list), true, vec![], json!({}));
        assert_eq!(body["state"], "not_initialized");
        assert_eq!(body["whep_url"], Value::Null);
        assert_eq!(body["hls_url"], Value::Null);
        assert_eq!(body["mediamtx"]["running"], true);
    }

    #[test]
    fn a_live_main_path_is_running_with_the_primary_urls() {
        let list = json!({"items": [
            {"name": "ir", "ready": false, "source": null},
            {"name": "main", "ready": true, "source": {"type": "rtspSession"},
             "tracks": ["H264"], "readers": [{"type": "webRTCSession"}]},
        ]});
        let body = build_video_status(Some(&list), true, vec![], json!({}));
        assert_eq!(body["state"], "running");
        assert_eq!(body["whep_url"], "/whep");
        assert_eq!(body["hls_url"], "/hls/main/index.m3u8");
        assert_eq!(body["mediamtx"]["readers"], 1);
        assert_eq!(body["mediamtx"]["tracks"], json!(["H264"]));
    }
}
