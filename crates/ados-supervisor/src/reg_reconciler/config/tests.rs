use super::*;
use crate::reg_reconciler::RegReconcilerConfig;

// ----- reconciler config parsing -----

#[test]
fn absent_section_is_enabled_with_defaults() {
    let cfg = read_config_from("agent:\n  name: x\n");
    assert!(cfg.enabled);
    assert_eq!(
        cfg.tick_interval,
        Duration::from_secs(DEFAULT_TICK_INTERVAL_S)
    );
    // The fast-initial window is ON by default so a fresh boot converges fast.
    assert_eq!(
        cfg.fast_initial_window,
        Duration::from_secs(DEFAULT_FAST_INITIAL_WINDOW_S)
    );
    assert_eq!(
        cfg.fast_initial_tick,
        Duration::from_secs(DEFAULT_FAST_INITIAL_TICK_INTERVAL_S)
    );
}

#[test]
fn explicit_disable_is_honored() {
    let cfg = read_config_from("network:\n  reg_reconciler:\n    enabled: false\n");
    assert!(!cfg.enabled);
}

#[test]
fn explicit_interval_parses_and_floors_at_one() {
    let cfg = read_config_from(
        "network:\n  reg_reconciler:\n    enabled: true\n    tick_interval_s: 15\n",
    );
    assert!(cfg.enabled);
    assert_eq!(cfg.tick_interval, Duration::from_secs(15));
    let zero = read_config_from("network:\n  reg_reconciler:\n    tick_interval_s: 0\n");
    assert_eq!(zero.tick_interval, Duration::from_secs(1));
}

#[test]
fn fast_initial_fields_parse_and_floor_the_tick() {
    let cfg = read_config_from(
        "network:\n  reg_reconciler:\n    fast_initial_window_s: 90\n    fast_initial_tick_interval_s: 3\n",
    );
    assert_eq!(cfg.fast_initial_window, Duration::from_secs(90));
    assert_eq!(cfg.fast_initial_tick, Duration::from_secs(3));
    // The fast tick floors at 1 s so a zero cannot spin the reconcile.
    let floored =
        read_config_from("network:\n  reg_reconciler:\n    fast_initial_tick_interval_s: 0\n");
    assert_eq!(floored.fast_initial_tick, Duration::from_secs(1));
}

#[test]
fn fast_initial_window_zero_disables_the_fast_path() {
    // A zero window is honored verbatim (no floor): it disables the fast
    // convergence so the reconcile uses the steady cadence from boot.
    let cfg = read_config_from("network:\n  reg_reconciler:\n    fast_initial_window_s: 0\n");
    assert_eq!(cfg.fast_initial_window, Duration::ZERO);
    // With the window off, even uptime 0 yields the steady interval.
    assert_eq!(cfg.effective_interval(Duration::ZERO), cfg.tick_interval);
}

#[test]
fn effective_interval_is_fast_inside_the_window_then_steady() {
    let cfg = RegReconcilerConfig::default();
    // Inside the window (uptime < 60 s): the fast cadence.
    assert_eq!(
        cfg.effective_interval(Duration::from_secs(0)),
        cfg.fast_initial_tick
    );
    assert_eq!(
        cfg.effective_interval(Duration::from_secs(59)),
        cfg.fast_initial_tick
    );
    // At/after the window boundary: the steady cadence (proven steady-state).
    assert_eq!(
        cfg.effective_interval(Duration::from_secs(60)),
        cfg.tick_interval
    );
    assert_eq!(
        cfg.effective_interval(Duration::from_secs(600)),
        cfg.tick_interval
    );
}

#[test]
fn malformed_config_defaults_enabled() {
    let cfg = read_config_from(": : : not yaml");
    assert!(cfg.enabled);
    assert_eq!(
        cfg.tick_interval,
        Duration::from_secs(DEFAULT_TICK_INTERVAL_S)
    );
    assert_eq!(
        cfg.fast_initial_window,
        Duration::from_secs(DEFAULT_FAST_INITIAL_WINDOW_S)
    );
}

// ----- wanted domain + channel resolution (shared with the radio config) -----

#[test]
fn wanted_defaults_when_absent() {
    let w = read_wanted_from("agent:\n  name: x\n");
    assert_eq!(w.domain, "US");
    assert_eq!(w.channel, 149);
}

#[test]
fn wanted_reads_reg_domain_and_channel() {
    let w = read_wanted_from("video:\n  wfb:\n    reg_domain: in\n    channel: 165\n");
    // Uppercased.
    assert_eq!(w.domain, "IN");
    assert_eq!(w.channel, 165);
}

#[test]
fn wanted_rendezvous_pin_overrides_home_channel() {
    let w = read_wanted_from(
        "video:\n  wfb:\n    channel: 149\n    rendezvous_channel: 153\n    reg_domain: US\n",
    );
    assert_eq!(w.channel, 153);
    assert_eq!(w.domain, "US");
}

#[test]
fn wanted_empty_reg_domain_falls_back_to_default() {
    let w = read_wanted_from("video:\n  wfb:\n    reg_domain: ''\n    channel: 149\n");
    assert_eq!(w.domain, "US");
}

#[test]
fn wanted_region_pin_drives_the_global_domain() {
    // A pinned operating region makes the global wanted domain follow it, so
    // the onboard-WiFi global pin tracks the operator's jurisdiction.
    let w = read_wanted_from(
        "network:\n  regulatory:\n    mode: region\n    region: de\n\nvideo:\n  wfb:\n    channel: 149\n",
    );
    assert_eq!(w.domain, "DE");
    assert_eq!(w.channel, 149);
}

#[test]
fn wanted_unrestricted_uses_reconciler_override_then_legacy_then_default() {
    // Unrestricted + an explicit reconciler domain override wins.
    let w = read_wanted_from(
        "network:\n  regulatory:\n    mode: unrestricted\n  reg_reconciler:\n    domain: gb\n",
    );
    assert_eq!(w.domain, "GB");
    // Unrestricted + no override → the legacy video.wfb.reg_domain.
    let w = read_wanted_from(
        "network:\n  regulatory:\n    mode: unrestricted\n\nvideo:\n  wfb:\n    reg_domain: us\n",
    );
    assert_eq!(w.domain, "US");
    // Unrestricted + nothing set anywhere → the safe default US.
    let w = read_wanted_from("network:\n  regulatory:\n    mode: unrestricted\n");
    assert_eq!(w.domain, "US");
}

#[test]
fn wanted_region_pin_without_code_falls_back_to_unrestricted_resolution() {
    // A region mode with no code is not a forceable pin; the resolution falls
    // through to the unrestricted path (legacy reg_domain / default), never a
    // malformed global domain.
    let w = read_wanted_from(
        "network:\n  regulatory:\n    mode: region\n\nvideo:\n  wfb:\n    reg_domain: in\n",
    );
    assert_eq!(w.domain, "IN");
}

#[test]
fn wanted_never_forces_world_default_from_a_region_pin() {
    // A `region: '00'` pin is not forceable (the reconciler never forces the
    // world default), so it falls through rather than capping the radio.
    let w = read_wanted_from(
        "network:\n  regulatory:\n    mode: region\n    region: '00'\n\nvideo:\n  wfb:\n    reg_domain: us\n",
    );
    assert_eq!(w.domain, "US");
}
