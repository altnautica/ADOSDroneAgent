//! The auxiliary lane's loopback port pair, resolved from operator config.
//!
//! ## Why this exists
//!
//! Three processes have to agree on these two ports: the radio service that
//! spawns the transmitters and receivers, the MAVLink router that feeds the
//! uplink and consumes the re-emit, and the control surface that dials the
//! uplink for relay-proxy requests. They live in crates that deliberately do
//! not depend on one another, so the ports travelled as matching literals in
//! three files with comments pointing at each other.
//!
//! That is correct at exactly one value. The ports are operator-settable
//! (`video.wfb.aux_tx_port` / `aux_rx_port`), and changing either one moved the
//! radio while leaving the other two writing to the old number — so the uplink
//! stopped carrying anything and nothing anywhere reported an error. The
//! failure looks like a dead radio, which is the most expensive thing it could
//! look like.
//!
//! The defaults still live in one place, but every consumer now reads the
//! configured value, so an operator override moves all three together.

use serde::Deserialize;

/// UDP port the auxiliary transmit ingress reads application frames from.
/// Mirrors `ados-radio`'s `default_aux_tx_port`.
pub const DEFAULT_AUX_TX_PORT: u16 = 5602;

/// UDP loopback port the auxiliary receiver re-emits decoded frames onto.
/// Mirrors `ados-radio`'s `default_aux_rx_port`.
pub const DEFAULT_AUX_RX_PORT: u16 = 5603;

/// The agent's config file.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The config path, honouring an `ADOS_CONFIG_YAML` override so a test (and a
/// sim-bench run) can point every reader at the same temp file rather than the
/// real `/etc/ados/config.yaml`. Mirrors the `ADOS_RUN_DIR` seam the sidecar
/// paths already use.
pub fn config_path() -> std::path::PathBuf {
    match std::env::var("ADOS_CONFIG_YAML") {
        Ok(p) if !p.trim().is_empty() => std::path::PathBuf::from(p),
        _ => std::path::PathBuf::from(CONFIG_YAML),
    }
}

/// The resolved pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuxPorts {
    pub tx: u16,
    pub rx: u16,
}

impl Default for AuxPorts {
    fn default() -> Self {
        Self {
            tx: DEFAULT_AUX_TX_PORT,
            rx: DEFAULT_AUX_RX_PORT,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawRoot {
    #[serde(default)]
    video: RawVideo,
}

#[derive(Debug, Default, Deserialize)]
struct RawVideo {
    #[serde(default)]
    wfb: RawWfb,
}

#[derive(Debug, Default, Deserialize)]
struct RawWfb {
    #[serde(default)]
    aux_tx_port: Option<u16>,
    #[serde(default)]
    aux_rx_port: Option<u16>,
}

impl AuxPorts {
    /// Resolve from YAML text. Anything missing or unparseable falls back to
    /// the default for that field only, so one bad key cannot move the other
    /// port.
    ///
    /// Port 0 is rejected: it means "any port" to the kernel, which for a
    /// fixed rendezvous between three processes is never what an operator
    /// meant and would bind somewhere none of the others can find.
    pub fn from_yaml(text: &str) -> Self {
        let raw: RawRoot = serde_norway::from_str(text).unwrap_or_default();
        Self {
            tx: raw
                .video
                .wfb
                .aux_tx_port
                .filter(|p| *p != 0)
                .unwrap_or(DEFAULT_AUX_TX_PORT),
            rx: raw
                .video
                .wfb
                .aux_rx_port
                .filter(|p| *p != 0)
                .unwrap_or(DEFAULT_AUX_RX_PORT),
        }
    }

    /// Resolve from a config file, falling back to defaults when it is absent
    /// or unreadable — the same posture every other consumer of this file
    /// takes, since a node with no config still has to come up on a coherent
    /// pair.
    pub fn load_from(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_yaml(&text),
            Err(_) => Self::default(),
        }
    }

    /// Resolve from the agent's config file.
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_radio_services_own_defaults() {
        let p = AuxPorts::default();
        assert_eq!(p.tx, 5602);
        assert_eq!(p.rx, 5603);
    }

    #[test]
    fn an_operator_override_is_honoured() {
        // The regression: this used to move the radio while the router and the
        // control surface kept writing to 5602/5603, so the uplink carried
        // nothing and nothing reported an error.
        let p =
            AuxPorts::from_yaml("video:\n  wfb:\n    aux_tx_port: 6002\n    aux_rx_port: 6003\n");
        assert_eq!(p.tx, 6002);
        assert_eq!(p.rx, 6003);
    }

    #[test]
    fn one_overridden_port_does_not_disturb_the_other() {
        let p = AuxPorts::from_yaml("video:\n  wfb:\n    aux_tx_port: 6002\n");
        assert_eq!(p.tx, 6002);
        assert_eq!(p.rx, DEFAULT_AUX_RX_PORT);
    }

    #[test]
    fn an_empty_or_unrelated_config_uses_the_defaults() {
        assert_eq!(AuxPorts::from_yaml(""), AuxPorts::default());
        assert_eq!(
            AuxPorts::from_yaml("network:\n  hostname: node\n"),
            AuxPorts::default()
        );
    }

    #[test]
    fn malformed_yaml_falls_back_rather_than_panicking() {
        assert_eq!(
            AuxPorts::from_yaml("video: [this is not a map"),
            AuxPorts::default()
        );
    }

    #[test]
    fn port_zero_is_refused() {
        // Zero means "any port" to the kernel. For a fixed rendezvous between
        // three processes that binds somewhere the others cannot find.
        let p = AuxPorts::from_yaml("video:\n  wfb:\n    aux_tx_port: 0\n    aux_rx_port: 0\n");
        assert_eq!(p, AuxPorts::default());
    }

    #[test]
    fn a_missing_file_yields_the_defaults() {
        let p = AuxPorts::load_from(std::path::Path::new("/nonexistent/ados/config.yaml"));
        assert_eq!(p, AuxPorts::default());
    }
}
