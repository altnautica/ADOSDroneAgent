//! Parse the `network.usb_rehome` block of `config.yaml`.

use super::*;

/// Parse `network.usb_rehome`. Absent / malformed → enabled defaults.
pub fn read_config_from(text: &str) -> UsbRehomeConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        usb_rehome: Option<Rehome>,
    }
    #[derive(serde::Deserialize)]
    struct Rehome {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        cooldown_s: Option<u64>,
        #[serde(default)]
        healthy_reset_s: Option<u64>,
        #[serde(default)]
        tick_interval_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.usb_rehome {
            Some(r) => {
                let cooldown_s = r.cooldown_s.unwrap_or(DEFAULT_COOLDOWN_S).max(1);
                UsbRehomeConfig {
                    enabled: r.enabled,
                    cooldown: Duration::from_secs(cooldown_s),
                    healthy_reset: Duration::from_secs(
                        r.healthy_reset_s.unwrap_or(DEFAULT_HEALTHY_RESET_S).max(1),
                    ),
                    tick_interval: Duration::from_secs(
                        r.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1),
                    ),
                }
            }
            None => UsbRehomeConfig::default(),
        },
        Err(_) => UsbRehomeConfig::default(),
    }
}
