//! Per-profile fetch sets of the prebuilt catalog.

use super::*;

/// The camera boot probe is fetched on every profile even though the CSI
/// camera is drone-side. Its unit is gated on the probation marker, so a
/// node that never staged an overlay never execs it — and any node that DID
/// stage one must carry the binary that can revert it, or a blind apply on a
/// board with no camera has nothing to undo it.
#[test]
fn every_profile_fetches_the_camera_boot_probe() {
    for profile in ["drone", "ground_station", "workstation", "compute", "visor"] {
        let svcs: Vec<&str> = for_profile(profile).iter().map(|b| b.service).collect();
        assert!(
            svcs.contains(&"ados-camera-probe"),
            "{profile} must fetch ados-camera-probe"
        );
    }
    let probe = PREBUILT
        .iter()
        .find(|b| b.service == "ados-camera-probe")
        .expect("the camera probe must be in the catalog");
    assert_eq!(probe.dest, "/opt/ados/bin/ados-camera-probe");
    // Best-effort: a fetch miss must not abort an install on a board with no
    // camera at all.
    assert_eq!(probe.gate, Gate::BestEffort);
}

/// Both FC-bearing profiles fetch the swarm bus, matching its supervisor gate. A
/// ground station that fetched only the drone half would have no fleet view of its
/// own and would fall back to a cloud round-trip for something every node already
/// hears on the air.
#[test]
fn both_fc_profiles_fetch_the_swarm_bus() {
    for profile in ["drone", "ground_station"] {
        let svcs: Vec<&str> = for_profile(profile).iter().map(|b| b.service).collect();
        assert!(
            svcs.contains(&"ados-swarmbus"),
            "{profile} must fetch ados-swarmbus"
        );
    }
    // Best-effort, not Hard: a fetch miss must leave single-drone operation intact
    // rather than abort the install. The bus is not on the C2 path.
    let bus = PREBUILT
        .iter()
        .find(|b| b.service == "ados-swarmbus")
        .expect("ados-swarmbus must be in the catalog");
    assert_eq!(bus.gate, Gate::BestEffort);
    // The path the unit's ConditionPathExists + ExecStart name, or the unit
    // silently never starts.
    assert_eq!(bus.dest, "/opt/ados/bin/ados-swarmbus");
    assert_eq!(bus.asset, "ados-swarmbus-aarch64");
}

#[test]
fn both_profiles_fetch_the_config_tunnel_binary() {
    for profile in ["drone", "ground_station"] {
        let svcs: Vec<&str> = for_profile(profile).iter().map(|b| b.service).collect();
        assert!(
            svcs.contains(&"ados-tunnel-config"),
            "{profile} must fetch ados-tunnel-config for the config-over-radio channel"
        );
    }
}

/// The workstation-class profiles are full agents. `compute` used to be absent
/// from every profile set, so a `--profile compute` Linux install fetched no
/// binary at all — not even the supervisor its unit execs.
#[test]
fn workstation_class_profiles_fetch_the_cores() {
    for profile in ["workstation", "compute"] {
        let svcs: Vec<&str> = for_profile(profile).iter().map(|b| b.service).collect();
        // The orchestrator, cloud relay, control front (LAN pairing), plugin
        // host, logging, and TUI.
        for svc in [
            "ados-supervisor",
            "ados-cloud",
            "ados-control",
            "ados-plugin-host",
            "ados-logd",
            "ados-tui",
        ] {
            assert!(
                svcs.contains(&svc),
                "{profile} profile must fetch {svc}: {svcs:?}"
            );
        }
        // It does NOT fetch the SBC-only flight/radio/video surfaces.
        for svc in [
            "ados-mavlink-router",
            "ados-video",
            "ados-vision",
            "ados-radio",
        ] {
            assert!(
                !svcs.contains(&svc),
                "{profile} profile must NOT fetch {svc}: {svcs:?}"
            );
        }
    }
}

/// Extensions target every node profile, so every profile runs the plugin
/// host. The shipped unit is enable-linked everywhere; a profile that did not
/// fetch the binary left it inactive behind its `ConditionPathExists`.
#[test]
fn every_profile_fetches_the_plugin_host() {
    for profile in ["drone", "ground_station", "workstation", "compute", "visor"] {
        assert!(
            for_profile(profile)
                .iter()
                .any(|b| b.service == "ados-plugin-host"),
            "{profile} must fetch ados-plugin-host"
        );
    }
}

#[test]
fn gpio_is_best_effort_on_every_hardware_profile() {
    let gpio = PREBUILT
        .iter()
        .find(|b| b.service == "ados-gpio")
        .expect("ados-gpio must be in the catalog");
    // The GPIO-output service ships disabled (the unit guard execs /bin/true
    // until the operator opts in), so a missing binary degrades nothing and
    // must never abort the install.
    assert_eq!(gpio.gate, Gate::BestEffort);
    for profile in ["drone", "ground_station", "visor"] {
        assert!(
            for_profile(profile)
                .iter()
                .any(|b| b.service == "ados-gpio"),
            "{profile} must fetch ados-gpio"
        );
    }
}

/// The catalog's every-profile set is the canonical profile list, so a new
/// profile fetches the core services without a second edit here.
#[test]
fn any_is_the_canonical_profile_list() {
    let supervisor = PREBUILT
        .iter()
        .find(|b| b.service == "ados-supervisor")
        .expect("ados-supervisor must be in the catalog");
    assert_eq!(supervisor.profiles, ados_protocol::contracts::PROFILES);
}

/// A visor runs cameras, vision, a MAVLink link to its sensor hub, the setup
/// access point and a small panel with buttons. It has no long-range radio,
/// no RC link and no swarm bus.
#[test]
fn visor_fetches_the_capture_set_and_no_radio_lanes() {
    let svcs: Vec<&str> = for_profile("visor").iter().map(|b| b.service).collect();
    for svc in [
        "ados-supervisor",
        "ados-control",
        "ados-mavlink-router",
        "ados-video",
        "ados-vision",
        "mediamtx",
        "ados-net",
        "ados-input",
        "ados-display",
        "ados-display-probe",
        "ados-oled-i2c",
        "ados-gpio",
    ] {
        assert!(svcs.contains(&svc), "visor must fetch {svc}: {svcs:?}");
    }
    for svc in [
        "ados-radio",
        "ados-groundlink",
        "ados-pic",
        "ados-swarmbus",
        "ados-crsf",
        "ados-tunnel-config",
    ] {
        assert!(!svcs.contains(&svc), "visor must NOT fetch {svc}: {svcs:?}");
    }
    // The onnx vision build and its runtime follow the vision binary.
    assert!(PREBUILT_VISION_ONNX.profiles.contains(&"visor"));
    assert!(PREBUILT_VISION_ONNX_RUNTIME.profiles.contains(&"visor"));
}
