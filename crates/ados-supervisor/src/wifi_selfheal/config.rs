//! Config for the reactive WiFi self-heal watchdog (`network.wifi_selfheal`).
//!
//! Pure parsing is unit-tested on every host; the canonical-path read is
//! Linux-only (the tick that calls it is a no-op off Linux).

use std::time::Duration;

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

/// Default consecutive-failure count before a heal fires. A single failing tick
/// can be a momentarily-busy gateway; two in a row is a sustained dead path.
pub(super) const DEFAULT_FAIL_THRESHOLD: u32 = 2;

/// Default quiet period after a heal, per connection. A re-association takes a
/// few seconds to re-DHCP; this window covers that plus slack so the watchdog
/// never re-fires on a connection that is mid-recovery.
pub(super) const DEFAULT_COOLDOWN_S: u64 = 60;

/// Configuration for the WiFi self-heal watchdog, read from
/// `network.wifi_selfheal`. Default-ON: a fresh board with no config heals out
/// of the box. An operator can disable it cleanly if a bespoke network setup
/// ever conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiSelfHealConfig {
    /// Whether the watchdog runs at all. Default true.
    pub enabled: bool,
    /// Consecutive failing ticks before a re-association fires. Floored at 1 so a
    /// zero in config can never make a single transient failure trigger a heal.
    pub fail_threshold: u32,
    /// Per-connection quiet period after a heal.
    pub cooldown: Duration,
}

impl Default for WifiSelfHealConfig {
    fn default() -> Self {
        WifiSelfHealConfig {
            enabled: true,
            fail_threshold: DEFAULT_FAIL_THRESHOLD,
            cooldown: Duration::from_secs(DEFAULT_COOLDOWN_S),
        }
    }
}

/// Parse `network.wifi_selfheal` out of a config body. An absent section reads
/// as the all-defaults (enabled) config, so the watchdog is on out of the box. A
/// malformed config also falls back to the enabled default rather than silently
/// disabling the failover.
pub fn read_config_from(text: &str) -> WifiSelfHealConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        wifi_selfheal: Option<SelfHeal>,
    }
    #[derive(serde::Deserialize)]
    struct SelfHeal {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        fail_threshold: Option<u32>,
        #[serde(default)]
        cooldown_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.wifi_selfheal {
            Some(sh) => WifiSelfHealConfig {
                enabled: sh.enabled,
                fail_threshold: sh.fail_threshold.unwrap_or(DEFAULT_FAIL_THRESHOLD).max(1),
                cooldown: Duration::from_secs(sh.cooldown_s.unwrap_or(DEFAULT_COOLDOWN_S)),
            },
            None => WifiSelfHealConfig::default(),
        },
        Err(_) => WifiSelfHealConfig::default(),
    }
}

/// Read `network.wifi_selfheal` from the canonical config path. Re-read each
/// tick so a config edit takes effect without restarting the supervisor. Linux
/// only — the tick that reads it is a no-op on a non-Linux dev host.
#[cfg(target_os = "linux")]
pub(super) fn read_config() -> WifiSelfHealConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => WifiSelfHealConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
        assert_eq!(cfg.cooldown, Duration::from_secs(DEFAULT_COOLDOWN_S));
    }

    #[test]
    fn explicit_disable_is_honored() {
        let cfg = read_config_from("network:\n  wifi_selfheal:\n    enabled: false\n");
        assert!(!cfg.enabled);
    }

    #[test]
    fn explicit_tunables_parse() {
        let body =
            "network:\n  wifi_selfheal:\n    enabled: true\n    fail_threshold: 3\n    cooldown_s: 90\n";
        let cfg = read_config_from(body);
        assert!(cfg.enabled);
        assert_eq!(cfg.fail_threshold, 3);
        assert_eq!(cfg.cooldown, Duration::from_secs(90));
    }

    #[test]
    fn zero_threshold_is_floored_to_one() {
        let cfg = read_config_from("network:\n  wifi_selfheal:\n    fail_threshold: 0\n");
        assert_eq!(cfg.fail_threshold, 1);
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        let cfg = read_config_from(": : : not yaml");
        assert!(cfg.enabled);
        assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
    }
}
