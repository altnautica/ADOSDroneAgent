use super::*;
use serde_json::json;

fn legs(root: &Mapping) -> Vec<serde_json::Value> {
    root.get("cameras")
        .and_then(Value::as_sequence)
        .map(|s| {
            s.iter()
                .filter_map(|v| serde_json::to_value(v).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn seeded() -> Mapping {
    let mut root = Mapping::new();
    apply_video_cameras(
        &mut root,
        &json!([{
            "id": "front", "source": "/dev/video0", "role": "primary",
            "capture": true, "format": "gray8", "exposure_us": 4000, "gain": 2.5,
            "trigger": "hub", "trigger_channel": 1, "keyframe_interval": 15,
            "rotation": 180, "hflip": true, "vflip": false, "encoder": "software",
            "future_key": {"nested": 1}
        }]),
        OPERATOR_OWNER,
    )
    .unwrap();
    root
}

#[test]
fn an_operator_write_keeps_keys_the_body_omits() {
    let mut root = seeded();
    apply_video_cameras(
        &mut root,
        &json!([{"id": "front", "source": "/dev/video0", "role": "primary", "gain": 4.0}]),
        OPERATOR_OWNER,
    )
    .unwrap();
    let leg = &legs(&root)[0];
    assert_eq!(leg["gain"], 4.0);
    for key in [
        "capture",
        "format",
        "exposure_us",
        "trigger",
        "trigger_channel",
        "keyframe_interval",
        "rotation",
        "hflip",
        "vflip",
        "encoder",
        "future_key",
    ] {
        assert!(leg.get(key).is_some(), "{key} dropped");
    }
    assert_eq!(leg["future_key"], json!({"nested": 1}));
}

#[test]
fn an_explicit_null_clears_a_key() {
    let mut root = seeded();
    apply_video_cameras(
        &mut root,
        &json!([{"id": "front", "source": "/dev/video0", "exposure_us": null, "trigger": null}]),
        OPERATOR_OWNER,
    )
    .unwrap();
    let leg = &legs(&root)[0];
    assert!(leg.get("exposure_us").is_none());
    assert!(leg.get("trigger").is_none());
    assert_eq!(leg["gain"], 2.5);
}

#[test]
fn a_merged_entry_that_fails_the_shared_check_is_refused() {
    let mut root = seeded();
    let err = apply_video_cameras(
        &mut root,
        &json!([{"id": "front", "source": "/dev/video0", "exposure_us": 0}]),
        OPERATOR_OWNER,
    )
    .unwrap_err();
    assert!(err.contains("\"front\""), "{err}");
    assert_eq!(legs(&root)[0]["exposure_us"], 4000);
}

#[test]
fn a_plugin_write_replaces_its_legs_whole() {
    let mut root = Mapping::new();
    apply_video_cameras(
        &mut root,
        &json!([{"id": "ir", "source": "rtsp://pod/ir", "gain": 1.0}]),
        "com.example.pod",
    )
    .unwrap();
    apply_video_cameras(
        &mut root,
        &json!([{"id": "ir", "source": "rtsp://pod/ir"}]),
        "com.example.pod",
    )
    .unwrap();
    assert!(legs(&root)[0].get("gain").is_none());
}
