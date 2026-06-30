//! Ground-station relay/receiver/mesh configuration, read from the
//! `ground_station:` block of `/etc/ados/config.yaml`.
//!
//! The sibling `ados_radio::config::WfbConfig` covers only the `video.wfb:`
//! block (the radio link). The relay and receiver loops need a few extra
//! fields that live under `ground_station:` in Python (`WfbRelayConfig`,
//! `WfbReceiverConfig`, `MeshConfig`). Field names and defaults mirror the
//! Python dataclasses so a node reads the same values regardless of which
//! language drives the loop.

use serde::Deserialize;

fn default_receiver_mdns_service() -> String {
    "_ados-receiver._tcp".to_string()
}
fn default_port() -> u16 {
    5800
}
fn default_true() -> bool {
    true
}
fn default_bat_iface() -> String {
    "bat0".to_string()
}
fn default_atlas_listen_port() -> u16 {
    // The loopback port the WFB aux receiver re-emits decoded application frames
    // on (`wfb_rx -p 3 -u <aux_rx_port>`, `video.wfb.aux_rx_port`, default 5603).
    // The drone radiates Atlas events on the aux tx ingress (5602); the GS reads
    // the decoded side here. Kept distinct from the tx ingress so the relay can
    // never feed back into the transmit path.
    5603
}

/// Relay-role config (`ground_station.wfb_relay`).
#[derive(Debug, Clone, Deserialize)]
pub struct WfbRelayConfig {
    /// mDNS service type the receiver advertises and the relay resolves.
    #[serde(default = "default_receiver_mdns_service")]
    pub receiver_mdns_service: String,
    /// Default forward port when none is carried on the resolved record.
    #[serde(default = "default_port")]
    pub receiver_port: u16,
}

impl Default for WfbRelayConfig {
    fn default() -> Self {
        Self {
            receiver_mdns_service: default_receiver_mdns_service(),
            receiver_port: default_port(),
        }
    }
}

/// Receiver-role config (`ground_station.wfb_receiver`).
#[derive(Debug, Clone, Deserialize)]
pub struct WfbReceiverConfig {
    /// Aggregator UDP listen port for inbound relay forwards.
    #[serde(default = "default_port")]
    pub listen_port: u16,
    /// Whether the local monitor adapter is aggregated alongside relay forwards.
    #[serde(default = "default_true")]
    pub accept_local_nic: bool,
}

impl Default for WfbReceiverConfig {
    fn default() -> Self {
        Self {
            listen_port: default_port(),
            accept_local_nic: default_true(),
        }
    }
}

/// Mesh fabric config (`ground_station.mesh`) — the subset the loops read.
#[derive(Debug, Clone, Deserialize)]
pub struct MeshConfig {
    /// The batman-adv carrier interface (default `bat0`).
    #[serde(default = "default_bat_iface")]
    pub bat_iface: String,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            bat_iface: default_bat_iface(),
        }
    }
}

/// Atlas world-model aux-lane relay config (`ground_station.atlas`).
///
/// Inert by default (`enabled = false`): the relay only spawns when a relay-role
/// node has `enabled` set AND a configured `compute_base_url`. A non-Atlas ground
/// station therefore reads the same defaults and behaves byte-identically.
/// `listen_port` is the loopback port the WFB aux receiver delivers decoded Atlas
/// datagrams to (`video.wfb.aux_rx_port`, default 5603); `compute_base_url` is the
/// compute node's `atlas_event_router` base URL each decoded event is re-POSTed to
/// (v1 is config-provided — mDNS auto-resolve of the `workstation` profile is a
/// later wave).
#[derive(Debug, Clone, Deserialize)]
pub struct AtlasRelayConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_atlas_listen_port")]
    pub listen_port: u16,
    #[serde(default)]
    pub compute_base_url: Option<String>,
}

impl Default for AtlasRelayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_port: default_atlas_listen_port(),
            compute_base_url: None,
        }
    }
}

/// The `ground_station:` config subset the relay/receiver loops consume.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GroundStationConfig {
    #[serde(default)]
    pub wfb_relay: WfbRelayConfig,
    #[serde(default)]
    pub wfb_receiver: WfbReceiverConfig,
    #[serde(default)]
    pub mesh: MeshConfig,
    #[serde(default)]
    pub atlas: AtlasRelayConfig,
}

impl GroundStationConfig {
    /// Load from the `ground_station:` block in the agent config file. A missing
    /// or unparseable file yields the Python defaults.
    pub fn load_from(path: &std::path::Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            ground_station: GroundStationConfig,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return GroundStationConfig::default();
        };
        let raw: RawConfig = serde_norway::from_str(&text).unwrap_or_default();
        raw.ground_station
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_python_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = GroundStationConfig::load_from(&dir.path().join("nope.yaml"));
        assert_eq!(c.wfb_relay.receiver_mdns_service, "_ados-receiver._tcp");
        assert_eq!(c.wfb_relay.receiver_port, 5800);
        assert_eq!(c.wfb_receiver.listen_port, 5800);
        assert!(c.wfb_receiver.accept_local_nic);
        assert_eq!(c.mesh.bat_iface, "bat0");
        // Atlas relay is inert by default.
        assert!(!c.atlas.enabled);
        assert_eq!(c.atlas.listen_port, 5603);
        assert!(c.atlas.compute_base_url.is_none());
    }

    #[test]
    fn reads_atlas_block() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "ground_station:\n  atlas:\n    enabled: true\n    listen_port: 5611\n    compute_base_url: http://10.0.0.9:8099\n",
        )
        .unwrap();
        let c = GroundStationConfig::load_from(&cfg);
        assert!(c.atlas.enabled);
        assert_eq!(c.atlas.listen_port, 5611);
        assert_eq!(
            c.atlas.compute_base_url.as_deref(),
            Some("http://10.0.0.9:8099")
        );
        // An atlas block with only `enabled` set falls back to the default port.
        std::fs::write(&cfg, "ground_station:\n  atlas:\n    enabled: true\n").unwrap();
        let c = GroundStationConfig::load_from(&cfg);
        assert!(c.atlas.enabled);
        assert_eq!(c.atlas.listen_port, 5603);
        assert!(c.atlas.compute_base_url.is_none());
    }

    #[test]
    fn reads_ground_station_block() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "ground_station:\n  wfb_relay:\n    receiver_port: 5900\n  wfb_receiver:\n    listen_port: 5901\n    accept_local_nic: false\n  mesh:\n    bat_iface: bat1\n",
        )
        .unwrap();
        let c = GroundStationConfig::load_from(&cfg);
        assert_eq!(c.wfb_relay.receiver_port, 5900);
        assert_eq!(c.wfb_receiver.listen_port, 5901);
        assert!(!c.wfb_receiver.accept_local_nic);
        assert_eq!(c.mesh.bat_iface, "bat1");
        // Unset relay mDNS service falls back to the default.
        assert_eq!(c.wfb_relay.receiver_mdns_service, "_ados-receiver._tcp");
    }

    #[test]
    fn other_blocks_dont_break_parse() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  profile: ground_station\nvideo:\n  wfb:\n    channel: 36\n",
        )
        .unwrap();
        let c = GroundStationConfig::load_from(&cfg);
        // No ground_station block → all defaults.
        assert_eq!(c.wfb_receiver.listen_port, 5800);
    }
}
