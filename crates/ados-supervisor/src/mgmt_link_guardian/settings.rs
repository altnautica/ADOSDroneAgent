//! Parse the `network.management_link_guardian` block of `config.yaml`.

use super::*;

/// Parse `network.management_link_guardian` out of a config body. An absent
/// section reads as the all-defaults (enabled) config; a malformed config also
/// falls back to enabled rather than silently disabling the protection.
pub fn read_config_from(text: &str) -> MgmtGuardianConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        management_link_guardian: Option<Guardian>,
    }
    #[derive(serde::Deserialize)]
    struct Guardian {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        tick_interval_s: Option<u64>,
        #[serde(default)]
        fast_initial_window_s: Option<u64>,
        #[serde(default)]
        fast_initial_tick_interval_s: Option<u64>,
        #[serde(default)]
        fail_threshold: Option<u32>,
        #[serde(default)]
        repairs_per_window: Option<u32>,
        #[serde(default)]
        repair_window_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.management_link_guardian {
            Some(g) => MgmtGuardianConfig {
                enabled: g.enabled,
                tick_interval: Duration::from_secs(
                    g.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1),
                ),
                fast_initial_window: Duration::from_secs(
                    g.fast_initial_window_s
                        .unwrap_or(DEFAULT_FAST_INITIAL_WINDOW_S),
                ),
                fast_initial_tick: Duration::from_secs(
                    g.fast_initial_tick_interval_s
                        .unwrap_or(DEFAULT_FAST_INITIAL_TICK_INTERVAL_S)
                        .max(1),
                ),
                fail_threshold: g.fail_threshold.unwrap_or(DEFAULT_FAIL_THRESHOLD).max(1),
                repairs_per_window: g
                    .repairs_per_window
                    .unwrap_or(DEFAULT_REPAIRS_PER_WINDOW)
                    .max(1),
                repair_window: Duration::from_secs(
                    g.repair_window_s.unwrap_or(DEFAULT_REPAIR_WINDOW_S).max(1),
                ),
            },
            None => MgmtGuardianConfig::default(),
        },
        Err(_) => MgmtGuardianConfig::default(),
    }
}
