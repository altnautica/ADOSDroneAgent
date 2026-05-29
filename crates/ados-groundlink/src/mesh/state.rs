//! Mesh snapshot types + the `mesh-state.json` writer.
//!
//! Ports the `MeshSnapshot`/`MeshNeighbor`/`MeshGateway` dataclasses and
//! `_write_state_json` from `mesh_manager.py`. The JSON shape is byte-identical
//! to the Python writer (same key names + nesting) so the REST layer and OLED
//! that read the file cross-process see the same surface. Atomic tmp + rename.

use serde::Serialize;

use crate::paths::MESH_STATE_JSON;

/// One batman-adv neighbor (a row of `batctl n -H`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MeshNeighbor {
    pub mac: String,
    pub iface: String,
    pub tq: i64,
    pub last_seen_ms: i64,
}

/// One batman-adv gateway (a row of `batctl gwl -H`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MeshGateway {
    pub mac: String,
    pub class_up_kbps: i64,
    pub class_down_kbps: i64,
    pub tq: i64,
    pub selected: bool,
}

/// The live mesh snapshot the poll loop maintains and persists.
#[derive(Debug, Clone, Serialize)]
pub struct MeshSnapshot {
    pub role: String,
    pub bat_iface: String,
    pub mesh_iface: String,
    pub carrier: String,
    pub mesh_id: String,
    pub up: bool,
    pub neighbors: Vec<MeshNeighbor>,
    pub gateways: Vec<MeshGateway>,
    pub selected_gateway: Option<String>,
    pub partition: bool,
    pub started_at_ms: i64,
    pub last_poll_ms: i64,
}

impl MeshSnapshot {
    /// A fresh snapshot for `role` with the configured interfaces, before
    /// bringup (matching the Python constructor defaults).
    pub fn new(role: &str, bat_iface: &str, carrier: &str) -> Self {
        Self {
            role: role.to_string(),
            bat_iface: bat_iface.to_string(),
            mesh_iface: String::new(),
            carrier: carrier.to_string(),
            mesh_id: String::new(),
            up: false,
            neighbors: Vec::new(),
            gateways: Vec::new(),
            selected_gateway: None,
            partition: false,
            started_at_ms: 0,
            last_poll_ms: 0,
        }
    }

    /// Write the snapshot to `mesh-state.json` (Contract-E path) atomically.
    /// Best-effort: an I/O error is logged by the caller, never fatal.
    pub fn write(&self) -> std::io::Result<()> {
        let path = std::path::Path::new(MESH_STATE_JSON);
        crate::sidecars::write_json_atomic(path, self, 0o644)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_json_shape_matches_python_keys() {
        let mut snap = MeshSnapshot::new("receiver", "bat0", "802.11s");
        snap.mesh_iface = "wlan1".into();
        snap.mesh_id = "ados-abc".into();
        snap.up = true;
        snap.neighbors.push(MeshNeighbor {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            iface: "wlan1".into(),
            tq: 240,
            last_seen_ms: 1234,
        });
        snap.gateways.push(MeshGateway {
            mac: "11:22:33:44:55:66".into(),
            class_up_kbps: 10000,
            class_down_kbps: 2000,
            tq: 255,
            selected: true,
        });
        snap.selected_gateway = Some("11:22:33:44:55:66".into());

        let v = serde_json::to_value(&snap).unwrap();
        for k in [
            "role",
            "bat_iface",
            "mesh_iface",
            "carrier",
            "mesh_id",
            "up",
            "neighbors",
            "gateways",
            "selected_gateway",
            "partition",
            "started_at_ms",
            "last_poll_ms",
        ] {
            assert!(v.get(k).is_some(), "missing key {k}");
        }
        // Nested neighbor/gateway dicts carry the dataclass `__dict__` keys.
        let n = &v["neighbors"][0];
        for k in ["mac", "iface", "tq", "last_seen_ms"] {
            assert!(n.get(k).is_some(), "missing neighbor key {k}");
        }
        let g = &v["gateways"][0];
        for k in ["mac", "class_up_kbps", "class_down_kbps", "tq", "selected"] {
            assert!(g.get(k).is_some(), "missing gateway key {k}");
        }
        assert_eq!(v["role"], "receiver");
        assert_eq!(v["selected_gateway"], "11:22:33:44:55:66");
    }
}
