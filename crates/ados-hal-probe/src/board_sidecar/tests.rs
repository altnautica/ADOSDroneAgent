use super::*;
use crate::host_facts::{parse_cpu_online, parse_cpuinfo_model, parse_mem_total_mb};

fn facts(compatible: &str, model: &str) -> HostFacts {
    HostFacts {
        model: model.to_string(),
        compatible: compatible.to_string(),
        cpuinfo_model: String::new(),
        ram_mb: 8192,
        cpu_cores: 8,
        machine: "aarch64".to_string(),
        os: "linux".to_string(),
        board_override: String::new(),
        usb_ttys: Vec::new(),
    }
}

fn yaml_files(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .expect("board profile directory must be readable")
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(".yaml")
                .then(|| (name, std::fs::read(e.path()).expect("readable profile")))
        })
        .collect();
    out.sort();
    out
}

#[test]
fn every_board_profile_yaml_is_registered_and_parses() {
    // A board added to the bundled directory but not to the embedded table
    // would silently detect as unknown.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("boards");
    let on_disk: Vec<String> = yaml_files(&dir)
        .into_iter()
        .map(|(name, _)| name.trim_end_matches(".yaml").to_string())
        .collect();
    let mut embedded: Vec<String> = BOARD_PROFILE_YAML
        .iter()
        .map(|(stem, _)| stem.to_string())
        .collect();
    embedded.sort();
    assert_eq!(
        embedded, on_disk,
        "the embedded board table and boards/ have drifted"
    );

    // And every one of them parses into the fingerprint's view of a profile.
    assert_eq!(
        load_profiles().len(),
        BOARD_PROFILE_YAML.len(),
        "a board profile failed to parse"
    );
}

#[test]
fn bundled_boards_match_the_canonical_hal_profiles() {
    // In the source repository the HAL profiles under src/ados/hal/boards
    // are canonical; the crate bundles a copy so it packages on its own.
    // A packaged crate has no canonical directory, so there is nothing to
    // compare against.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let canonical = root.join("../../src/ados/hal/boards");
    if !canonical.is_dir() {
        return;
    }
    let bundled = yaml_files(&root.join("boards"));
    let canonical = yaml_files(&canonical);
    let names = |v: &[(String, Vec<u8>)]| v.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    assert_eq!(
        names(&bundled),
        names(&canonical),
        "crates/ados-hal-probe/boards/ and src/ados/hal/boards/ list different files"
    );
    for ((name, a), (_, b)) in bundled.iter().zip(&canonical) {
        assert!(
            a == b,
            "boards/{name} differs from src/ados/hal/boards/{name}; copy it over"
        );
    }
}

#[test]
fn the_sidecar_version_matches_the_shared_contract_registry() {
    assert_eq!(
        BOARD_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("board").unwrap()
    );
}

#[test]
fn an_npu_board_reports_its_real_tops_and_accelerator_flag() {
    // The offload reconciler keys on this: `npu_tops: 0` made an NPU board
    // offload detection it could run onboard.
    let profiles = load_profiles();
    let fp = resolve(&profiles, &facts("rockchip,rk3588s2", "Rockchip RK3588S2"));
    assert!(fp.npu_tops > 0.0, "fingerprint: {fp:?}");
    assert!(fp.has_accelerator);
    assert_ne!(fp.name, "generic-arm64");
    assert_eq!(fp.version, BOARD_SIDECAR_VERSION);
}

#[test]
fn the_compatible_token_disambiguates_two_boards_with_one_soc_model() {
    // Every Allwinner A733 board reports the same model string, so only the
    // device-tree compatible token tells the A7S from the A7Z. Matching on
    // the model alone would report the wrong board's tier and NPU.
    let profiles = load_profiles();
    let a7z = resolve(&profiles, &facts("radxa,cubie-a7z", "sun60iw2"));
    let a7s = resolve(&profiles, &facts("radxa,cubie-a7s", "sun60iw2"));
    assert_eq!(a7z.name, "Radxa Cubie A7Z");
    assert_eq!(a7s.name, "Radxa Cubie A7S");
    assert_ne!(a7z.tier, a7s.tier);
    // The detected model string is preserved, not replaced by the profile.
    assert_eq!(a7z.model, "sun60iw2");

    // With NO compatible token the shared SoC-family string identifies
    // nothing: it must resolve to neither board rather than to whichever
    // profile happened to list it (the A7Z listed `sun60iw2`, which is the
    // A7S's own verified on-rig model string, so a real A7S came up with
    // the A7Z's FC UART and a 40-pin GPIO map it does not have).
    let ambiguous = resolve(&profiles, &facts("", "sun60iw2"));
    assert_eq!(ambiguous.name, "generic-arm64", "{ambiguous:?}");
}

#[test]
fn an_unknown_board_falls_back_to_the_generic_profile_not_to_nothing() {
    let profiles = load_profiles();
    let fp = resolve(&profiles, &facts("acme,unheard-of-v9", "ACME Unheard Of"));
    assert_eq!(fp.name, "generic-arm64");
    assert_eq!(fp.model, "ACME Unheard Of");
    // Tiered from what it actually has, not from the generic placeholder.
    assert_eq!(fp.tier, detect_tier(8192));
    assert_eq!(fp.npu_tops, 0.0);
    assert!(!fp.has_accelerator);
}

#[test]
fn an_unknown_x86_64_host_resolves_to_the_generic_x86_64_profile() {
    let profiles = load_profiles();
    let mut f = facts("", "");
    f.machine = "x86_64".to_string();
    let fp = resolve(&profiles, &f);
    assert_eq!(fp.name, "generic-x86_64", "{fp:?}");
    assert_eq!(fp.npu_tops, 0.0);
    assert!(!fp.has_accelerator);
}

#[test]
fn the_cpuinfo_fallback_is_used_when_the_device_tree_says_nothing() {
    let profiles = load_profiles();
    let mut f = facts("", "");
    f.cpuinfo_model = "Raspberry Pi 4 Model B Rev 1.4".to_string();
    let fp = resolve(&profiles, &f);
    assert!(fp.name.contains("Raspberry Pi 4"), "fingerprint: {fp:?}");
}

#[test]
fn tiers_follow_the_ram_bands() {
    assert_eq!(detect_tier(256), 1);
    assert_eq!(detect_tier(1024), 2);
    assert_eq!(detect_tier(4096), 3);
    assert_eq!(detect_tier(8192), 4);
}

#[test]
fn the_document_carries_the_python_writer_key_set_exactly() {
    // The two writers must agree by construction: this is the contract the
    // Rust readers (pairing route, status route, offload reconciler) and any
    // remaining Python reader share.
    let profiles = load_profiles();
    let fp = resolve(&profiles, &facts("raspberrypi,5-model-b", "Raspberry Pi 5"));
    let value = serde_json::to_value(&fp).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "arch",
            "buttons",
            "cpu_cores",
            "encoder",
            "has_accelerator",
            "has_local_inference",
            "hw_video_codecs",
            "local_inference",
            "model",
            "name",
            "npu_tops",
            "profile",
            "ram_mb",
            "soc",
            "tier",
            "vendor",
            "version",
        ]
    );
}

#[test]
fn the_sidecar_is_written_atomically_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run").join("board.json");
    let profiles = load_profiles();
    let fp = resolve(&profiles, &facts("raspberrypi,5-model-b", "Raspberry Pi 5"));
    write_sidecar(&path, &fp).unwrap();

    let back: BoardFingerprint =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(back, fp);
    // No tmp file left behind.
    assert!(!path.with_extension("json.tmp").exists());
}

#[test]
fn a_local_inference_board_reports_the_cpu_path() {
    // cm5 declares `local_inference: onnx` and no NPU: the perception tier
    // must read a local path, not an offload one.
    let profiles = load_profiles();
    let cm5 = profiles
        .iter()
        .find(|p| p.name == "Raspberry Pi CM5")
        .expect("cm5 profile");
    let fp = from_profile(cm5, "Raspberry Pi CM5", &facts("", ""));
    assert_eq!(fp.local_inference, "onnx");
    assert!(fp.has_local_inference);
    assert!(!fp.has_accelerator);
}

#[test]
fn the_sidecar_carries_the_profile_stem() {
    let profiles = load_profiles();
    let cm5 = profiles
        .iter()
        .find(|p| p.stem == "cm5")
        .expect("cm5 profile");
    let fp = from_profile(cm5, "Raspberry Pi CM5", &facts("", ""));
    assert_eq!(fp.profile, "cm5");
    assert_eq!(fp.name, "Raspberry Pi CM5");
    // An unmatched board resolves through its generic profile's stem.
    let generic = resolve(&profiles, &facts("acme,unheard-of-v9", "ACME Unheard Of"));
    assert_eq!(generic.profile, "generic-arm64");
}

#[test]
fn a_panel_board_publishes_its_button_pins() {
    let profiles = load_profiles();
    let rpi4 = profiles.iter().find(|p| p.stem == "rpi4b").expect("rpi4b");
    let fp = from_profile(rpi4, "Raspberry Pi 4 Model B", &facts("", ""));
    assert_eq!(
        fp.buttons,
        Some(ButtonPins {
            up: 5,
            down: 6,
            select: 13,
            back: 19
        })
    );
    let cm5 = profiles.iter().find(|p| p.stem == "cm5").expect("cm5");
    assert_eq!(from_profile(cm5, "", &facts("", "")).buttons, None);
    // An older sidecar without these fields still reads.
    let old = r#"{"version":1,"name":"x","model":"x","tier":2,"ram_mb":1,"cpu_cores":1,"vendor":"v","soc":"s","arch":"a","hw_video_codecs":[],"npu_tops":0.0,"has_accelerator":false,"local_inference":"none","has_local_inference":false}"#;
    let fp: BoardFingerprint = serde_json::from_str(old).unwrap();
    assert_eq!(fp.profile, "");
    assert_eq!(fp.buttons, None);
    assert_eq!(fp.encoder, None);
}

#[test]
fn host_fact_parsers_read_the_real_shapes() {
    assert_eq!(parse_mem_total_mb("MemTotal:        8123456 kB\n"), 7933);
    assert_eq!(parse_mem_total_mb("SwapTotal: 0 kB\n"), 0);
    assert_eq!(parse_cpu_online("0-7\n"), 8);
    assert_eq!(parse_cpu_online("0,2-3\n"), 3);
    assert_eq!(parse_cpu_online("\n"), 0);
    assert_eq!(
        parse_cpuinfo_model("processor\t: 0\nHardware\t: BCM2835\n"),
        "BCM2835"
    );
    assert_eq!(parse_cpuinfo_model("flags : none\n"), "");
}

/// The board-override grammar: the YAML filename STEM, and nothing else.
///
/// Asserted over EVERY embedded board so a new profile cannot be added with
/// an unreachable override. The Python half asserts the same property over
/// the same directory (`tests/test_hal.py::TestBoardOverride`), which is
/// what makes the two halves resolve one token to one board.
#[test]
fn every_board_stem_is_a_resolvable_override_token() {
    let profiles = load_profiles();
    for (stem, _) in BOARD_PROFILE_YAML {
        let mut f = facts("", "");
        f.board_override = (*stem).to_string();
        let fp = resolve(&profiles, &f);
        let expected = profiles
            .iter()
            .find(|p| &p.stem == stem)
            .expect("every embedded stem parses");
        // The variant-aware name for these facts, so the 8-core Rock 5C row
        // compares against its own variant rather than the Lite's name.
        let expected_name = select_variant(expected, f.cpu_cores)
            .and_then(|v| v.name.clone())
            .unwrap_or_else(|| expected.name.clone());
        assert_eq!(fp.name, expected_name, "override {stem} resolved wrong");
    }
}

#[test]
fn the_override_stem_pins_the_a7s_against_its_shared_soc_family_string() {
    // The documented escape hatch for a mis-detected board. `cubie-a7s` is
    // a legal slug (the display-name form never was), so this is the token
    // an operator and a shell script can actually write.
    let profiles = load_profiles();
    let mut f = facts("", "sun60iw2");
    f.board_override = "cubie-a7s".to_string();
    let fp = resolve(&profiles, &f);
    assert_eq!(fp.name, "Radxa Cubie A7S");
    assert_eq!(fp.soc, "Allwinner A733");
    // The real device-tree model is still recorded, not replaced by the token.
    assert_eq!(fp.model, "sun60iw2");
}

#[test]
fn an_override_naming_no_profile_falls_through_to_auto_detection() {
    // A typo must not cost the node its profile: the override exists to
    // correct a mis-detection, so an unresolvable token degrades to what the
    // hardware says rather than to a profile-less phantom board.
    let profiles = load_profiles();
    let mut f = facts("raspberrypi,5-model-b", "Raspberry Pi 5");
    f.board_override = "Radxa ROCK 5C Lite (RK3582)".to_string();
    let fp = resolve(&profiles, &f);
    assert!(fp.name.contains("Raspberry Pi 5"), "fingerprint: {fp:?}");
}

/// The Rock 5C / 5C Lite share ONE device tree, so the core count decides.
/// Filename order used to, which published `soc: RK3582` and a name ending
/// in "Lite" for a full 8-core RK3588S2 board.
#[test]
fn the_shared_rock_5c_device_tree_is_split_by_core_count() {
    let profiles = load_profiles();
    let mut lite = facts("radxa,rock-5c", "Radxa ROCK 5C ");
    lite.cpu_cores = 6;
    let lite = resolve(&profiles, &lite);
    assert_eq!(lite.name, "Radxa ROCK 5C Lite (RK3582)");
    assert_eq!(lite.soc, "RK3582");

    let mut full = facts("radxa,rock-5c", "Radxa ROCK 5C ");
    full.cpu_cores = 8;
    let full = resolve(&profiles, &full);
    assert_eq!(full.name, "Radxa ROCK 5C (RK3588S2)");
    assert_eq!(full.soc, "RK3588S2");
    // Both bins keep the NPU and the VPU, so the perception tier is the same.
    assert_eq!(full.npu_tops, lite.npu_tops);
    assert!(full.has_accelerator);
}

/// No two boards may claim one device-tree string. The three collisions this
/// guards were invisible to a byte-identical-pattern comparison because they
/// were substring overlaps, a shared SoC-family name, and a shared DT.
#[test]
fn real_device_tree_strings_resolve_to_exactly_one_board() {
    let profiles = load_profiles();
    // (compatible, model, cpu_cores) -> resolved name.
    let cases: &[(&str, &str, i64, &str)] = &[
        ("radxa,cubie-a7s", "sun60iw2", 8, "Radxa Cubie A7S"),
        ("radxa,cubie-a7z", "sun60iw2", 8, "Radxa Cubie A7Z"),
        (
            "radxa,rock-5c",
            "Radxa ROCK 5C ",
            6,
            "Radxa ROCK 5C Lite (RK3582)",
        ),
        (
            "radxa,rock-5c",
            "Radxa ROCK 5C ",
            8,
            "Radxa ROCK 5C (RK3588S2)",
        ),
        ("radxa,cm3", "Radxa CM3 IO Board", 4, "Radxa CM3 (RK3566)"),
        ("rockchip,rk3566", "RK3566 EVB", 4, "Radxa CM3 (RK3566)"),
        ("radxa,cm4", "Radxa CM4", 8, "Radxa CM4 (RK3588S2)"),
        (
            "radxa,rock-5b-plus",
            "Radxa ROCK 5B+",
            8,
            "Radxa ROCK 5B+ (RK3588)",
        ),
        (
            "raspberrypi,4-model-b",
            "Raspberry Pi 4 Model B Rev 1.4",
            4,
            "Raspberry Pi 4B",
        ),
        (
            "raspberrypi,3-model-b-plus",
            "Raspberry Pi 3 Model B Plus Rev 1.3",
            4,
            "Raspberry Pi 3",
        ),
        (
            "raspberrypi,3-compute-module",
            "Raspberry Pi Compute Module 3 Plus Rev 1.0",
            4,
            "Raspberry Pi Compute Module 3",
        ),
    ];
    for (compatible, model, cores, expected) in cases {
        let mut f = facts(compatible, model);
        f.cpu_cores = *cores;
        let fp = resolve(&profiles, &f);
        assert_eq!(
            fp.name, *expected,
            "{compatible} / {model} / {cores} cores resolved to {}",
            fp.name
        );
    }
    // The plain ROCK 5B is a substring prefix of the 5B+ strings, never the
    // other way round, so it must not resolve to the 5B+ profile.
    let fp = resolve(&profiles, &facts("radxa,rock-5b", "Radxa ROCK 5B"));
    assert_ne!(
        fp.profile, "rock-5b-plus",
        "ROCK 5B resolved to {}",
        fp.name
    );
}

#[test]
fn the_rk3566_board_keeps_its_npu_and_its_third_uart() {
    // Two profiles used to claim RK3566 with different `npu_tops` (0.0 vs
    // 0.8) and different UART sets, so `has_accelerator` and the perception
    // tier flipped on identical hardware depending on which filename sorted
    // first. One profile survives, and it is the measured one.
    let profiles = load_profiles();
    let fp = resolve(&profiles, &facts("radxa,cm3", "Radxa CM3 IO Board"));
    assert_eq!(fp.npu_tops, 0.8);
    assert!(fp.has_accelerator);
    assert!(fp.hw_video_codecs.iter().any(|c| c == "vp9_dec"));
}

#[test]
fn a_dev_mac_is_labelled_as_one_but_still_carries_the_generic_declarations() {
    // Parity with the Python fallback: same name, and the generic profile's
    // declarations (the fallback UART candidates) rather than nothing.
    let profiles = load_profiles();
    let mut f = facts("", "");
    f.os = "macos".to_string();
    f.machine = "aarch64".to_string();
    let fp = resolve(&profiles, &f);
    assert_eq!(fp.name, "macOS (dev)");
    assert_eq!(fp.tier, detect_tier(8192));
    assert_eq!(fp.vendor, "unknown");
}

#[test]
fn the_document_carries_the_hub_and_gauge_only_when_there_is_one() {
    let mut rig: BoardProfile = serde_norway::from_str(
        "name: Test Rig\nmodel_patterns: [test rig]\n\
         imu: {bus: hub, model: ICM-42688-P, rate_hz: 1000}\n\
         gnss: {transport: hub}\n\
         sensor_hub: {board: matek_h743_slim_v3}\n\
         battery_gauge: {type: ina226, i2c_bus: 1, addr: 64, shunt_ohm: 0.0005, max_current_a: 60.0}\n",
    )
    .unwrap();
    rig.stem = "test-rig".into();
    assert_eq!(rig.imu.as_ref().unwrap().rate_hz, Some(1000));

    // Declared hub, unplugged: present false, null device; gauge copied.
    let fp = from_profile(&rig, "Test Rig", &facts("", ""));
    let v = serde_json::to_value(&fp).unwrap();
    assert_eq!(
        v["sensor_hub"],
        serde_json::json!({"present": false, "device": null, "vid_pid": "1209:ad05"})
    );
    assert_eq!(
        v["battery_gauge"],
        serde_json::json!({"type": "ina226", "i2c_bus": 1, "addr": 64,
                           "shunt_ohm": 0.0005, "max_current_a": 60.0})
    );

    // Plugged in: the node is reported.
    let mut f = facts("", "");
    f.usb_ttys = vec![UsbTty {
        device: "/dev/ttyACM0".into(),
        vid: 0x1209,
        pid: 0xAD05,
    }];
    let fp = from_profile(&rig, "Test Rig", &f);
    let hub = fp.sensor_hub.as_ref().unwrap();
    assert!(hub.present);
    assert_eq!(hub.device.as_deref(), Some("/dev/ttyACM0"));
    // Round-trips through the reader.
    let back: BoardFingerprint =
        serde_json::from_str(&serde_json::to_string(&fp).unwrap()).unwrap();
    assert_eq!(back, fp);

    // A board declaring neither omits both keys; an attached hub still shows.
    let profiles = load_profiles();
    let pi5 = facts("raspberrypi,5-model-b", "Raspberry Pi 5");
    let v = serde_json::to_value(resolve(&profiles, &pi5)).unwrap();
    assert!(v.get("sensor_hub").is_none());
    assert!(v.get("battery_gauge").is_none());
    let mut pi5_hub = pi5;
    pi5_hub.usb_ttys = f.usb_ttys;
    let fp = resolve(&profiles, &pi5_hub);
    assert!(fp.sensor_hub.unwrap().present);
}
