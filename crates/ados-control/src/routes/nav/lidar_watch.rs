//! The scanning lidar as the hub sees it: when each `plugin.world-engine.scan`
//! arrived (for the measured scan rate and the last scan's age) and the latest
//! `plugin.world-engine.lidar` driver status.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::Value;

/// The window the scan rate is measured over.
pub const SCAN_RATE_WINDOW: Duration = Duration::from_secs(2);

/// A driver status older than this is no longer the driver's word (it
/// publishes at 1 Hz).
pub const STATUS_FRESH: Duration = Duration::from_secs(3);

/// The driver's `plugin.world-engine.lidar` report.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarStatus {
    pub device: Option<String>,
    pub connected: bool,
    /// `good`, `warning` or `error`; `None` before the first health read.
    pub health: Option<String>,
}

impl LidarStatus {
    /// The report from a payload; `None` for one without a boolean `connected`.
    fn from_payload(v: &Value) -> Option<Self> {
        Some(LidarStatus {
            device: v.get("device").and_then(Value::as_str).map(str::to_owned),
            connected: v.get("connected")?.as_bool()?,
            health: v
                .get("health")
                .and_then(Value::as_str)
                .filter(|h| matches!(*h, "good" | "warning" | "error"))
                .map(str::to_owned),
        })
    }
}

/// What the sensing routes read of the lidar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LidarView {
    /// Scans per second over the last [`SCAN_RATE_WINDOW`]; `None` with fewer
    /// than two scans in it.
    pub rate_hz: Option<f64>,
    /// Milliseconds since the last scan; `None` when none has arrived.
    pub age_ms: Option<u64>,
    /// The driver's report, only while fresh.
    pub status: Option<LidarStatus>,
}

/// The arrivals and the driver status.
#[derive(Debug, Default)]
pub(crate) struct LidarWatch {
    scans: VecDeque<Instant>,
    last_scan: Option<Instant>,
    status: Option<(LidarStatus, Instant)>,
}

impl LidarWatch {
    pub(crate) fn on_scan(&mut self, now: Instant) {
        self.scans.push_back(now);
        self.last_scan = Some(now);
        self.prune(now);
    }

    pub(crate) fn on_status(&mut self, payload: &Value, now: Instant) {
        if let Some(s) = LidarStatus::from_payload(payload) {
            self.status = Some((s, now));
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .scans
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > SCAN_RATE_WINDOW)
        {
            self.scans.pop_front();
        }
    }

    pub(crate) fn view(&mut self, now: Instant) -> LidarView {
        self.prune(now);
        let rate_hz = match (self.scans.front(), self.scans.back()) {
            (Some(first), Some(last)) if self.scans.len() >= 2 && last > first => {
                Some((self.scans.len() - 1) as f64 / last.duration_since(*first).as_secs_f64())
            }
            _ => None,
        };
        let ms = |t: Instant| u64::try_from(now.saturating_duration_since(t).as_millis()).ok();
        LidarView {
            rate_hz,
            age_ms: self.last_scan.and_then(ms),
            status: self
                .status
                .as_ref()
                .filter(|(_, at)| now.saturating_duration_since(*at) <= STATUS_FRESH)
                .map(|(s, _)| s.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_rate_is_measured_over_the_window_and_the_age_since_the_last_scan() {
        let t0 = Instant::now();
        let mut w = LidarWatch::default();
        assert_eq!(w.view(t0), LidarView::default());
        for i in 0..30 {
            w.on_scan(t0 + Duration::from_millis(100 * i));
        }
        let v = w.view(t0 + Duration::from_millis(2950));
        let rate = v.rate_hz.unwrap();
        assert!((rate - 10.0).abs() < 0.01, "{rate}");
        assert_eq!(v.age_ms, Some(50));
        // Silent past the window: no rate, the age keeps growing.
        let v = w.view(t0 + Duration::from_secs(10));
        assert_eq!(v.rate_hz, None);
        assert_eq!(v.age_ms, Some(7100));
    }

    #[test]
    fn the_driver_status_is_read_while_fresh() {
        let t0 = Instant::now();
        let mut w = LidarWatch::default();
        w.on_status(&json!({"v":1,"connected":"yes"}), t0);
        assert_eq!(w.view(t0).status, None);
        w.on_status(
            &json!({"v":1,"device":"/dev/ttyUSB0","connected":true,"health":"warning"}),
            t0,
        );
        let s = w.view(t0 + Duration::from_secs(1)).status.unwrap();
        assert!(s.connected);
        assert_eq!(s.health.as_deref(), Some("warning"));
        assert_eq!(s.device.as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(w.view(t0 + Duration::from_secs(4)).status, None);
    }
}
