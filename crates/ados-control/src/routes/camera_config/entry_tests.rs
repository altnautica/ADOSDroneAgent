use super::*;

fn config_with(entry: &str) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(f.path(), format!("cameras:\n{entry}")).unwrap();
    f
}

const FULL: &str = "  - id: front\n    source: rtsp://sim/front\n    role: primary\n    capture: true\n    format: gray8\n    exposure_us: 4000\n    gain: 2.5\n    trigger: hub\n    trigger_channel: 1\n    keyframe_interval: 15\n    rotation: 180\n    hflip: true\n    vflip: false\n    encoder: software\n";

#[test]
fn roster_rows_carry_every_entry_key_raw() {
    let cfg = config_with(FULL);
    let legs = RosterVideoConfig::load_from(cfg.path()).cameras;
    let mut rows = build_roster(&legs, &[], &HashMap::new());
    attach_entry_keys(&mut rows, &load_entries(cfg.path()));
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
fn absent_entry_keys_are_null() {
    let cfg = config_with("  - id: front\n    source: /dev/video0\n");
    let legs = RosterVideoConfig::load_from(cfg.path()).cameras;
    let mut rows = build_roster(&legs, &[], &HashMap::new());
    attach_entry_keys(&mut rows, &load_entries(cfg.path()));
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
