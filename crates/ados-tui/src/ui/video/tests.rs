use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::app::{App, Slot, TabId};
use crate::model::ProfileRoutes;
use crate::ui::tests::{draw, feed, test_app};

fn land(app: &mut App, id: ResourceId, value: Value) {
    app.core.res.insert(
        id.clone(),
        Slot {
            value: Some(value),
            error: None,
            code: None,
            error_body: None,
            at: Instant::now(),
        },
    );
    app.tabs.video.on_resource(&mut app.core, &id);
}

#[test]
fn relative_urls_resolve_against_the_target() {
    assert_eq!(
        absolute("http://192.168.1.50:8080/", "/whep"),
        "http://192.168.1.50:8080/whep"
    );
    assert_eq!(
        absolute("http://x:8080", "http://y:8889/main/whep"),
        "http://y:8889/main/whep"
    );
}

#[test]
fn video_renders_encoder_radio_cameras_latency_and_urls() {
    let mut app = test_app();
    let status = json!({
        "version": "0.99.108", "device_name": "ados-x", "profile": "drone", "paired": true,
        "mavlink": {"connected": false},
        "video": {"state": "running"},
        "access_urls": [{"kind": "video", "label": "viewer", "url": "http://ados-x.local:8889/main/"}]
    });
    feed(&mut app, status, ProfileRoutes::default(), Duration::ZERO);
    app.activate_tab(TabId::Video);
    land(
        &mut app,
        ResourceId::VideoConfig,
        json!({
            "radio": {"channel": 149, "band": "u-nii-3", "mcs_index": 1, "fec_k": 8, "fec_n": 12, "tx_power_dbm": 5, "preset": "conservative"},
            "encoder": {"bitrate_kbps": 4000, "width": 1280, "height": 720, "fps": 30, "codec": "h264"},
            "adaptive": {"available": true}
        }),
    );
    land(
        &mut app,
        ResourceId::VideoCameras,
        json!({"cameras": [
        {"device_path": "/dev/video0", "type": "csi", "label": "Main camera", "width": 1920, "height": 1080}
    ], "assignments": {}}),
    );
    land(
        &mut app,
        ResourceId::VideoLatency,
        json!({"latency_ms": 92.0, "ewma_ms": 88.4, "samples": 40, "source": "sei"}),
    );
    land(
        &mut app,
        ResourceId::SetupStatus,
        json!({"video": {"state": "running", "whep_url": "/whep", "hls_url": "/hls/main/index.m3u8"}}),
    );
    let text = draw(&mut app, 120, 40);
    for want in [
        "H264",
        "1280×720",
        "30 fps",
        "4000 kbit/s",
        "149 · u-nii-3",
        "MCS 1 · FEC 8/12",
        "/dev/video0",
        "Main camera",
        "1920×1080",
        "88 ms",
        "sei",
        "/whep",
        "/hls/main/index.m3u8",
        "http://ados-x.local:8889/main/whep",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}
