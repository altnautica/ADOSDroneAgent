use super::detector::{parse_fetch_output, write_detector_block, FetchOutcome, FetchedDetector};
use super::rknn::board_identity;
use super::*;

#[test]
fn rockchip_carriers_count_as_npu_boards_by_their_compatible_soc() {
    // Device-tree model + compatible as the kernel reports them; the model
    // alone names no SoC on these carriers.
    for (model, compatible) in [
        ("Radxa ROCK 5B+", "radxa,rock-5b-plus\0rockchip,rk3588\0"),
        (
            "Radxa CM4 IO Board",
            "radxa,cm4-io\0radxa,cm4\0rockchip,rk3588s\0",
        ),
        (
            "Orange Pi 5",
            "rockchip,rk3588s-orangepi-5\0rockchip,rk3588\0",
        ),
        ("Radxa ROCK 5C", "radxa,rock-5c\0rockchip,rk3588s\0"),
        (
            "Rockchip RK3576 EVB1 V10 Board",
            "rockchip,rk3576-evb1-v10\0rockchip,rk3576\0",
        ),
    ] {
        let id = board_identity(model, compatible);
        assert!(id.starts_with(model), "{id}");
        assert!(!id.contains('\0'), "{id:?}");
        assert!(is_npu_board(&id), "{id}");
        // The onnx vision build stays an ONNX-fallback choice on these boards.
        assert!(!crate::binaries::board_prefers_onnx_vision(&id), "{id}");
    }
    for (model, compatible) in [
        ("Radxa CM3 IO", "radxa,cm3-io\0radxa,cm3\0rockchip,rk3566\0"),
        (
            "Raspberry Pi 5 Model B Rev 1.0",
            "raspberrypi,5-model-b\0brcm,bcm2712\0",
        ),
        ("", ""),
    ] {
        assert!(!is_npu_board(&board_identity(model, compatible)), "{model}");
    }
    // A Pi 5 still gets the onnx build through its model string.
    let pi = board_identity(
        "Raspberry Pi 5 Model B Rev 1.0",
        "raspberrypi,5-model-b\0brcm,bcm2712\0",
    );
    assert!(crate::binaries::board_prefers_onnx_vision(&pi));
    assert_eq!(board_identity("rock-5c-lite\0", ""), "rock-5c-lite");
}

#[test]
fn npu_boards_match_across_soc_slug_and_display_name() {
    // A SoC string, a board slug, and a full display name all resolve.
    assert!(is_npu_board("rk3588"));
    assert!(is_npu_board("Radxa ROCK 5C Lite (RK3582)"));
    assert!(is_npu_board("rock-5c-lite"));
    assert!(is_npu_board("Orange Pi 5 Plus"));
    assert!(is_npu_board("rk3576"));
    // The ROCK 5C device tree reports "Radxa ROCK 5C" with no SoC in it.
    assert!(is_npu_board("Radxa ROCK 5C"));
}

/// The `model_patterns` of a board profile in the HAL probe crate.
fn hal_model_patterns(stem: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ados-hal-probe/boards")
        .join(format!("{stem}.yaml"));
    let doc: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["model_patterns"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn every_rock_5c_lite_hal_pattern_is_an_npu_board() {
    // Whatever string the board reports, the NPU step provisions it.
    let patterns = hal_model_patterns("rock-5c-lite");
    assert!(!patterns.is_empty());
    for p in patterns {
        assert!(is_npu_board(&p), "{p}");
    }
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

fn fetched(runtime: &str, id: &str, path: &str, labels: &[&str]) -> FetchedDetector {
    FetchedDetector {
        runtime: runtime.to_string(),
        model_id: id.to_string(),
        model_path: path.to_string(),
        head: String::new(),
        input: None,
        labels: labels.iter().map(|s| s.to_string()).collect(),
    }
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
    let det = FetchedDetector {
        head: "yolo5".to_string(),
        input: Some((640, 640)),
        ..fetched(
            "rknn",
            "com.example.coco",
            "/opt/ados/models/coco.rknn",
            &["person", "car"],
        )
    };
    let wrote = write_detector_block(&cfg, &det).unwrap();
    assert!(wrote, "a fresh config gets the detector block written");

    let y = read_yaml(&cfg);
    // Other keys survive.
    assert_eq!(y["agent"]["name"].as_str(), Some("my-drone"));
    assert_eq!(y["vision"]["enabled"].as_bool(), Some(true));
    assert_eq!(y["vision"]["backend"].as_str(), Some("rknn"));
    // The detector block is written with id/path/enabled/head/input/labels:
    // a .rknn file does not describe its own head or input size.
    let det = &y["vision"]["detector"];
    assert_eq!(det["model_id"].as_str(), Some("com.example.coco"));
    assert_eq!(
        det["model_path"].as_str(),
        Some("/opt/ados/models/coco.rknn")
    );
    assert_eq!(det["enabled"].as_bool(), Some(true));
    assert_eq!(det["head"].as_str(), Some("yolo5"));
    assert_eq!(det["input_width"].as_u64(), Some(640));
    assert_eq!(det["input_height"].as_u64(), Some(640));
    let classes = det["class_labels"].as_sequence().unwrap();
    assert_eq!(classes.len(), 2);
    assert_eq!(classes[0].as_str(), Some("person"));
}

#[test]
fn write_detector_block_leaves_unnamed_head_and_input_to_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    assert!(write_detector_block(&cfg, &fetched("onnx", "m", "/m.onnx", &[])).unwrap());
    let det = &read_yaml(&cfg)["vision"]["detector"];
    assert!(det.get("head").is_none());
    assert!(det.get("input_width").is_none());
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
    let wrote =
        write_detector_block(&cfg, &fetched("rknn", "auto-pick", "/auto.rknn", &[])).unwrap();
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
    let wrote = write_detector_block(&cfg, &fetched("rknn", "m", "/m.rknn", &[])).unwrap();
    assert!(wrote);
    assert!(cfg.exists());
    let y = read_yaml(&cfg);
    assert_eq!(y["vision"]["detector"]["model_id"].as_str(), Some("m"));
}

#[test]
fn fetch_output_names_the_npu_detector_on_a_board_with_a_platform_file() {
    let out = "some log line\nFETCHED\trknn\tperson_yolox_s_v1\t/opt/ados/models/vision/\
               yolox_s_person_640_rk3588.rknn\tyolo5\t640x640\tperson\n";
    let FetchOutcome::Fetched(det) = parse_fetch_output(out).unwrap() else {
        panic!("expected a fetched detector");
    };
    assert_eq!(det.runtime, "rknn");
    assert_eq!(det.model_id, "person_yolox_s_v1");
    assert!(det.model_path.ends_with("_rk3588.rknn"));
    assert_eq!(det.head, "yolo5");
    assert_eq!(det.input, Some((640, 640)));
    assert_eq!(det.labels, vec!["person".to_string()]);
}

#[test]
fn a_rockchip_board_without_a_platform_file_resolves_the_onnx_cpu_detector() {
    // The fetch reports the ONNX variant; the backend it returns is what the
    // config names, and the fetch step then installs the onnx-enabled vision
    // build for the board instead of the default (no-ONNX) build.
    let out =
        "FETCHED\tonnx\tperson_yolox_s_v1\t/m/yolox_s_person_640.onnx\tyolo5\t640x640\tperson";
    let FetchOutcome::Fetched(det) = parse_fetch_output(out).unwrap() else {
        panic!("expected a fetched detector");
    };
    assert_eq!(det.runtime, "onnx");
    for board in hal_model_patterns("rock-5c-lite") {
        assert!(
            crate::binaries::wants_onnx_vision(&board, Some(&det.runtime)),
            "{board}"
        );
        assert!(
            !crate::binaries::wants_onnx_vision(&board, Some("rknn")),
            "{board}"
        );
    }
}

#[test]
fn fetch_output_defers_and_rejects_malformed_results() {
    assert_eq!(
        parse_fetch_output("DEFERRED\n").unwrap(),
        FetchOutcome::Deferred
    );
    // Wrong field count, unknown runtime, empty id/path, garbage.
    for bad in [
        "FETCHED\trknn\tm\t/m.rknn",
        "FETCHED\thailo\tm\t/m.hef\t\t\t",
        "FETCHED\tonnx\t\t/m.onnx\t\t\t",
        "FETCHED\tonnx\tm\t\t\t\t",
        "",
        "Traceback (most recent call last):",
    ] {
        assert!(parse_fetch_output(bad).is_err(), "{bad:?}");
    }
    // An unparseable input size is left to the engine rather than failing.
    let FetchOutcome::Fetched(det) =
        parse_fetch_output("FETCHED\tonnx\tm\t/m.onnx\t\tsquare\t").unwrap()
    else {
        panic!("expected a fetched detector");
    };
    assert_eq!(det.input, None);
    assert!(det.labels.is_empty());
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
