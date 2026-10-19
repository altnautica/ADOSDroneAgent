//! The link state shared between the session and the sidecar writer, and the
//! `/run/ados/sensorhub.json` body built from it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// Schema version of the `sensorhub` sidecar (registry row `sensorhub`).
pub const SENSORHUB_SIDECAR_VERSION: u16 = 1;

/// The sidecar path (`ADOS_RUN_DIR` aware).
pub fn sidecar_path() -> PathBuf {
    ados_protocol::paths::run_path("sensorhub")
}

/// Identity from the hub's `Hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloInfo {
    pub fw: String,
    pub board: String,
    pub proto_ver: u16,
    pub caps: u32,
}

/// Everything the service knows about the link. Counters are cumulative over
/// the service lifetime.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HubState {
    pub connected: bool,
    /// `serial:/dev/ttyACM0` or `tcp:host:port`; empty until a transport is
    /// resolved.
    pub transport: String,
    pub hello: Option<HelloInfo>,
    pub uptime_s: Option<u32>,
    pub crc_errors: u64,
    pub dropped: u64,
    pub imu_samples: u64,
    pub triggers: u64,
    /// `Pps.locked` / `Health.pps_state` (0 none … 3 holdover), once reported.
    pub pps_state: Option<u8>,
}

pub type SharedState = Arc<Mutex<HubState>>;

/// The sidecar body; field order is the wire order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SidecarBody {
    pub version: u16,
    pub connected: bool,
    pub transport: String,
    pub fw: Option<String>,
    pub board: Option<String>,
    pub proto_ver: Option<u16>,
    pub uptime_s: Option<u32>,
    pub crc_errors: u64,
    pub dropped: u64,
    pub imu_odr_hz: Option<f64>,
    pub trigger_hz: Option<f64>,
}

/// Turns successive state snapshots into sidecar bodies, measuring the IMU
/// sample and trigger rates between them.
#[derive(Debug, Default)]
pub struct RateMeter {
    last: Option<(u64, u64, u64)>,
}

impl RateMeter {
    /// The body for `s` taken at host time `now_ns`. Rates are `null` while
    /// disconnected or until a previous snapshot exists.
    pub fn body(&mut self, s: &HubState, now_ns: u64) -> SidecarBody {
        let prev = self.last.replace((now_ns, s.imu_samples, s.triggers));
        let rates = prev.filter(|_| s.connected).and_then(|(t0, imu0, trig0)| {
            let dt = now_ns.checked_sub(t0).filter(|d| *d > 0)? as f64 / 1e9;
            Some((
                s.imu_samples.saturating_sub(imu0) as f64 / dt,
                s.triggers.saturating_sub(trig0) as f64 / dt,
            ))
        });
        let hello = s.hello.as_ref();
        SidecarBody {
            version: SENSORHUB_SIDECAR_VERSION,
            connected: s.connected,
            transport: s.transport.clone(),
            fw: hello.map(|h| h.fw.clone()),
            board: hello.map(|h| h.board.clone()),
            proto_ver: hello.map(|h| h.proto_ver),
            uptime_s: s.uptime_s,
            crc_errors: s.crc_errors,
            dropped: s.dropped,
            imu_odr_hz: rates.map(|r| r.0),
            trigger_hz: rates.map(|r| r.1),
        }
    }
}

/// Write the body atomically.
pub fn write_sidecar(path: &Path, body: &SidecarBody) -> std::io::Result<()> {
    ados_protocol::sidecar::write_json_atomic(path, body, 0o644)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: [&str; 11] = [
        "version",
        "connected",
        "transport",
        "fw",
        "board",
        "proto_ver",
        "uptime_s",
        "crc_errors",
        "dropped",
        "imu_odr_hz",
        "trigger_hz",
    ];

    #[test]
    fn the_body_has_the_contract_keys_in_order_and_measures_rates() {
        let mut meter = RateMeter::default();
        let mut s = HubState {
            connected: true,
            transport: "tcp:127.0.0.1:7600".into(),
            ..HubState::default()
        };
        let first = meter.body(&s, 1_000_000_000);
        assert_eq!(first.imu_odr_hz, None);
        assert_eq!(first.fw, None);
        let text = serde_json::to_string(&first).unwrap();
        let positions: Vec<usize> = KEYS
            .iter()
            .map(|k| text.find(&format!("\"{k}\":")).unwrap())
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v.as_object().unwrap().len(), KEYS.len());

        s.imu_samples = 2_000;
        s.triggers = 40;
        s.hello = Some(HelloInfo {
            fw: "ados-sensorhub 0.1.0".into(),
            board: "sim".into(),
            proto_ver: 1,
            caps: 0,
        });
        let second = meter.body(&s, 3_000_000_000);
        assert_eq!(second.imu_odr_hz, Some(1000.0));
        assert_eq!(second.trigger_hz, Some(20.0));
        assert_eq!(second.proto_ver, Some(1));

        s.connected = false;
        assert_eq!(meter.body(&s, 4_000_000_000).imu_odr_hz, None);
    }

    #[test]
    fn the_sidecar_lands_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sensorhub.json");
        let body = RateMeter::default().body(&HubState::default(), 1);
        write_sidecar(&path, &body).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["connected"], false);
        assert!(v["fw"].is_null());
    }
}
