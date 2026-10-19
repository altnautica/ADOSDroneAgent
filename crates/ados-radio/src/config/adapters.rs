//! Per-adapter role assignment, read from the `radio.adapters:` list of the
//! agent config.
//!
//! ```yaml
//! radio:
//!   adapters:
//!     - match: { mac: "aa:bb:cc:dd:ee:01" }
//!       role: rxtx
//!       antenna: omni-5.8
//!     - match: { usb_path: "1-1.3" }
//!       role: rx
//!       antenna: patch-5.8
//! ```
//!
//! An empty list leaves every role to the automatic assignment in
//! [`crate::adapter::resolve_roles`].

use serde::{Deserialize, Serialize};

pub use ados_protocol::radio_adapters::AdapterRole;

/// How one config entry identifies a detected adapter. A MAC compares
/// case-insensitively; a USB path compares exactly. Interface names are not
/// accepted: they can swap between reboots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdapterMatch {
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub usb_path: Option<String>,
}

impl AdapterMatch {
    /// True when this matcher names the adapter with `mac` / `usb_path`. A
    /// matcher with neither key set matches nothing.
    pub fn matches(&self, mac: Option<&str>, usb_path: Option<&str>) -> bool {
        let mac_hit = match (self.mac.as_deref(), mac) {
            (Some(want), Some(have)) => want.trim().eq_ignore_ascii_case(have.trim()),
            _ => false,
        };
        let usb_hit = match (self.usb_path.as_deref(), usb_path) {
            (Some(want), Some(have)) => want.trim() == have.trim(),
            _ => false,
        };
        mac_hit || usb_hit
    }
}

/// One `radio.adapters` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdapterEntry {
    #[serde(rename = "match", default)]
    pub matcher: AdapterMatch,
    pub role: AdapterRole,
    #[serde(default)]
    pub antenna: Option<String>,
}

/// The `radio.adapters` list. Empty means automatic role assignment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct RadioAdaptersConfig {
    #[serde(default)]
    pub adapters: Vec<AdapterEntry>,
}

/// JSON pointer of the `radio` section in the agent config.
pub const RADIO_CONFIG_POINTER: &str = "/radio";

impl RadioAdaptersConfig {
    /// Load the `radio.adapters` list from the agent config. An absent file is
    /// the empty list; a malformed list is logged by the loader and falls back
    /// to the empty list (automatic roles), which never strands the radio.
    pub fn load_from(path: &std::path::Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        ados_config::schema::load_slice::<RadioAdaptersConfig>(path, RADIO_CONFIG_POINTER)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_parse_from_yaml() {
        let text = "adapters:\n  - match: { mac: \"AA:BB:CC:DD:EE:01\" }\n    role: rxtx\n    antenna: omni-5.8\n  - match: { usb_path: \"1-1.3\" }\n    role: mesh\n";
        let cfg: RadioAdaptersConfig = serde_norway::from_str(text).unwrap();
        assert_eq!(cfg.adapters.len(), 2);
        assert_eq!(cfg.adapters[0].role, AdapterRole::Rxtx);
        assert_eq!(cfg.adapters[0].antenna.as_deref(), Some("omni-5.8"));
        assert_eq!(cfg.adapters[1].matcher.usb_path.as_deref(), Some("1-1.3"));
        assert_eq!(cfg.adapters[1].role, AdapterRole::Mesh);
        assert!(cfg.adapters[1].antenna.is_none());
    }

    #[test]
    fn a_mac_matches_case_insensitively_and_a_usb_path_exactly() {
        let m = AdapterMatch {
            mac: Some("AA:BB:CC:DD:EE:01".into()),
            usb_path: None,
        };
        assert!(m.matches(Some("aa:bb:cc:dd:ee:01"), None));
        assert!(!m.matches(Some("aa:bb:cc:dd:ee:02"), Some("1-1.2")));
        let u = AdapterMatch {
            mac: None,
            usb_path: Some("1-1.2".into()),
        };
        assert!(u.matches(None, Some("1-1.2")));
        assert!(!u.matches(None, Some("1-1.20")));
        assert!(!AdapterMatch::default().matches(Some("aa:bb:cc:dd:ee:01"), Some("1-1.2")));
    }
}
