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

    /// Ship the snapshot to the logging store as a single full-snapshot
    /// `mesh.state` event, mirroring the wfb-status producer: the SAME body the
    /// `write()` above persists rides one event so the durable read source and
    /// the on-disk sidecar stay in lockstep. The detail map carries every
    /// snapshot key, including the nested `neighbors` / `gateways` arrays which
    /// round-trip through `json_object_to_fields`. Best-effort: an absent
    /// logging daemon drops the event without disturbing the poll loop.
    pub fn emit(&self, ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>) {
        if let Some(em) = ingest {
            let v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
            em.emit_event(
                "mesh.state",
                ados_protocol::logd::Level::Info,
                crate::wfb_rx::stats::json_object_to_fields(&v),
            );
        }
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

    #[test]
    fn snapshot_body_round_trips_through_the_event_detail_map() {
        // The body shipped to the store must decode back to the identical JSON
        // the sidecar writes, so the durable read source matches the live
        // fallback. The nested neighbor/gateway arrays + the null
        // selected_gateway are the at-risk legs.
        let mut snap = MeshSnapshot::new("relay", "bat0", "802.11s");
        snap.mesh_iface = "wlan1".into();
        snap.mesh_id = "ados-xyz".into();
        snap.up = true;
        snap.last_poll_ms = 1_700_000_000_000;
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

        let body = serde_json::to_value(&snap).unwrap();
        let fields = crate::wfb_rx::stats::json_object_to_fields(&body);
        use ados_protocol::frame::{decode_len, HEADER_SIZE};
        use ados_protocol::logd::{EventFrame, IngestFrame, Level, LOGD_MAX_FRAME};
        let mut frame = EventFrame::new(0, "mesh.state", "ados-groundlink", Level::Info);
        frame.detail = fields;
        let bytes = IngestFrame::Event(frame).encode().unwrap();
        let header: [u8; HEADER_SIZE] = bytes[..HEADER_SIZE].try_into().unwrap();
        let len = decode_len(header, LOGD_MAX_FRAME, true).unwrap();
        let decoded = match IngestFrame::decode(&bytes[HEADER_SIZE..HEADER_SIZE + len]).unwrap() {
            IngestFrame::Event(e) => e,
            other => panic!("expected an event frame, got {other:?}"),
        };
        let back = serde_json::to_value(decoded.detail).unwrap();
        assert_eq!(back, body);
        // The nested arrays + a null gateway survive the round-trip.
        assert_eq!(back["neighbors"][0]["mac"], "aa:bb:cc:dd:ee:ff");
        assert_eq!(back["gateways"][0]["class_up_kbps"], 10000);
        assert_eq!(back["selected_gateway"], "11:22:33:44:55:66");
    }

    #[tokio::test]
    async fn emit_enqueues_one_event_with_an_emitter_and_none_without() {
        // Passing an emitter ships exactly one mesh.state event; passing None
        // enqueues nothing. The emitter records every enqueue regardless of
        // whether a daemon is listening.
        let dir = tempfile::tempdir().unwrap();
        let snap = MeshSnapshot::new("receiver", "bat0", "802.11s");

        let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-groundlink",
            dir.path().join("ingest.sock"),
        );
        let stats = emitter.stats();
        snap.emit(Some(&emitter));
        assert_eq!(stats.enqueued(), 1);

        let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-groundlink",
            dir.path().join("ingest2.sock"),
        );
        let none_stats = none_emitter.stats();
        snap.emit(None);
        assert_eq!(none_stats.enqueued(), 0);
    }
}
