use super::*;

#[test]
fn unrestricted_options_line_is_exact() {
    // Default per-rate calibration table is OFF (=0): full driver power on the
    // home channel, bounded by the software txpower clamp, not the efuse table.
    let line = render_modprobe_options(&ModprobeMode::Unrestricted {
        tx_pwr_by_rate: DEFAULT_TX_PWR_BY_RATE,
    });
    assert_eq!(
        line,
        "options 8812eu rtw_regd_src=0 rtw_country_code=00 rtw_tx_pwr_lmt_enable=0 rtw_tx_pwr_by_rate=0"
    );
    assert_eq!(DEFAULT_TX_PWR_BY_RATE, 0);
}

#[test]
fn unrestricted_tx_pwr_by_rate_override_is_emitted() {
    // The bench A/B override threads through verbatim.
    let line = render_modprobe_options(&ModprobeMode::Unrestricted { tx_pwr_by_rate: 1 });
    assert!(line.contains("rtw_tx_pwr_by_rate=1"));
}

#[test]
fn region_options_line_is_exact_and_carries_the_code() {
    let line = render_modprobe_options(&ModprobeMode::Region("IN".to_string()));
    assert_eq!(
        line,
        "options 8812eu rtw_regd_src=0 rtw_country_code=IN rtw_tx_pwr_lmt_enable=1"
    );
    // A different region substitutes verbatim.
    let de = render_modprobe_options(&ModprobeMode::Region("DE".to_string()));
    assert_eq!(
        de,
        "options 8812eu rtw_regd_src=0 rtw_country_code=DE rtw_tx_pwr_lmt_enable=1"
    );
}

#[test]
fn unrestricted_lifts_the_legal_cap_and_the_efuse_power_table() {
    // Under unrestricted BOTH driver power tables are off by default: the
    // regulatory power-LIMIT table (no legal per-region cap) and the per-rate
    // power CALIBRATION table (=0, so the efuse per-rate table cannot cap the
    // home channel below the software clamp). The host-VBUS power-budget clamp is
    // enforced separately by the radio crate's tx-power ramp — that is what keeps
    // a bus-powered adapter from browning out, not the efuse table.
    let line = render_modprobe_options(&ModprobeMode::Unrestricted {
        tx_pwr_by_rate: DEFAULT_TX_PWR_BY_RATE,
    });
    assert!(line.contains("rtw_tx_pwr_lmt_enable=0"));
    assert!(line.contains("rtw_tx_pwr_by_rate=0"));
    // And the regdb source is the driver's private regdb (efuse ignored).
    assert!(line.contains("rtw_regd_src=0"));
    assert!(line.contains("rtw_country_code=00"));
}

#[test]
fn region_re_enables_the_power_limit_table() {
    // A pinned region turns the legal power-limit table back ON.
    let line = render_modprobe_options(&ModprobeMode::Region("US".to_string()));
    assert!(line.contains("rtw_tx_pwr_lmt_enable=1"));
    // The driver still uses its own regdb (regd_src=0), never the OS source.
    assert!(line.contains("rtw_regd_src=0"));
}

/// The default unrestricted posture (per-rate calibration off), for tests.
fn unr() -> ModprobeMode {
    ModprobeMode::Unrestricted {
        tx_pwr_by_rate: DEFAULT_TX_PWR_BY_RATE,
    }
}

#[test]
fn region_never_emits_regd_src_1() {
    // regd_src=1 (OS source) still applies the efuse country hint, which
    // cannot lift a restrictive baked country. The generator must NEVER emit
    // it — that is the bug in the vendored template this module replaces.
    for mode in [
        unr(),
        ModprobeMode::Region("US".to_string()),
        ModprobeMode::Region("IN".to_string()),
    ] {
        assert!(!render_modprobe_options(&mode).contains("rtw_regd_src=1"));
    }
}

#[test]
fn file_body_has_a_bland_header_and_the_options_line() {
    let body = render_modprobe_file(&unr());
    // The header describes the file, not any internal planning artifact.
    assert!(body.starts_with("# ADOS Drone Agent"));
    assert!(body.contains(&render_modprobe_options(&unr())));
    assert!(body.ends_with('\n'));
}

#[test]
fn mode_from_config_defaults_to_unrestricted() {
    // No block at all → unrestricted (default per-rate calibration).
    assert_eq!(mode_from_config("agent:\n  name: x\n"), unr());
    // Explicit unrestricted (region present but irrelevant) → unrestricted.
    assert_eq!(
        mode_from_config("network:\n  regulatory:\n    mode: unrestricted\n    region: US\n"),
        unr()
    );
    // Malformed config → unrestricted (permissive).
    assert_eq!(mode_from_config(": : not yaml"), unr());
}

#[test]
fn mode_from_config_reads_tx_pwr_by_rate_override() {
    // The bench A/B knob overrides the default under the unrestricted posture.
    assert_eq!(
        mode_from_config("network:\n  regulatory:\n    tx_pwr_by_rate: 1\n"),
        ModprobeMode::Unrestricted { tx_pwr_by_rate: 1 }
    );
    // A pinned region ignores the override (it uses the legal power-limit table).
    assert_eq!(
        mode_from_config(
            "network:\n  regulatory:\n    mode: region\n    region: US\n    tx_pwr_by_rate: 1\n"
        ),
        ModprobeMode::Region("US".to_string())
    );
}

#[test]
fn mode_from_config_reads_a_pinned_region() {
    assert_eq!(
        mode_from_config("network:\n  regulatory:\n    mode: region\n    region: in\n"),
        ModprobeMode::Region("IN".to_string())
    );
    // Case-insensitive mode token.
    assert_eq!(
        mode_from_config("network:\n  regulatory:\n    mode: REGION\n    region: DE\n"),
        ModprobeMode::Region("DE".to_string())
    );
}

#[test]
fn mode_from_config_region_without_valid_code_is_unrestricted() {
    // region mode with no code → unrestricted.
    assert_eq!(
        mode_from_config("network:\n  regulatory:\n    mode: region\n"),
        unr()
    );
    // region mode with a malformed code → unrestricted (permissive).
    assert_eq!(
        mode_from_config("network:\n  regulatory:\n    mode: region\n    region: USA\n"),
        unr()
    );
}

#[test]
fn config_region_predicate_matches_iso_alpha2() {
    assert!(is_valid_region_code("US"));
    assert!(is_valid_region_code("00"));
    assert!(!is_valid_region_code("USA"));
    assert!(!is_valid_region_code(""));
}

#[test]
fn live_matches_unrestricted_wants_country_00_and_private_regdb() {
    assert!(live_matches("00", "0", &unr()));
    // A foreign baked country under unrestricted is the stale-driver case.
    assert!(!live_matches("BO", "0", &unr()));
    // The OS regdb source is never a match (it applies the efuse hint).
    assert!(!live_matches("00", "1", &unr()));
    // Whitespace / case tolerant (sysfs values often carry trailing newlines).
    assert!(live_matches(" 00 ", " 0 ", &unr()));
}

#[test]
fn live_matches_region_wants_that_country() {
    assert!(live_matches(
        "US",
        "0",
        &ModprobeMode::Region("US".to_string())
    ));
    assert!(live_matches(
        "us",
        "0",
        &ModprobeMode::Region("US".to_string())
    ));
    assert!(!live_matches(
        "00",
        "0",
        &ModprobeMode::Region("US".to_string())
    ));
    assert!(!live_matches(
        "DE",
        "0",
        &ModprobeMode::Region("US".to_string())
    ));
}
