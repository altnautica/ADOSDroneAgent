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
    /// Identity from this session's `Hello`; cleared when a session starts.
    pub hello: Option<HelloInfo>,
    pub uptime_s: Option<u32>,
    pub crc_errors: u64,
    pub dropped: u64,
    /// IMU samples received in the current session.
    pub imu_samples: u64,
    pub triggers: u64,
    /// `Pps.locked` / `Health.pps_state` (0 none … 3 holdover), once reported.
    pub pps_state: Option<u8>,
    /// The per-IMU output data rate this session configured the hub with.
    pub imu_odr_hz: Option<u16>,
    /// The camera trigger rate this session configured the hub with (0 = off).
    pub trigger_hz: Option<u16>,
}

impl HubState {
    /// A session opened with the configured IMU and trigger rates: nothing
    /// learnt from an earlier session is current.
    pub fn begin_session(&mut self, imu_odr_hz: u16, trigger_hz: u16) {
        self.connected = true;
        self.hello = None;
        self.uptime_s = None;
        self.pps_state = None;
        self.imu_samples = 0;
        self.imu_odr_hz = Some(imu_odr_hz);
        self.trigger_hz = Some(trigger_hz);
    }

    /// The session ended: the link facts stop being live.
    pub fn end_session(&mut self) {
        self.connected = false;
        self.uptime_s = None;
        self.imu_odr_hz = None;
        self.trigger_hz = None;
    }
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
    /// The measured trigger rate between the last two snapshots.
    pub trigger_hz: Option<f64>,
    /// The trigger rate the session configured the hub with.
    pub trigger_hz_configured: Option<f64>,
}

/// Turns successive state snapshots into sidecar bodies, measuring the
/// trigger rate between them.
#[derive(Debug, Default)]
pub struct RateMeter {
    last: Option<(u64, u64)>,
}

impl RateMeter {
    /// The body for `s` taken at host time `now_ns`. `imu_odr_hz` is the
    /// configured per-IMU rate once IMU samples flow this session;
    /// `trigger_hz` is the measured trigger rate and `trigger_hz_configured`
    /// the rate the hub was told to trigger at. All are `null` while
    /// disconnected.
    pub fn body(&mut self, s: &HubState, now_ns: u64) -> SidecarBody {
        let prev = self.last.replace((now_ns, s.triggers));
        let trigger_hz = prev.filter(|_| s.connected).and_then(|(t0, trig0)| {
            let dt = now_ns.checked_sub(t0).filter(|d| *d > 0)? as f64 / 1e9;
            Some(s.triggers.saturating_sub(trig0) as f64 / dt)
        });
        let imu_odr_hz = s
            .imu_odr_hz
            .filter(|_| s.connected && s.imu_samples > 0)
            .map(f64::from);
        let trigger_hz_configured = s.trigger_hz.filter(|_| s.connected).map(f64::from);
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
            imu_odr_hz,
            trigger_hz,
            trigger_hz_configured,
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

    const KEYS: [&str; 12] = [
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
        "trigger_hz_configured",
    ];

    #[test]
    fn the_body_has_the_contract_keys_in_order_and_reports_rates() {
        let mut meter = RateMeter::default();
        let mut s = HubState {
            transport: "tcp:127.0.0.1:7600".into(),
            ..HubState::default()
        };
        s.begin_session(1000, 20);
        let first = meter.body(&s, 1_000_000_000);
        // No IMU sample yet: no rate claimed.
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

        // Two IMUs at 1 kHz each: the per-IMU rate, not the 2 kHz sum.
        s.imu_samples = 4_000;
        s.triggers = 40;
        s.hello = Some(HelloInfo {
            fw: "ados-sensorhub 0.1.0".into(),
            board: "sim".into(),
            proto_ver: 1,
            caps: 0,
        });
        s.uptime_s = Some(9);
        let second = meter.body(&s, 3_000_000_000);
        assert_eq!(second.imu_odr_hz, Some(1000.0));
        assert_eq!(second.trigger_hz, Some(20.0));
        assert_eq!(second.trigger_hz_configured, Some(20.0));
        assert_eq!(second.proto_ver, Some(1));

        s.end_session();
        let down = meter.body(&s, 4_000_000_000);
        assert_eq!(
            (down.imu_odr_hz, down.trigger_hz, down.trigger_hz_configured),
            (None, None, None)
        );
        assert_eq!(down.uptime_s, None);

        // A new session forgets the previous hub's identity.
        s.begin_session(2000, 30);
        let next = meter.body(&s, 5_000_000_000);
        assert!(next.connected);
        assert_eq!((next.fw, next.board, next.proto_ver), (None, None, None));
        assert_eq!(next.imu_odr_hz, None);
    }

    #[test]
    fn an_under_delivering_trigger_keeps_its_configured_rate_apart() {
        // Configured for 20 Hz, the hub delivers 10 Hz: the measured and the
        // configured rates disagree, so a reader can give a rate verdict.
        let mut meter = RateMeter::default();
        let mut s = HubState::default();
        s.begin_session(1000, 20);
        meter.body(&s, 1_000_000_000);
        s.triggers = 10;
        let body = meter.body(&s, 2_000_000_000);
        assert_eq!(body.trigger_hz, Some(10.0));
        assert_eq!(body.trigger_hz_configured, Some(20.0));
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
