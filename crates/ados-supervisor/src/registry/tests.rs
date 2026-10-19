use super::*;

#[test]
fn registry_has_expected_shape() {
    let specs = build_specs();
    // ados-discovery and ados-health were removed; ados-vision-hailo and
    // ados-lora added; the sensor plane (clock, sensor hub, GNSS) added.
    assert_eq!(specs.len(), 32, "service count drifted from the catalog");
    // The sensor plane is Hardware tier, gated to the profiles that carry it.
    for (name, gate) in [
        ("ados-clock", "drone|ground_station|visor"),
        ("ados-sensorhub", "drone|visor"),
        ("ados-gnss", "ground_station|visor"),
    ] {
        let spec = specs.iter().find(|s| s.name == name).unwrap();
        assert_eq!(spec.profile_gate, Some(gate), "{name}");
        assert_eq!(spec.category, Category::Hardware, "{name}");
        assert!(!spec.headless_keep, "{name}");
    }
    // Core tier members. ados-mavlink/api/cloud/health/logd are the
    // cross-profile always-on core (the single cloud unit serves the gateway
    // + heartbeat on both profiles, spawning the ground-station bridge when
    // the role resolves to a ground station).
    let core: Vec<_> = specs
        .iter()
        .filter(|s| s.category == Category::Core)
        .map(|s| s.name)
        .collect();
    assert_eq!(
        core,
        vec![
            "ados-mavlink",
            "ados-swarmbus",
            "ados-api",
            "ados-cloud",
            "ados-logd"
        ]
    );
    // The drone TX manager is drone-gated; the GS RX is role-gated to direct.
    let wfb = specs.iter().find(|s| s.name == "ados-wfb").unwrap();
    assert_eq!(wfb.profile_gate, Some("drone"));
    let rx = specs.iter().find(|s| s.name == "ados-wfb-rx").unwrap();
    assert_eq!(rx.profile_gate, Some("ground_station"));
    assert_eq!(rx.role_gate, Some("direct"));
    // The units ados-net owns in-process carry no row at all: a packaged
    // unit that every profile tears down must not be something the
    // supervisor reports on or tries to reconcile.
    for absent in ["ados-modem", "ados-wifi-client", "ados-ethernet"] {
        assert!(
            !specs.iter().any(|s| s.name == absent),
            "{absent} is torn down on every profile and must not be registered"
        );
    }
    // The vision engine is a camera-node (drone, visor) Hardware unit and is
    // NOT in the
    // headless keep set (vision/AI is excluded from the lean core).
    let vis = specs.iter().find(|s| s.name == "ados-vision").unwrap();
    assert_eq!(vis.profile_gate, Some("drone|visor"));
    assert_eq!(vis.category, Category::Hardware);
    assert!(!vis.headless_keep);
    // The CRSF RC lane is a Hardware unit on both FC-bearing profiles
    // (ground-side transmitter today, drone-side for the relay last mile),
    // never in the headless keep set. Its presence here is what arms the
    // Elrs hot-plug class routing to it.
    let crsf = specs.iter().find(|s| s.name == "ados-crsf").unwrap();
    assert_eq!(crsf.profile_gate, Some("drone|ground_station"));
    assert_eq!(crsf.category, Category::Hardware);
    assert!(!crsf.headless_keep);
    // The config-over-radio channel is a cross-profile Hardware unit
    // (drone terminator + ground injector), never in the headless keep set.
    let tunnel = specs
        .iter()
        .find(|s| s.name == "ados-tunnel-config")
        .unwrap();
    assert_eq!(tunnel.profile_gate, Some("drone|ground_station"));
    assert_eq!(tunnel.category, Category::Hardware);
    assert!(!tunnel.headless_keep);
}

#[test]
fn registry_has_hailo_and_no_deleted_units() {
    let specs = build_specs();
    let hailo = specs
        .iter()
        .find(|s| s.name == "ados-vision-hailo")
        .expect("hailo sidecar registered");
    assert_eq!(hailo.category, Category::Hardware);
    assert_eq!(hailo.profile_gate, Some("drone"));
    assert_eq!(hailo.role_gate, None);
    assert!(!hailo.headless_keep);
    for gone in ["ados-discovery", "ados-health", "ados-wifi-client"] {
        assert!(
            !specs.iter().any(|s| s.name == gone),
            "{gone} was removed and must not be registered"
        );
    }
}

#[test]
fn headless_keep_set_is_exactly_the_lean_core() {
    // The lean headless profile boots only the Rust core: the MAVLink
    // router, the swarm state bus, the durable store, the camera encode, the
    // radio TX, and the native HTTP front. Everything else (FastAPI, cloud,
    // health, GS units, on-demand) is NOT in the KEEP set, so the headless
    // gate blocks it. The store is in the set because it is Rust (zero-Python
    // holds) and a lean flight node is the node an operator cannot reach —
    // the one that most needs its own flight recorder.
    let specs = build_specs();
    let kept: Vec<_> = specs
        .iter()
        .filter(|s| s.headless_keep)
        .map(|s| s.name)
        .collect();
    assert_eq!(
        kept,
        vec![
            "ados-mavlink",
            "ados-swarmbus",
            "ados-logd",
            "ados-video",
            "ados-wfb",
            "ados-control"
        ],
        "headless KEEP set drifted"
    );
    // ados-api (FastAPI) is explicitly NOT kept: headless is zero-Python.
    let api = specs.iter().find(|s| s.name == "ados-api").unwrap();
    assert!(!api.headless_keep);
}

#[test]
fn the_log_store_is_supervised_cross_profile_and_kept_headless() {
    // Regression: an unregistered ados-logd got no liveness check, no
    // auto-restart, no parked retry and — the one that matters — no
    // `systemctl reset-failed`, so five restarts in a minute left the black
    // box permanently dead and only an SSH session could clear it.
    let specs = build_specs();
    let logd = specs
        .iter()
        .find(|s| s.name == "ados-logd")
        .expect("ados-logd must be in the supervised set");
    assert_eq!(logd.category, Category::Core);
    assert_eq!(logd.profile_gate, None, "the store runs on every profile");
    assert_eq!(logd.role_gate, None);
    assert!(
        logd.headless_keep,
        "a lean flight node is the node that most needs its own recorder"
    );
}

#[test]
fn every_catalog_unit_is_part_of_the_supervisor() {
    // The supervisor's shutdown leaves every adopted row to systemd, which is
    // sound only because `PartOf=` carries the supervisor's own stop and
    // restart to the unit. A unit without it would keep running after
    // `systemctl stop ados-supervisor` and would not be cycled by a restart.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    for def in SERVICE_REGISTRY.iter() {
        let path = dir.join(format!("{}.service", def.name));
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let part_of = body
            .lines()
            .filter_map(|l| l.trim().strip_prefix("PartOf="))
            .flat_map(str::split_whitespace)
            .any(|u| u == "ados-supervisor.service");
        assert!(
            part_of,
            "{} is not PartOf=ados-supervisor.service",
            def.name
        );
    }
}

#[test]
fn the_parked_retry_cadence_is_in_the_fixed_recovery_band() {
    // A parked service is retried inside the fixed 2-5 s recovery band, not
    // on a half-minute cooldown: a unit that needs one more attempt after a
    // brownout must not cost the operator 30 s per try.
    assert!(PARKED_RETRY_COOLDOWN >= Duration::from_secs(2));
    assert!(PARKED_RETRY_COOLDOWN <= Duration::from_secs(5));
}

#[test]
fn mavlink_is_core_on_the_fc_profiles_only() {
    // The MAVLink router is the sole C2 path with no packaged fallback, so
    // it is Core (always started) on the FC-bearing profiles. It is gated to
    // `drone|ground_station|visor` (a Visor's stage-A sensor hub is a flight
    // controller over USB), NOT the FC-less compute node (which never
    // fetches the router binary and would crash-loop on an unconditional
    // start). No role gate.
    let specs = build_specs();
    let mav = specs
        .iter()
        .find(|s| s.name == "ados-mavlink")
        .expect("ados-mavlink must be in the registry");
    assert_eq!(mav.category, Category::Core);
    assert_eq!(mav.profile_gate, Some("drone|ground_station|visor"));
    assert_eq!(mav.role_gate, None);
}

#[test]
fn the_swarm_bus_is_core_on_both_fc_profiles_and_kept_headless() {
    // Gated like ados-mavlink on the FC-bearing vehicle profiles, and for the
    // same reason: the bus is that seam seen from either end. A drone fills its
    // beacon from the router's state socket; a ground station listens so the
    // operator's fleet view is local-first. Gating it to `drone` alone would
    // make the fleet view depend on a cloud round-trip, and gating it wider
    // would start it on an FC-less compute node that never fetches the binary.
    let specs = build_specs();
    let bus = specs
        .iter()
        .find(|s| s.name == "ados-swarmbus")
        .expect("ados-swarmbus must be in the registry");
    assert_eq!(bus.category, Category::Core);
    assert_eq!(bus.profile_gate, Some("drone|ground_station"));
    assert_eq!(bus.role_gate, None);
    // Onboard collision avoidance reads this table, so a lean headless flight
    // node that keeps the router must keep the bus that feeds separation.
    assert!(bus.headless_keep);
    // Same gate as the router except the Visor, which runs the router for its
    // stage-A sensor hub but has no fleet slot.
    let mav = specs.iter().find(|s| s.name == "ados-mavlink").unwrap();
    assert_eq!(
        mav.profile_gate.map(|g| g.replace("|visor", "")).as_deref(),
        bus.profile_gate,
        "same gate as the router, minus the visor"
    );
    assert_eq!(bus.profile_gate, Some("drone|ground_station"));
}

#[test]
fn crash_loop_label_applies_after_threshold_in_window() {
    let mut spec = ServiceSpec::from_def(&SERVICE_REGISTRY[0]);
    let t0 = Instant::now();
    for i in 0..(MAX_FAILURES - 1) {
        assert!(!spec.record_failure(t0 + Duration::from_millis(i as u64)));
    }
    assert!(spec.record_failure(t0 + Duration::from_millis(MAX_FAILURES as u64)));
    assert_eq!(spec.state, ServiceState::CrashLooping);
    assert_eq!(spec.state.as_str(), "crash_looping");
}

#[test]
fn failures_outside_the_window_do_not_count_toward_a_crash_loop() {
    let mut spec = ServiceSpec::from_def(&SERVICE_REGISTRY[0]);
    let t0 = Instant::now();
    for _ in 0..(MAX_FAILURES - 1) {
        spec.record_failure(t0);
    }
    let later = t0 + FAILURE_WINDOW + Duration::from_secs(1);
    assert!(!spec.record_failure(later));
    assert_eq!(spec.failure_times.len(), 1);
}
