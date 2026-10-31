//! Where the mesh pairing service reads and writes: the persistent mesh
//! directory, the run-dir journal, the agent config, and the systemd hook it
//! restarts the mesh bring-up through. One value carries every path so tests
//! run the whole service against a temp tree.

use std::path::{Path, PathBuf};

use crate::gs_config::GroundStationConfig;
use crate::paths;

use super::invite::InviteBundle;
use super::{INVITE_TTL_S, PAIR_UDP_PORT};

/// The unit the mesh bring-up runs as; restarted after a re-key so it re-reads
/// the rotated identity.
pub const BATMAN_UNIT: &str = "ados-batman";

/// Every path and hook the pairing service touches.
#[derive(Debug, Clone)]
pub struct PairingEnv {
    /// The agent config file (`ground_station:`, `agent.device_id`, `video.wfb`).
    pub config_path: PathBuf,
    /// Persistent mesh state: revocations, paired relays, the relay re-key file.
    pub mesh_dir: PathBuf,
    /// The deployment mesh id file.
    pub mesh_id_path: PathBuf,
    /// Where a relay persists the PSK an invite delivers.
    pub mesh_psk_path: PathBuf,
    /// The receiver hint a relay persists from an invite.
    pub receiver_info_path: PathBuf,
    /// The drone-paired WFB rx key (sealed into invites, persisted by relays).
    pub wfb_rx_key_path: PathBuf,
    /// The cross-process pair-event journal.
    pub pair_events_path: PathBuf,
    /// The kernel neighbour table read for a relay's mesh MAC.
    pub arp_table: PathBuf,
    /// The UDP port the accept window listens on for join requests.
    pub udp_port: u16,
    /// The `systemctl` binary the batman restart runs.
    pub systemctl: String,
}

/// Config values read fresh on each use, so an operator edit applies without a
/// restart.
#[derive(Debug, Clone)]
pub struct PairingSettings {
    pub device_id: String,
    pub psk_path: PathBuf,
    pub bat_iface: String,
    pub receiver_mdns_service: String,
    pub receiver_listen_port: u16,
    pub drone_channel: u8,
}

impl PairingEnv {
    /// The on-box layout.
    pub fn live() -> Self {
        Self {
            config_path: PathBuf::from(paths::CONFIG_YAML),
            mesh_dir: PathBuf::from(paths::MESH_DIR),
            mesh_id_path: PathBuf::from(paths::MESH_ID_PATH),
            mesh_psk_path: PathBuf::from(paths::MESH_PSK_PATH),
            receiver_info_path: PathBuf::from(paths::MESH_RECEIVER_JSON),
            wfb_rx_key_path: PathBuf::from(ados_radio::paths::WFB_RX_KEY),
            pair_events_path: PathBuf::from(paths::run_path("pair-events.jsonl")),
            arp_table: PathBuf::from("/proc/net/arp"),
            udp_port: PAIR_UDP_PORT,
            systemctl: "systemctl".to_string(),
        }
    }

    /// A layout rooted at `root`, with the config at `root/config.yaml`.
    pub fn rooted(root: &Path) -> Self {
        let mesh = root.join("mesh");
        Self {
            config_path: root.join("config.yaml"),
            mesh_id_path: mesh.join("id"),
            mesh_psk_path: mesh.join("psk.key"),
            receiver_info_path: mesh.join("receiver.json"),
            mesh_dir: mesh,
            wfb_rx_key_path: root.join("wfb/rx.key"),
            pair_events_path: root.join("pair-events.jsonl"),
            arp_table: root.join("arp"),
            udp_port: PAIR_UDP_PORT,
            systemctl: "systemctl".to_string(),
        }
    }

    /// The revocation list.
    pub fn revocations_path(&self) -> PathBuf {
        self.mesh_dir.join("revocations.json")
    }

    /// The current config values.
    pub fn settings(&self) -> PairingSettings {
        let gs = GroundStationConfig::load_from(&self.config_path);
        let wfb = ados_radio::config::WfbConfig::load_from(&self.config_path);
        PairingSettings {
            device_id: agent_device_id(&self.config_path),
            psk_path: PathBuf::from(gs.mesh.shared_key_path),
            bat_iface: gs.mesh.bat_iface,
            receiver_mdns_service: gs.wfb_relay.receiver_mdns_service,
            receiver_listen_port: gs.wfb_receiver.listen_port,
            drone_channel: wfb.channel,
        }
    }

    /// The mesh id file and the configured PSK file a re-key rotates.
    pub fn identity_paths(&self) -> (PathBuf, PathBuf) {
        (self.settings().psk_path, self.mesh_id_path.clone())
    }

    /// The invite bundle for this deployment, from the mesh identity on disk and
    /// the config. `None` while the mesh identity has not been created.
    pub fn build_invite_bundle(&self) -> Option<InviteBundle> {
        let settings = self.settings();
        let mesh_id = std::fs::read_to_string(&self.mesh_id_path).ok()?;
        let psk = std::fs::read(&settings.psk_path).ok()?;
        let wfb_rx_key = std::fs::read(&self.wfb_rx_key_path).unwrap_or_default();
        let now = crate::mesh_events::now_ms();
        Some(InviteBundle {
            mesh_id: mesh_id.trim().to_string(),
            mesh_psk: psk.trim_ascii().to_vec(),
            drone_channel: i64::from(settings.drone_channel),
            wfb_rx_key,
            receiver_mdns_host: hostname(),
            receiver_mdns_port: i64::from(settings.receiver_listen_port),
            issued_at_ms: now,
            expires_at_ms: now + INVITE_TTL_S * 1000,
        })
    }
}

/// This node's hostname, the name relays resolve the receiver by.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_default()
}

/// `agent.device_id` from the config file, empty when unset or unreadable.
pub fn agent_device_id(config_path: &Path) -> String {
    if !config_path.exists() {
        return String::new();
    }
    ados_config::schema::load_slice::<Option<String>>(config_path, "/agent/device_id")
        .ok()
        .flatten()
        .unwrap_or_default()
}
