use super::*;

#[test]
fn reg_domain_format_accepts_valid_rejects_malformed() {
    // Two uppercase letters or digits → accepted.
    assert!(is_valid_reg_domain("IN"));
    assert!(is_valid_reg_domain("US"));
    assert!(is_valid_reg_domain("00")); // world domain
                                        // Anything else → rejected before it reaches `iw reg set`.
    assert!(!is_valid_reg_domain("in")); // lowercase
    assert!(!is_valid_reg_domain("USA")); // too long
    assert!(!is_valid_reg_domain("I")); // too short
    assert!(!is_valid_reg_domain("")); // empty
    assert!(!is_valid_reg_domain("I N")); // whitespace / wrong length
    assert!(!is_valid_reg_domain("U;")); // injected punctuation
}

#[test]
fn parse_dfs_channels_keeps_radar_and_no_ir_only() {
    let text = "\
Band 2:
	Frequencies:
		* 5180 MHz [36] (disabled)
		* 5200 MHz [40] (20.0 dBm)
		* 5260 MHz [52] (no IR, radar detection)
		* 5300 MHz [60] (radar detection)
		* 5745 MHz [149] (30.0 dBm)
";
    let got = parse_dfs_channels(text);
    // 36 is disabled (not DFS), 40/149 are usable (not DFS); 52/60 are DFS.
    let want: std::collections::BTreeSet<u8> = [52, 60].into_iter().collect();
    assert_eq!(got, want);
}

#[test]
fn assert_reg_ready_passes_when_channel_enabled_and_non_dfs() {
    let enabled: std::collections::BTreeSet<u8> = [36, 40, 149, 153].into_iter().collect();
    let dfs: std::collections::BTreeSet<u8> = [52, 60].into_iter().collect();
    assert!(assert_reg_ready(149, &enabled, &dfs, false).is_ok());
}

#[test]
fn assert_reg_ready_rejects_channel_not_enabled() {
    let enabled: std::collections::BTreeSet<u8> = [36, 40, 149].into_iter().collect();
    let dfs = std::collections::BTreeSet::new();
    assert_eq!(
        assert_reg_ready(165, &enabled, &dfs, false),
        Err(RegError::ChannelNotEnabled { channel: 165 })
    );
}

#[test]
fn assert_reg_ready_rejects_dfs_home_unless_allowed() {
    let enabled: std::collections::BTreeSet<u8> = [52, 149].into_iter().collect();
    let dfs: std::collections::BTreeSet<u8> = [52].into_iter().collect();
    // DFS home refused by default.
    assert_eq!(
        assert_reg_ready(52, &enabled, &dfs, false),
        Err(RegError::ChannelIsDfs { channel: 52 })
    );
    // Opt-in clears it (the channel is still in the enabled set).
    assert!(assert_reg_ready(52, &enabled, &dfs, true).is_ok());
}

#[test]
fn assert_reg_ready_passes_when_enabled_set_unknown() {
    // An empty enabled set means "could not read the wiphy list" — the gate
    // must not wedge a board whose channel list is unreadable.
    let empty = std::collections::BTreeSet::new();
    let dfs = std::collections::BTreeSet::new();
    assert!(assert_reg_ready(149, &empty, &dfs, false).is_ok());
}

#[test]
fn self_managed_phy_with_conflicting_country_is_detected() {
    // The live override shape: global says US, a self-managed phy bakes BO.
    let text = "\
global
country US: DFS-FCC
	(5170 - 5250 @ 80), (N/A, 17), (N/A)
phy#3 (self-managed)
country BO: DFS-UNSET
	(5170 - 5250 @ 80), (N/A, 20), (N/A)
";
    assert_eq!(
        parse_conflicting_self_managed_phy(text, "US"),
        Some(("phy3".to_string(), "BO".to_string()))
    );
}

#[test]
fn self_managed_phy_matching_global_is_not_a_conflict() {
    // A self-managed phy that already carries the wanted country is fine.
    let text = "\
global
country US: DFS-FCC
phy#0 (self-managed)
country US: DFS-FCC
";
    assert_eq!(parse_conflicting_self_managed_phy(text, "US"), None);
}

#[test]
fn non_self_managed_country_block_is_not_an_override() {
    // The plain global block (no self-managed phy) is never an override, even
    // when it differs from the wanted domain — that is the retry/timeout path,
    // not the unrecoverable EEPROM case.
    let text = "\
global
country BO: DFS-UNSET
	(5170 - 5250 @ 80), (N/A, 20), (N/A)
";
    assert_eq!(parse_conflicting_self_managed_phy(text, "US"), None);
}

#[test]
fn self_managed_spelling_variants_both_parse() {
    // `iw` has emitted both "self managed" and "self-managed".
    let spaced = "phy3 (self managed)\ncountry BO: DFS-UNSET\n";
    assert_eq!(
        parse_conflicting_self_managed_phy(spaced, "US"),
        Some(("phy3".to_string(), "BO".to_string()))
    );
}

#[test]
fn reg_error_reason_codes_are_stable_and_bland() {
    assert_eq!(RegError::CommandFailed.reason_code(), "command_failed");
    assert_eq!(RegError::InvalidFormat.reason_code(), "invalid_format");
    assert_eq!(
        RegError::VerifyTimeout {
            want: "US".into(),
            got: Some("BO".into())
        }
        .reason_code(),
        "verify_timeout"
    );
    assert_eq!(
        RegError::EepromOverride {
            want: "US".into(),
            got: "BO".into()
        }
        .reason_code(),
        "phy_override"
    );
    assert_eq!(
        RegError::ChannelNotEnabled { channel: 165 }.reason_code(),
        "channel_not_enabled"
    );
    assert_eq!(
        RegError::ChannelIsDfs { channel: 52 }.reason_code(),
        "channel_dfs"
    );
}

#[test]
fn reg_is_verified_matches_wanted_domain_case_insensitively() {
    // Live domain equals the wanted domain → verified.
    assert!(reg_is_verified(Some("US"), "US"));
    // Case does not matter (iw can emit either).
    assert!(reg_is_verified(Some("us"), "US"));
    assert!(reg_is_verified(Some("US"), "us"));
    // A different live domain (the forbidden-band case) → not verified.
    assert!(!reg_is_verified(Some("BO"), "US"));
    // Unknown live domain (iw unreadable) → not verified.
    assert!(!reg_is_verified(None, "US"));
    // Empty wanted domain → nothing to match, never verified.
    assert!(!reg_is_verified(Some("US"), ""));
    assert!(!reg_is_verified(None, ""));
}

#[test]
fn parse_wiphy_extracts_phy_name() {
    let info = "Interface wlan1\n\ttype monitor\n\twiphy 0\n";
    assert_eq!(parse_wiphy(info).as_deref(), Some("phy0"));
    let info2 = "Interface wlan1\n\twiphy 3\n";
    assert_eq!(parse_wiphy(info2).as_deref(), Some("phy3"));
}

#[test]
fn parse_wiphy_missing_is_none() {
    assert!(parse_wiphy("Interface wlan1\n\ttype monitor\n").is_none());
}

#[test]
fn parse_enabled_channels_keeps_usable_skips_disabled_and_dfs() {
    let text = "\
Band 2:
	Frequencies:
		* 5180 MHz [36] (disabled)
		* 5200 MHz [40] (20.0 dBm)
		* 5260 MHz [52] (no IR, radar detection)
		* 5300 MHz [60] (radar detection)
		* 5745 MHz [149] (30.0 dBm)
		* 5765 MHz [153] (30.0 dBm)
";
    let got = parse_enabled_channels(text);
    let want: std::collections::BTreeSet<u8> = [40, 149, 153].into_iter().collect();
    assert_eq!(got, want);
}

#[test]
fn parse_enabled_channels_empty_input_is_empty() {
    assert!(parse_enabled_channels("").is_empty());
    // A line with no bracket token contributes nothing.
    assert!(parse_enabled_channels("Band 2:\n\tFrequencies:\n").is_empty());
}
