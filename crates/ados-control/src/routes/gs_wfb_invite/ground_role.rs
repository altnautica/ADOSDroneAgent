//! The ground-role half of an invite bundle (version 2): what a phone needs to
//! stand in for this ground station as the fleet's transmitting ground.
//!
//! Version 1 handed a phone receive material only. Version 2 adds, under
//! `ground`:
//!
//! - every registered drone's slot, device id, system id and per-pair relay
//!   secret, so the phone mints relay tickets the drones accept and never
//!   re-enrols or renumbers an aircraft;
//! - this box's transmit parameters (`mcs_index`, `aux_tx_port`, TX power,
//!   regulatory domain and DFS posture), so the phone's planes match the box's
//!   and it radiates under the same regulatory posture;
//! - this box's device id, name and LAN address, and the uplink key the phone
//!   authenticates its uplink handover requests with (and the box its
//!   take-back calls), so the two can hand the uplink back and forth.

use base64::Engine as _;
use serde_json::{json, Value};

/// One registered drone as the phone needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroundDrone {
    pub slot: u8,
    pub device_id: String,
    /// The MAVLink system id the fleet policy assigns this slot.
    pub sysid: Option<u8>,
    /// The per-pair relay secret, hex, or `None` for a registration that has
    /// none yet (its Request-lane calls stay refused until it is issued).
    pub relay_secret_hex: Option<String>,
}

/// The ground-role material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroundRole {
    pub box_device_id: String,
    pub box_name: String,
    /// The address the phone reached this box on, when known.
    pub box_lan_address: Option<String>,
    pub mcs_index: u8,
    pub aux_tx_port: u16,
    pub tx_power_dbm: i8,
    pub reg_domain: Option<String>,
    pub dfs_allowed: bool,
    pub drones: Vec<GroundDrone>,
    /// The key the phone's uplink requests are authenticated with.
    pub uplink_key: [u8; 32],
}

impl GroundRole {
    /// The bundle's `ground` object.
    pub fn to_json(&self) -> Value {
        let drones: Vec<Value> = self
            .drones
            .iter()
            .map(|d| {
                json!({
                    "slot": d.slot,
                    "device_id": d.device_id,
                    "sysid": d.sysid,
                    "relay_secret_hex": d.relay_secret_hex,
                })
            })
            .collect();
        json!({
            "box_device_id": self.box_device_id,
            "box_name": self.box_name,
            "box_lan_address": self.box_lan_address,
            "mcs_index": self.mcs_index,
            "aux_tx_port": self.aux_tx_port,
            "tx_power_dbm": self.tx_power_dbm,
            "regulatory": {"domain": self.reg_domain, "dfs_allowed": self.dfs_allowed},
            "drones": drones,
            "uplink_key_b64": base64::engine::general_purpose::STANDARD.encode(self.uplink_key),
        })
    }
}

/// The drones of the box's fleet registry, as the ground role carries them.
pub fn drones_of(registry: &ados_groundlink::FleetRegistry) -> Vec<GroundDrone> {
    registry
        .slots()
        .map(|s| GroundDrone {
            slot: s.slot,
            device_id: s.device_id.clone(),
            sysid: ados_groundlink::fleet_identity_policy::system_id_for_slot(s.slot),
            relay_secret_hex: s.relay_secret.clone(),
        })
        .collect()
}
