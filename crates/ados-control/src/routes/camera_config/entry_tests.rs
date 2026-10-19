use super::*;

const FULL: &str = "cameras:\n  - id: front\n    source: rtsp://sim/front\n    role: primary\n    capture: true\n    format: gray8\n    exposure_us: 4000\n    gain: 2.5\n    trigger: hub\n    trigger_channel: 1\n    keyframe_interval: 15\n    rotation: 180\n    hflip: true\n    vflip: false\n    encoder: software\n";

fn rows_for(text: &str) -> Vec<Value> {
    let mut rows = build_roster(
        &RosterVideoConfig::from_text(text).cameras,
        &[],
        &HashMap::new(),
    );
    attach_entry_keys(&mut rows, &entries_from(text));
    rows
}

#[test]
fn roster_rows_carry_every_entry_key_raw() {
    let rows = rows_for(FULL);
    let r = &rows[0];
    assert_eq!(r["capture"], true);
    assert_eq!(r["format"], "gray8");
    assert_eq!(r["exposure_us"], 4000);
    assert_eq!(r["gain"], 2.5);
    assert_eq!(r["trigger"], "hub");
    assert_eq!(r["trigger_channel"], 1);
    assert_eq!(r["keyframe_interval"], 15);
    assert_eq!(r["rotation"], 180);
    assert_eq!(r["hflip"], true);
    assert_eq!(r["vflip"], false);
    assert_eq!(r["encoder"], "software");
    // GET → PUT: the row as served validates as a PUT entry.
    assert_eq!(validate_cameras(&rows), Ok(()));
}

#[test]
fn an_echoed_row_drops_its_roster_only_keys() {
    let mut rows = rows_for(FULL);
    assert!(rows[0].get("state").is_some());
    strip_roster_only(&mut rows);
    for key in ROSTER_ONLY_KEYS {
        assert!(rows[0].get(key).is_none(), "{key} would persist");
    }
    // Config keys survive the strip.
    assert_eq!(rows[0]["exposure_us"], 4000);
    assert_eq!(rows[0]["source"], "rtsp://sim/front");
}

#[test]
fn absent_entry_keys_are_null() {
    let rows = rows_for("cameras:\n  - id: front\n    source: /dev/video0\n");
    for key in ENTRY_KEYS {
        assert_eq!(rows[0][key], Value::Null, "{key}");
    }
}

#[test]
fn a_put_entry_failing_the_shared_check_is_rejected_with_its_id() {
    let err =
        validate_cameras(&[json!({"id": "front", "source": "/dev/video0", "format": "mjpeg"})])
            .unwrap_err();
    assert!(err.contains("\"front\""), "{err}");
    let err = validate_cameras(&[
        json!({"id": "front", "source": "/dev/video0", "keyframe_interval": 99}),
    ])
    .unwrap_err();
    assert!(err.contains("\"front\""), "{err}");
    // An explicit null (a clear) passes.
    assert_eq!(
        validate_cameras(&[json!({"id": "front", "source": "/dev/video0", "trigger": null})]),
        Ok(())
    );
}
