//! What a beacon says about this node: the vehicle state from the MAVLink
//! service's state socket when it carries a position, else this node's own
//! GNSS receiver from the GNSS service's sidecar (looked up in the sidecar
//! registry, so a build without the GNSS service simply has none).

use std::time::Duration;

use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::frame::Beacon;

const STATE_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// One vehicle-state snapshot (the router pushes the last-known one on
/// connect), or `None` when the socket is absent or silent.
async fn read_state() -> Option<Value> {
    let path = ados_protocol::paths::run_dir().join(ados_protocol::paths::STATE_SOCK);
    let stream = UnixStream::connect(path).await.ok()?;
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    tokio::time::timeout(
        STATE_READ_TIMEOUT,
        ados_protocol::state::read_state_value_into(&mut reader, &mut buf),
    )
    .await
    .ok()?
    .ok()?
}

/// A beacon from a vehicle-state snapshot, when it has a position fix.
pub fn from_state(state: &Value) -> Option<Beacon> {
    let pos = state.get("position")?;
    let lat = pos.get("lat")?.as_f64()?;
    let lon = pos.get("lon")?.as_f64()?;
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    Some(Beacon {
        position: Some((lat, lon)),
        alt_m: pos.get("alt_msl").and_then(Value::as_f64),
        battery_pct: state
            .pointer("/battery/remaining")
            .and_then(Value::as_f64)
            .filter(|v| (0.0..=100.0).contains(v))
            .map(|v| v.round() as u8),
        mode: state
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// A beacon from the GNSS sidecar's first receiver with a fix.
pub fn from_gnss(gnss: &Value) -> Option<Beacon> {
    let rx = gnss.get("receivers")?.as_array()?.iter().find(|r| {
        r.get("fix")
            .and_then(Value::as_str)
            .is_some_and(|f| f != "none")
    })?;
    Some(Beacon {
        position: Some((rx.get("lat_deg")?.as_f64()?, rx.get("lon_deg")?.as_f64()?)),
        alt_m: rx.get("h_msl_m").and_then(Value::as_f64),
        battery_pct: None,
        mode: "GS".into(),
    })
}

fn read_gnss() -> Option<Value> {
    let row = ados_protocol::paths::sidecar("gnss")?;
    let text = std::fs::read_to_string(ados_protocol::paths::resolve(row.path)).ok()?;
    serde_json::from_str(&text).ok()
}

/// This node's beacon now. A node with no position still beacons (its
/// presence and battery are still news), with the position fields empty.
pub async fn current() -> Beacon {
    if let Some(b) = read_state().await.as_ref().and_then(from_state) {
        return b;
    }
    read_gnss().as_ref().and_then(from_gnss).unwrap_or(Beacon {
        position: None,
        alt_m: None,
        battery_pct: None,
        mode: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_with_a_fix_becomes_a_beacon() {
        let s = json!({"mode": "AUTO", "position": {"lat": 12.97, "lon": 77.59, "alt_msl": 920.4},
                       "battery": {"remaining": 81}});
        let b = from_state(&s).unwrap();
        assert_eq!(b.mode, "AUTO");
        assert_eq!(b.battery_pct, Some(81));
        assert_eq!(b.position, Some((12.97, 77.59)));
        assert!(from_state(&json!({"position": {"lat": 0.0, "lon": 0.0}})).is_none());
    }

    #[test]
    fn gnss_sidecar_receiver_becomes_a_beacon() {
        let g =
            json!({"receivers": [{"fix": "3d", "lat_deg": 1.5, "lon_deg": 2.5, "h_msl_m": 10.0}]});
        let b = from_gnss(&g).unwrap();
        assert_eq!(b.mode, "GS");
        assert_eq!(b.alt_m, Some(10.0));
        assert!(from_gnss(&json!({"receivers": [{"fix": "none"}]})).is_none());
    }
}
