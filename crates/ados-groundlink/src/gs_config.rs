//! Ground-station relay/receiver/mesh configuration, read from the
//! `ground_station:` block of `/etc/ados/config.yaml`.
//!
//! The sibling `ados_radio::config::WfbConfig` covers only the `video.wfb:`
//! block (the radio link). The relay and receiver loops need a few extra
//! fields that live under `ground_station:` in Python (`WfbRelayConfig`,
//! `WfbReceiverConfig`, `MeshConfig`). Field names and defaults mirror the
//! Python dataclasses so a node reads the same values regardless of which
//! language drives the loop.

use serde::{Deserialize, Serialize};

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
fn default_carrier() -> String {
    "802.11s".to_string()
}
fn default_shared_key_path() -> String {
    crate::paths::MESH_PSK_PATH.to_string()
}
fn default_mesh_channel() -> u8 {
    1
}
fn default_cloud_uplink() -> String {
    "auto".to_string()
}

/// Relay-role config (`ground_station.wfb_relay`).
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Deserialize, Serialize)]
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

/// Mesh fabric config (`ground_station.mesh`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeshConfig {
    /// Operator pin for the mesh carrier adapter; wins over every resolver.
    #[serde(default)]
    pub interface_override: Option<String>,
    /// The L2 carrier: `802.11s` (preferred) or `ibss`.
    #[serde(default = "default_carrier")]
    pub carrier: String,
    /// Operator-pinned deployment mesh id; empty/absent uses the id file.
    #[serde(default)]
    pub mesh_id: Option<String>,
    /// Where the deployment mesh PSK lives (0600 raw bytes).
    #[serde(default = "default_shared_key_path")]
    pub shared_key_path: String,
    /// 2.4 GHz carrier channel, 1-13.
    #[serde(default = "default_mesh_channel")]
    pub channel: u8,
    /// The batman-adv carrier interface (default `bat0`).
    #[serde(default = "default_bat_iface")]
    pub bat_iface: String,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            interface_override: None,
            carrier: default_carrier(),
            mesh_id: None,
            shared_key_path: default_shared_key_path(),
            channel: default_mesh_channel(),
            bat_iface: default_bat_iface(),
        }
    }
}

/// The `ground_station:` config subset the ground-station services consume.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GroundStationConfig {
    /// Whether this node advertises a batman-adv gateway: `auto`, `force_on`
    /// or `force_off`.
    #[serde(default = "default_cloud_uplink")]
    pub cloud_uplink: String,
    #[serde(default)]
    pub wfb_relay: WfbRelayConfig,
    #[serde(default)]
    pub wfb_receiver: WfbReceiverConfig,
    #[serde(default)]
    pub mesh: MeshConfig,
}

impl Default for GroundStationConfig {
    fn default() -> Self {
        Self {
            cloud_uplink: default_cloud_uplink(),
            wfb_relay: WfbRelayConfig::default(),
            wfb_receiver: WfbReceiverConfig::default(),
            mesh: MeshConfig::default(),
        }
    }
}

/// JSON pointer of the `ground_station` slice in the agent config.
pub const CONFIG_POINTER: &str = "/ground_station";

impl GroundStationConfig {
    /// Load from the `ground_station:` block in the agent config file. A missing
    /// or unparseable file yields the Python defaults. Does NOT publish the
    /// config-status sidecar — the runtime consumers (relay / receiver)
    /// and tests call this, so the write is confined to
    /// [`publish_config_status`](Self::publish_config_status) at service startup.
    pub fn load_from(path: &std::path::Path) -> Self {
        Self::load_reporting(path).0
    }

    /// Like [`load_from`](Self::load_from) but also returns the parse-error
    /// message so the startup path can surface it. `None` on success or a
    /// missing/unreadable file; `Some(msg)` on a present-but-malformed file.
    ///
    /// The slice is read from the effective config (schema defaults overlaid
    /// with the file), so an absent key is the schema default.
    pub fn load_reporting(path: &std::path::Path) -> (Self, Option<String>) {
        if !path.exists() {
            return (GroundStationConfig::default(), None);
        }
        match ados_config::schema::load_slice::<GroundStationConfig>(path, CONFIG_POINTER) {
            Ok(cfg) => (cfg, None),
            Err(e) => (GroundStationConfig::default(), Some(e)),
        }
    }

    /// Read the config once at service startup and publish its config-status
    /// sidecar (a null error when the block is valid) so a malformed
    /// `ground_station:` block is visible on the remote Health surface, not only
    /// in the log. Best-effort via `write_config_status`; never blocks startup.
    pub fn publish_config_status(path: &std::path::Path) {
        let (_, error) = Self::load_reporting(path);
        ados_config::write_config_status("ground_station", error.as_deref());
    }
}

#[cfg(test)]
mod tests;
