//! Parse the `network.mgmt_failover` block of `config.yaml`.

use super::*;

/// Parse `network.mgmt_failover` out of a config body. Absent / malformed →
/// enabled defaults so the reach-back is on out of the box.
pub fn read_config_from(text: &str) -> MgmtFailoverConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        mgmt_failover: Option<Failover>,
    }
    #[derive(serde::Deserialize)]
    struct Failover {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        down_debounce_s: Option<u64>,
        #[serde(default)]
        recover_debounce_s: Option<u64>,
        #[serde(default)]
        tick_interval_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.mgmt_failover {
            Some(f) => MgmtFailoverConfig {
                enabled: f.enabled,
                down_debounce: Duration::from_secs(
                    f.down_debounce_s.unwrap_or(DEFAULT_DOWN_DEBOUNCE_S),
                ),
                recover_debounce: Duration::from_secs(
                    f.recover_debounce_s.unwrap_or(DEFAULT_RECOVER_DEBOUNCE_S),
                ),
                tick_interval: Duration::from_secs(
                    f.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1),
                ),
            },
            None => MgmtFailoverConfig::default(),
        },
        Err(_) => MgmtFailoverConfig::default(),
    }
}
