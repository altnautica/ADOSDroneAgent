//! The mesh invite bundle: what a relay receives on approval.
//!
//! Fields map 1:1 into `/etc/ados/mesh/` paths on the relay side. Ports
//! `pairing_manager.InviteBundle`: `pack()` is `json.dumps(sort_keys=True)` with
//! the binary key material hex-encoded, `unpack()` is the inverse.
//!
//! `sort_keys` is load-bearing for interop, so `pack()` builds a `BTreeMap`
//! (keys are emitted in deterministic alphabetical order, matching Python). The
//! plaintext is carried inside the ChaCha20Poly1305 AEAD, so the exact
//! inter-key whitespace is NOT authenticated and Python's `json.loads` parses
//! the compact form identically; the field set, types, and hex encoding are the
//! contract.

use std::collections::BTreeMap;

use serde_json::Value;

/// What a relay receives on approval. `mesh_psk` and `wfb_rx_key` are raw bytes;
/// they are hex-encoded in the packed JSON (matching Python `.hex()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteBundle {
    pub mesh_id: String,
    pub mesh_psk: Vec<u8>,
    pub drone_channel: i64,
    pub wfb_rx_key: Vec<u8>,
    pub receiver_mdns_host: String,
    pub receiver_mdns_port: i64,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
}

impl InviteBundle {
    /// Serialize to the packed JSON bytes (sorted keys, hex-encoded binaries).
    /// Mirrors `InviteBundle.pack`: `json.dumps(payload, sort_keys=True)`.
    pub fn pack(&self) -> Vec<u8> {
        // BTreeMap → deterministic alphabetical key order, matching sort_keys.
        let mut map: BTreeMap<&str, Value> = BTreeMap::new();
        map.insert("mesh_id", Value::String(self.mesh_id.clone()));
        map.insert("mesh_psk", Value::String(hex::encode(&self.mesh_psk)));
        map.insert("drone_channel", Value::from(self.drone_channel));
        map.insert("wfb_rx_key", Value::String(hex::encode(&self.wfb_rx_key)));
        map.insert(
            "receiver_mdns_host",
            Value::String(self.receiver_mdns_host.clone()),
        );
        map.insert("receiver_mdns_port", Value::from(self.receiver_mdns_port));
        map.insert("issued_at_ms", Value::from(self.issued_at_ms));
        map.insert("expires_at_ms", Value::from(self.expires_at_ms));
        serde_json::to_vec(&map).expect("BTreeMap<&str, Value> always serializes")
    }

    /// Parse the packed JSON bytes back into a bundle. Mirrors
    /// `InviteBundle.unpack`: `json.loads` + `bytes.fromhex`.
    pub fn unpack(blob: &[u8]) -> Result<InviteBundle, String> {
        let data: Value = serde_json::from_slice(blob).map_err(|e| e.to_string())?;
        let mesh_id = str_field(&data, "mesh_id")?;
        let mesh_psk = hex_field(&data, "mesh_psk")?;
        let drone_channel = int_field(&data, "drone_channel")?;
        let wfb_rx_key = hex_field(&data, "wfb_rx_key")?;
        let receiver_mdns_host = str_field(&data, "receiver_mdns_host")?;
        let receiver_mdns_port = int_field(&data, "receiver_mdns_port")?;
        let issued_at_ms = int_field(&data, "issued_at_ms")?;
        let expires_at_ms = int_field(&data, "expires_at_ms")?;
        Ok(InviteBundle {
            mesh_id,
            mesh_psk,
            drone_channel,
            wfb_rx_key,
            receiver_mdns_host,
            receiver_mdns_port,
            issued_at_ms,
            expires_at_ms,
        })
    }
}

fn str_field(v: &Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing/invalid string field {key}"))
}

/// Parse an integer field, tolerating a numeric JSON value (Python writes ints
/// for the *_ms / channel / port fields).
fn int_field(v: &Value, key: &str) -> Result<i64, String> {
    v.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("missing/invalid int field {key}"))
}

fn hex_field(v: &Value, key: &str) -> Result<Vec<u8>, String> {
    let s = str_field(v, key)?;
    hex::decode(&s).map_err(|e| format!("bad hex in field {key}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> InviteBundle {
        InviteBundle {
            mesh_id: "ados-abc123def4".into(),
            mesh_psk: vec![0xAB; 32],
            drone_channel: 149,
            wfb_rx_key: vec![0xCD; 64],
            receiver_mdns_host: "gs-recv.local".into(),
            receiver_mdns_port: 5800,
            issued_at_ms: 1_700_000_000_000,
            expires_at_ms: 1_700_000_120_000,
        }
    }

    #[test]
    fn pack_unpack_round_trips() {
        let b = sample();
        let packed = b.pack();
        let back = InviteBundle::unpack(&packed).unwrap();
        assert_eq!(b, back);
    }

    #[test]
    fn packed_keys_are_sorted_and_hex_encoded() {
        let packed = sample().pack();
        let s = String::from_utf8(packed).unwrap();
        // Alphabetical key order (sort_keys parity): drone_channel first.
        let drone = s.find("drone_channel").unwrap();
        let mesh = s.find("mesh_id").unwrap();
        let wfb = s.find("wfb_rx_key").unwrap();
        assert!(drone < mesh, "drone_channel must precede mesh_id");
        assert!(mesh < wfb, "mesh_id must precede wfb_rx_key");
        // Binary fields are hex strings, not arrays.
        assert!(s.contains(&format!("\"mesh_psk\":\"{}\"", "ab".repeat(32))));
        assert!(s.contains(&format!("\"wfb_rx_key\":\"{}\"", "cd".repeat(64))));
    }

    #[test]
    fn unpack_parses_a_python_style_spaced_bundle() {
        // Python `json.dumps(..., sort_keys=True)` uses `", "` / `": "` spacing.
        // The relay must parse that verbatim (the AEAD plaintext carries the
        // exact bytes Python produced). This is the interop direction.
        let python_json = concat!(
            "{",
            "\"drone_channel\": 149, ",
            "\"expires_at_ms\": 1700000120000, ",
            "\"issued_at_ms\": 1700000000000, ",
            "\"mesh_id\": \"ados-abc123def4\", ",
            "\"mesh_psk\": \"",
            // 32 bytes of 0xAB
            "abababababababababababababababababababababababababababababababab\", ",
            "\"receiver_mdns_host\": \"gs-recv.local\", ",
            "\"receiver_mdns_port\": 5800, ",
            "\"wfb_rx_key\": \"",
            // 64 bytes of 0xCD
            "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
            "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd\"",
            "}"
        );
        let b = InviteBundle::unpack(python_json.as_bytes()).expect("parses python json");
        assert_eq!(b, sample());
    }
}
