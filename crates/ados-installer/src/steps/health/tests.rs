use super::*;

#[test]
fn the_control_wait_is_bounded_and_names_the_last_state() {
    // A unit that comes up on the third poll returns then, without sleeping
    // out the rest of the bound.
    let mut states = ["inactive", "activating", "active"].into_iter();
    let mut sleeps = 0;
    let up = wait_until_active(|| states.next().unwrap().to_string(), 10, || sleeps += 1);
    assert_eq!(up, Ok(()));
    assert_eq!(sleeps, 2);

    // A unit that never comes up stops after the bound, and the miss carries
    // what systemd last reported rather than a bare timeout.
    let mut probes = 0;
    let down = wait_until_active(
        || {
            probes += 1;
            "failed".to_string()
        },
        5,
        || {},
    );
    assert_eq!(down, Err("failed".to_string()));
    assert_eq!(probes, 5);
}

#[test]
fn drone_expects_only_the_supervisor() {
    let units = expected_profile_units("drone");
    assert_eq!(units, &["ados-supervisor.service"]);
}

#[test]
fn ground_station_expects_the_receive_and_ap_set() {
    let units = expected_profile_units("ground_station");
    assert!(units.contains(&"ados-supervisor.service"));
    assert!(units.contains(&"ados-wfb-rx.service"));
    assert!(units.contains(&"ados-hostapd.service"));
    assert!(units.contains(&"ados-setup-captive.service"));
}

#[test]
fn no_profile_expects_a_retired_or_hardware_gated_unit() {
    // A retired unit is pruned before the gate runs, and the Hailo sidecar
    // is inactive on a board without the accelerator, so expecting either
    // would fail a healthy install.
    for profile in ["drone", "ground_station", "workstation", "compute"] {
        let units = expected_profile_units(profile);
        for unit in crate::steps::systemd::RETIRED_UNITS {
            assert!(!units.contains(unit), "{profile} expects retired {unit}");
        }
        assert!(
            !units.contains(&"ados-vision-hailo.service"),
            "{profile} expects the hardware-gated hailo sidecar"
        );
    }
}

#[test]
fn missing_hard_binaries_reports_only_absent_hard_gates() {
    // Pretend every binary is present → no misses.
    let none = missing_hard_binaries("drone", |_| true);
    assert!(none.is_empty());

    // Pretend ados-video is absent (a Hard gate on the drone profile) but
    // everything else present → exactly that one is reported.
    let one = missing_hard_binaries("drone", |dest| dest != "/opt/ados/bin/ados-video");
    assert_eq!(one, vec!["ados-video"]);

    // A best-effort binary being absent must NOT be reported (e.g. ados-tui).
    let beste = missing_hard_binaries("drone", |dest| dest != "/opt/ados/bin/ados-tui");
    assert!(
        beste.is_empty(),
        "best-effort miss must not flag the health gate"
    );

    // All Hard gates absent → all of them reported (drone Hard set:
    // supervisor, MAVLink router, video, cloud, vision).
    let all = missing_hard_binaries("drone", |_| false);
    for svc in [
        "ados-supervisor",
        "ados-mavlink-router",
        "ados-video",
        "ados-cloud",
        "ados-vision",
    ] {
        assert!(
            all.contains(&svc),
            "{svc} (Hard) must be flagged when absent"
        );
    }
    // ados-radio is best-effort on drone → must not appear.
    assert!(!all.contains(&"ados-radio"));
}

#[test]
fn default_radio_binary_is_profile_specific_and_gated() {
    // All present → no miss on either profile.
    assert!(missing_default_radio_binary("drone", |_| true).is_none());
    assert!(missing_default_radio_binary("ground_station", |_| true).is_none());

    // Drone default unit binary missing → ados-radio reported.
    let drone = missing_default_radio_binary("drone", |dest| dest != "/opt/ados/bin/ados-radio");
    assert_eq!(drone, Some("/opt/ados/bin/ados-radio"));

    // Ground-station default unit binary missing → ados-groundlink reported.
    let gs = missing_default_radio_binary("ground_station", |dest| {
        dest != "/opt/ados/bin/ados-groundlink"
    });
    assert_eq!(gs, Some("/opt/ados/bin/ados-groundlink"));

    // The drone never gates on the groundlink binary, and the ground station
    // never gates on the radio binary (each profile checks only its own unit).
    assert!(
        missing_default_radio_binary("drone", |dest| dest != "/opt/ados/bin/ados-groundlink")
            .is_none()
    );
    assert!(missing_default_radio_binary("ground_station", |dest| dest
        != "/opt/ados/bin/ados-radio")
    .is_none());

    // An unknown profile has no default WFB unit binary to gate on.
    assert!(missing_default_radio_binary("workstation", |_| false).is_none());
}

#[test]
fn display_binaries_gated_on_the_panel_marker() {
    // No panel bound → the display binaries never gate the install, even
    // when both are absent (the units are a clean no-op there).
    assert!(missing_default_display_binaries(|| false, |_| false).is_empty());

    // Panel bound + both present → no miss.
    assert!(missing_default_display_binaries(|| true, |_| true).is_empty());

    // Panel bound + the render daemon missing → exactly that one reported.
    let one =
        missing_default_display_binaries(|| true, |dest| dest != "/opt/ados/bin/ados-display");
    assert_eq!(one, vec!["/opt/ados/bin/ados-display"]);

    // Panel bound + both missing → both reported.
    let both = missing_default_display_binaries(|| true, |_| false);
    assert_eq!(
        both,
        vec![
            "/opt/ados/bin/ados-display",
            "/opt/ados/bin/ados-display-probe"
        ]
    );
}

#[test]
fn radio_stack_set_covers_the_bind_protocol() {
    // The bind FSM tools + artifacts the auto-pair path needs at boot.
    assert!(RADIO_BINS.contains(&"wfb_keygen"));
    assert!(RADIO_BINS.contains(&"wfb-server"));
    assert!(BIND_ARTIFACTS.contains(&"/etc/bind.key"));
    assert!(BIND_ARTIFACTS.contains(&"/etc/bind.yaml"));
}

#[test]
fn wifibroadcast_template_checked_in_both_unit_dirs() {
    // Both the deployed drop-in dir and the packaged unit dir are searched
    // so a template in either location satisfies the gate.
    assert!(WIFIBROADCAST_UNIT_DIRS.contains(&"/etc/systemd/system"));
    assert!(WIFIBROADCAST_UNIT_DIRS.contains(&"/usr/lib/systemd/system"));
}

#[test]
fn missing_hard_binaries_respects_profile() {
    // ados-groundlink is best-effort + ground-only; ados-cloud is Hard +
    // both. On the GS profile with everything absent, ados-cloud (Hard)
    // shows, ados-groundlink (best-effort) does not, ados-video (drone-only)
    // does not.
    let gs = missing_hard_binaries("ground_station", |_| false);
    assert!(gs.contains(&"ados-cloud"));
    assert!(gs.contains(&"ados-supervisor"));
    assert!(!gs.contains(&"ados-groundlink"));
    assert!(!gs.contains(&"ados-video"));
}

#[test]
fn every_profile_is_held_to_exactly_the_hard_binaries_it_fetches() {
    // Health and fetch share one catalog: with everything absent, each
    // profile is flagged for its own Hard binaries and nothing else. A
    // workstation or compute node fetches no MAVLink router, so a missing
    // one must not fail its install; a drone or ground station runs the
    // router as its sole C2 path, so a missing one must.
    for profile in ["drone", "ground_station", "workstation", "compute"] {
        let missing = missing_hard_binaries(profile, |_| false);
        let fetched_hard: Vec<&str> = binaries::for_profile(profile)
            .into_iter()
            .filter(|b| b.gate == Gate::Hard)
            .map(|b| b.service)
            .collect();
        assert_eq!(missing, fetched_hard, "{profile}");
        assert!(missing.contains(&"ados-supervisor"), "{profile}");
        let router = missing.contains(&"ados-mavlink-router");
        let runs_router = matches!(profile, "drone" | "ground_station");
        assert_eq!(router, runs_router, "{profile}: router gate");
    }
    // Everything a workstation fetches is present → a healthy install,
    // with no router on disk at all.
    let ws_dests: Vec<&str> = binaries::for_profile("workstation")
        .into_iter()
        .map(|b| b.dest)
        .collect();
    assert!(missing_hard_binaries("workstation", |d| ws_dests.contains(&d)).is_empty());
}
