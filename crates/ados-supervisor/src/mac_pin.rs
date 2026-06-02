//! Always-on stable-MAC reconciler.
//!
//! Some onboard USB WiFi chipsets have no efuse MAC and randomize their address
//! every boot, churning the DHCP lease (and the box's IP). This task re-affirms
//! the pin for known no-efuse chipsets and runs the cross-boot learner for
//! unknown ones, on startup and a slow timer (so a hot-plugged adapter is
//! handled without an install). Like the install-time step it only writes a
//! next-boot `systemd-networkd` `.link`; it never touches a live interface, so
//! it cannot drop the operator's management link.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::watch;

use ados_macpin::engine::{self, ReconcileConfig};
use ados_macpin::AdapterState;

use crate::config::CONFIG_YAML;

/// How often the reconciler re-checks the adapters.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(300);

/// Parse `network.mac_pin` out of a config body. Default ON (pinning is
/// non-destructive — file-only, next-boot), live re-tag OFF. An absent section
/// reads as enabled, keeping the supervisor in step with the Python config
/// model's default.
pub fn read_config_from(text: &str) -> ReconcileConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        mac_pin: Option<MacPin>,
    }
    #[derive(serde::Deserialize)]
    struct MacPin {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        apply_live_allowed: bool,
        #[serde(default)]
        overrides: HashMap<String, String>,
    }
    fn default_true() -> bool {
        true
    }
    let enabled_default = ReconcileConfig { enabled: true, ..Default::default() };
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.mac_pin {
            Some(mp) => ReconcileConfig {
                enabled: mp.enabled,
                apply_live_allowed: mp.apply_live_allowed,
                overrides: mp.overrides,
            },
            None => enabled_default,
        },
        Err(_) => enabled_default,
    }
}

fn read_config() -> ReconcileConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => ReconcileConfig { enabled: true, ..Default::default() },
    }
}

/// Run the reconciler until `shutdown` flips. Re-reads config each tick so a
/// config edit takes effect without a restart.
pub async fn run(mut shutdown: watch::Receiver<bool>) {
    tracing::info!("mac_pin_reconciler_started");
    loop {
        let cfg = read_config();
        if cfg.enabled {
            // The reconcile does blocking sysfs / udevadm / file I/O — keep it
            // off the async runtime.
            match tokio::task::spawn_blocking(move || engine::reconcile(&cfg, true)).await {
                Ok(state) => {
                    let pinned = count(&state, AdapterState::Pinned);
                    let candidates = count(&state, AdapterState::Candidate);
                    if pinned > 0 || candidates > 0 {
                        tracing::info!(pinned, candidates, "mac_pin_reconcile");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "mac_pin_reconcile_task_failed"),
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    tracing::info!("mac_pin_reconciler_stopped");
}

fn count(state: &ados_macpin::MacPinsState, want: AdapterState) -> usize {
    state.adapters.iter().filter(|a| a.state == want).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_is_enabled() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert!(!cfg.apply_live_allowed);
        assert!(cfg.overrides.is_empty());
    }

    #[test]
    fn explicit_disable_is_honored() {
        let cfg = read_config_from("network:\n  mac_pin:\n    enabled: false\n");
        assert!(!cfg.enabled);
    }

    #[test]
    fn overrides_and_apply_live_parse() {
        let body = "network:\n  mac_pin:\n    enabled: true\n    apply_live_allowed: true\n    overrides:\n      \"1234:5678\": \"02:11:22:33:44:55\"\n";
        let cfg = read_config_from(body);
        assert!(cfg.enabled);
        assert!(cfg.apply_live_allowed);
        assert_eq!(cfg.overrides.get("1234:5678").map(String::as_str), Some("02:11:22:33:44:55"));
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        let cfg = read_config_from(": : : not yaml");
        assert!(cfg.enabled);
    }
}
