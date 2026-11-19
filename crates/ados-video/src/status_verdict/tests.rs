use super::*;

#[test]
fn a_ready_secondary_path_does_not_stand_in_for_an_absent_main() {
    let list = json!({"items": [
        {"name": "eo_wide", "ready": true, "source": {"type": "rpiCameraSource"}},
    ]});
    assert!(main_path(&list).is_none());
}

#[test]
fn main_is_found_by_name_wherever_it_is_listed() {
    let list = json!({"items": [
        {"name": "ir", "ready": true, "source": {"type": "rtspSource"}},
        {"name": "main", "ready": false, "source": null},
    ]});
    let main = main_path(&list).expect("main is listed");
    assert_eq!(main.get("name"), Some(&json!("main")));
    assert!(!path_ready(main));
}

#[test]
fn ready_requires_both_the_flag_and_a_publisher() {
    let ready = |v: Value| path_ready(v.as_object().unwrap());
    assert!(ready(
        json!({"ready": true, "source": {"type": "webRTCSession"}})
    ));
    // mediamtx can report ready on a path whose publisher just left.
    assert!(!ready(json!({"ready": true, "source": null})));
    assert!(!ready(json!({"ready": true})));
    assert!(!ready(
        json!({"ready": false, "source": {"type": "rtspSession"}})
    ));
}

#[test]
fn a_sidecar_dated_in_the_future_is_not_fresh() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("camera-state.json"),
        json!({"pipeline_state": "streaming", "updated_at_unix": 1_010.0}).to_string(),
    )
    .unwrap();
    assert!(!read_pipeline_outcome_in(dir.path(), 1_000.0).fresh);
    assert!(read_pipeline_outcome_in(dir.path(), 1_009.0).fresh);
}
