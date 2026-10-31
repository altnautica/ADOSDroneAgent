use super::detector::write_detector_block;
use super::*;

#[test]
fn npu_boards_match_across_soc_slug_and_display_name() {
    // A SoC string, a board slug, and a full display name all resolve.
    assert!(is_npu_board("rk3588"));
    assert!(is_npu_board("Radxa ROCK 5C Lite (RK3582)"));
    assert!(is_npu_board("rock-5c-lite"));
    assert!(is_npu_board("Orange Pi 5 Plus"));
    assert!(is_npu_board("rk3576"));
}

#[test]
fn non_npu_boards_do_not_match() {
    // The small-NPU / no-NPU boards the sidecar does not target.
    assert!(!is_npu_board("Raspberry Pi 4 Model B"));
    assert!(!is_npu_board("Raspberry Pi Compute Module 4"));
    assert!(!is_npu_board("rk3566"));
    // No accelerated vision backend exists for Jetson, so it must not be
    // provisioned (the RKNN runtime and sidecar cannot run there).
    assert!(!is_npu_board("NVIDIA Jetson Orin Nano"));
    assert!(!is_npu_board("tegra234"));
    assert!(!is_npu_board(""));
    assert!(!is_npu_board("unknown"));
}

#[test]
fn wheel_cp_tag_maps_supported_pythons_and_rejects_others() {
    // The 2.3.2 toolkit ships cp37–cp312 aarch64 wheels.
    assert_eq!(wheel_cp_tag(3, 11).as_deref(), Some("cp311"));
    assert_eq!(wheel_cp_tag(3, 12).as_deref(), Some("cp312"));
    assert_eq!(wheel_cp_tag(3, 10).as_deref(), Some("cp310"));
    assert_eq!(wheel_cp_tag(3, 7).as_deref(), Some("cp37"));
    // 3.13+ has no published wheel at this toolkit version → None (the
    // caller fails loudly with an actionable reason, never a silent skip).
    assert_eq!(wheel_cp_tag(3, 13), None);
    // A non-3 major never matches.
    assert_eq!(wheel_cp_tag(2, 7), None);
}

#[test]
fn wheel_filename_uses_the_abi_tag() {
    // Every cp tag >= 3.8 repeats the cp tag as the ABI tag.
    assert_eq!(
        wheel_filename("cp311"),
        "rknn_toolkit_lite2-2.3.2-cp311-cp311-manylinux_2_17_aarch64.manylinux2014_aarch64.whl"
    );
    // cp37 uses the cp37m ABI tag.
    assert_eq!(
        wheel_filename("cp37"),
        "rknn_toolkit_lite2-2.3.2-cp37-cp37m-manylinux_2_17_aarch64.manylinux2014_aarch64.whl"
    );
}

fn read_yaml(path: &Path) -> serde_norway::Value {
    serde_norway::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn write_detector_block_writes_and_preserves_other_keys() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "agent:\n  name: my-drone\nvision:\n  enabled: true\n  backend: rknn\n",
    )
    .unwrap();
    let labels = vec!["person".to_string(), "car".to_string()];
    let wrote = write_detector_block(
        &cfg,
        "com.example.coco",
        "/opt/ados/models/coco.rknn",
        &labels,
    )
    .unwrap();
    assert!(wrote, "a fresh config gets the detector block written");

    let y = read_yaml(&cfg);
    // Other keys survive.
    assert_eq!(y["agent"]["name"].as_str(), Some("my-drone"));
    assert_eq!(y["vision"]["enabled"].as_bool(), Some(true));
    assert_eq!(y["vision"]["backend"].as_str(), Some("rknn"));
    // The detector block is written with id/path/enabled/labels.
    let det = &y["vision"]["detector"];
    assert_eq!(det["model_id"].as_str(), Some("com.example.coco"));
    assert_eq!(
        det["model_path"].as_str(),
        Some("/opt/ados/models/coco.rknn")
    );
    assert_eq!(det["enabled"].as_bool(), Some(true));
    let classes = det["class_labels"].as_sequence().unwrap();
    assert_eq!(classes.len(), 2);
    assert_eq!(classes[0].as_str(), Some("person"));
}

#[test]
fn write_detector_block_never_clobbers_a_configured_detector() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "vision:\n  detector:\n    model_id: operator-choice\n    model_path: /custom.rknn\n",
    )
    .unwrap();
    let wrote = write_detector_block(&cfg, "auto-pick", "/auto.rknn", &[]).unwrap();
    assert!(!wrote, "an already-configured detector is left untouched");
    let y = read_yaml(&cfg);
    assert_eq!(
        y["vision"]["detector"]["model_id"].as_str(),
        Some("operator-choice")
    );
}

#[test]
fn write_detector_block_creates_config_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let wrote = write_detector_block(&cfg, "m", "/m.rknn", &[]).unwrap();
    assert!(wrote);
    assert!(cfg.exists());
    let y = read_yaml(&cfg);
    assert_eq!(y["vision"]["detector"]["model_id"].as_str(), Some("m"));
}

#[test]
fn urls_pin_the_toolkit_version_and_arch() {
    let w = wheel_url("cp311");
    assert!(w.contains("/v2.3.2/rknn-toolkit-lite2/packages/"));
    assert!(w.contains("cp311"));
    assert!(w.ends_with(".whl"));

    let lib = librknnrt_url();
    assert!(lib.contains("/v2.3.2/rknpu2/runtime/Linux/librknn_api/aarch64/librknnrt.so"));
    assert!(lib.starts_with("https://"));
}
