//! The country code the access point advertises.
//!
//! hostapd's `country_code` was a hardcoded `IN` on every unit, while the
//! radio's own regulatory reconciler defaults to `US` when no region is
//! pinned. So a stock box ran its access point and its radio under two
//! different declared jurisdictions — one of them necessarily wrong, and
//! neither derived from anything the operator said.
//!
//! This resolves the AP's country from the same operator setting the radio
//! reads (`network.regulatory.region`) with the same default, so the two
//! agree. It governs ONLY the `country_code` line hostapd writes; the radio's
//! reconciler remains the authority for the radio itself, and nothing here
//! changes what that does.
//!
//! Behaviour change worth stating plainly: a unit with no pinned region moves
//! from advertising `IN` to advertising `US`. That is the point — it now
//! matches what the radio was already doing — but it is a change, not a
//! refactor.

use serde::Deserialize;

/// Country advertised when the operator has pinned no region. Matches the
/// radio reconciler's own default so the two halves of a stock box agree.
pub const DEFAULT_AP_COUNTRY: &str = "US";

#[derive(Debug, Default, Deserialize)]
struct RawRoot {
    #[serde(default)]
    network: RawNetwork,
}

#[derive(Debug, Default, Deserialize)]
struct RawNetwork {
    #[serde(default)]
    regulatory: RawRegulatory,
}

#[derive(Debug, Default, Deserialize)]
struct RawRegulatory {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    region: Option<String>,
}

/// Whether a string is a usable ISO country code for hostapd.
///
/// Exactly two ASCII letters, and never the world default `00` — hostapd
/// treats an unusable value as a configuration error and refuses to start,
/// which takes the access point down rather than degrading it.
fn is_usable_country(s: &str) -> bool {
    s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic())
}

/// Resolve the AP country from config text.
///
/// The region counts only when the operator has actually opted into a
/// jurisdiction (`mode: region`), mirroring the radio's own reading — a region
/// left in the file while the mode is unrestricted is not a pin.
pub fn from_yaml(text: &str) -> String {
    let raw: RawRoot = serde_norway::from_str(text).unwrap_or_default();
    let pinned = raw
        .network
        .regulatory
        .mode
        .as_deref()
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("region"));
    if !pinned {
        return DEFAULT_AP_COUNTRY.to_string();
    }
    raw.network
        .regulatory
        .region
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_uppercase)
        .filter(|r| is_usable_country(r))
        .unwrap_or_else(|| DEFAULT_AP_COUNTRY.to_string())
}

/// Resolve from a config file, defaulting when it is absent or unreadable.
pub fn load_from(path: &std::path::Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => from_yaml(&text),
        Err(_) => DEFAULT_AP_COUNTRY.to_string(),
    }
}

/// Resolve from the agent's config file.
pub fn load() -> String {
    load_from(&crate::aux_ports::config_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unpinned_unit_matches_the_radios_own_default() {
        // The whole point: the AP and the radio stop disagreeing on a stock
        // box. This is also the behaviour change — it used to advertise IN.
        assert_eq!(from_yaml(""), "US");
        assert_eq!(
            from_yaml("network:\n  regulatory:\n    mode: unrestricted\n"),
            "US"
        );
    }

    #[test]
    fn a_pinned_region_is_honoured() {
        assert_eq!(
            from_yaml("network:\n  regulatory:\n    mode: region\n    region: IN\n"),
            "IN"
        );
        assert_eq!(
            from_yaml("network:\n  regulatory:\n    mode: region\n    region: de\n"),
            "DE",
            "an operator's lowercase entry is still a pin"
        );
    }

    #[test]
    fn a_region_without_the_mode_is_not_a_pin() {
        // Mirrors how the radio reads it: a region left in the file while the
        // mode is unrestricted has not been opted into.
        assert_eq!(from_yaml("network:\n  regulatory:\n    region: IN\n"), "US");
    }

    #[test]
    fn a_malformed_region_falls_back_rather_than_reaching_hostapd() {
        // hostapd refuses to start on an unusable country code, which takes
        // the access point down rather than degrading it.
        for bad in ["", "U", "USA", "00", "1N", "!!"] {
            let text = format!("network:\n  regulatory:\n    mode: region\n    region: {bad}\n");
            assert_eq!(
                from_yaml(&text),
                "US",
                "region {bad:?} must not reach hostapd"
            );
        }
    }

    #[test]
    fn malformed_yaml_falls_back_rather_than_panicking() {
        assert_eq!(from_yaml("network: [not a map"), "US");
    }

    #[test]
    fn a_missing_file_yields_the_default() {
        assert_eq!(
            load_from(std::path::Path::new("/nonexistent/ados/config.yaml")),
            "US"
        );
    }
}
