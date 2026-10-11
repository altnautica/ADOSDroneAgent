use super::*;

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
    let cfg = read_config_from("network:\n  wifi_powersave:\n    enabled: false\n");
    assert!(!cfg.enabled);
}

#[test]
fn explicit_interval_parses_and_floors_at_one() {
    let cfg = read_config_from(
        "network:\n  wifi_powersave:\n    enabled: true\n    tick_interval_s: 15\n",
    );
    assert!(cfg.enabled);
    assert_eq!(cfg.tick_interval, Duration::from_secs(15));
    let zero = read_config_from("network:\n  wifi_powersave:\n    tick_interval_s: 0\n");
    assert_eq!(zero.tick_interval, Duration::from_secs(1));
}

#[test]
fn fast_initial_fields_parse_and_floor_the_tick() {
    let cfg = read_config_from(
        "network:\n  wifi_powersave:\n    fast_initial_window_s: 90\n    fast_initial_tick_interval_s: 3\n",
    );
    assert_eq!(cfg.fast_initial_window, Duration::from_secs(90));
    assert_eq!(cfg.fast_initial_tick, Duration::from_secs(3));
    // The fast tick floors at 1 s so a zero cannot spin the reconcile.
    let floored =
        read_config_from("network:\n  wifi_powersave:\n    fast_initial_tick_interval_s: 0\n");
    assert_eq!(floored.fast_initial_tick, Duration::from_secs(1));
}

#[test]
fn fast_initial_window_zero_disables_the_fast_path() {
    // A zero window is honored verbatim (no floor): it disables the fast
    // convergence so the reconcile uses the steady cadence from boot.
    let cfg = read_config_from("network:\n  wifi_powersave:\n    fast_initial_window_s: 0\n");
    assert_eq!(cfg.fast_initial_window, Duration::ZERO);
    // With the window off, even uptime 0 yields the steady interval.
    assert_eq!(cfg.effective_interval(Duration::ZERO), cfg.tick_interval);
}

#[test]
fn effective_interval_is_fast_inside_the_window_then_steady() {
    let cfg = WifiPowersaveConfig::default();
    // Inside the window (uptime < 60 s): the fast cadence.
    assert_eq!(
        cfg.effective_interval(Duration::from_secs(0)),
        cfg.fast_initial_tick
    );
    assert_eq!(
        cfg.effective_interval(Duration::from_secs(59)),
        cfg.fast_initial_tick
    );
    // At/after the window boundary: the steady cadence.
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
