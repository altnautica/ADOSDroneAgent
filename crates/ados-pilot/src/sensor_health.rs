//! Navigation sensor health: the flight controller's optical flow and
//! downward range, and the scanning lidar, classified by the thresholds of
//! `cockpit-spec/sensor-health.json`.

use std::collections::VecDeque;

use ados_protocol::pilot::{
    FlowHealth, FlowState, LidarDriverHealth, LidarHealth, LidarState, RangeHealth, RangeState,
    SensorHealth,
};

use crate::sensor_spec::spec;

/// Rates are counted over this window.
pub const RATE_WINDOW_MS: u64 = 2000;
/// A lidar status report older than this no longer speaks for the driver
/// (the capture service publishes it at 1 Hz).
pub const LIDAR_STATUS_STALE_MS: u64 = 3000;

/// Arrival times over the rate window.
#[derive(Debug, Clone, Default)]
pub struct Arrivals {
    at: VecDeque<u64>,
    first_ms: Option<u64>,
}

impl Arrivals {
    pub fn push(&mut self, now_ms: u64) {
        self.first_ms.get_or_insert(now_ms);
        self.at.push_back(now_ms);
        while self
            .at
            .front()
            .is_some_and(|&t| now_ms.saturating_sub(t) >= RATE_WINDOW_MS)
        {
            self.at.pop_front();
        }
    }

    /// Arrivals per second over the window. Before a full window has passed
    /// since the first arrival, the rate is the intervals seen so far; `None`
    /// until two arrivals.
    pub fn rate_hz(&self, now_ms: u64) -> Option<f64> {
        let first = self.first_ms?;
        let n = self
            .at
            .iter()
            .filter(|&&t| now_ms.saturating_sub(t) < RATE_WINDOW_MS)
            .count() as f64;
        let span = now_ms.saturating_sub(first);
        let hz = if span >= RATE_WINDOW_MS {
            n * 1000.0 / RATE_WINDOW_MS as f64
        } else if n >= 2.0 && span > 0 {
            (n - 1.0) * 1000.0 / span as f64
        } else {
            return None;
        };
        Some((hz * 100.0).round() / 100.0)
    }
}

/// OPTICAL_FLOW from the flight controller.
#[derive(Debug, Clone, Default)]
pub struct FlowTrack {
    /// The last quality and when it arrived.
    last: Option<(u8, u64)>,
    /// When the quality fell below `low_quality` and stayed there.
    low_since: Option<u64>,
    arrivals: Arrivals,
}

impl FlowTrack {
    pub fn on_msg(&mut self, quality: u8, now_ms: u64) {
        self.last = Some((quality, now_ms));
        self.arrivals.push(now_ms);
        if quality < spec().thresholds.flow.low_quality {
            self.low_since.get_or_insert(now_ms);
        } else {
            self.low_since = None;
        }
    }

    pub fn seen(&self) -> bool {
        self.last.is_some()
    }

    pub fn state(&self, now_ms: u64) -> FlowState {
        let t = &spec().thresholds.flow;
        match self.last {
            None => FlowState::Absent,
            Some((_, at)) if now_ms.saturating_sub(at) >= t.lost_ms => FlowState::Lost,
            _ if self
                .low_since
                .is_some_and(|s| now_ms.saturating_sub(s) >= t.low_hold_ms) =>
            {
                FlowState::Low
            }
            _ => FlowState::Ok,
        }
    }

    /// When the current `low` or `lost` began (a low quality running into a
    /// loss counts from the low), `None` while ok or absent.
    pub fn bad_since(&self, now_ms: u64) -> Option<u64> {
        let t = &spec().thresholds.flow;
        let low_at = self.low_since.map(|s| s + t.low_hold_ms);
        match self.state(now_ms) {
            FlowState::Low => low_at,
            FlowState::Lost => {
                let lost_at = self.last.map(|(_, at)| at + t.lost_ms)?;
                Some(low_at.filter(|&l| l <= lost_at).unwrap_or(lost_at))
            }
            FlowState::Ok | FlowState::Absent => None,
        }
    }

    pub fn health(&self, now_ms: u64) -> FlowHealth {
        let mut h = FlowHealth::default();
        h.state = self.state(now_ms);
        h.quality = self.last.map(|(q, _)| q);
        h.rate_hz = self.arrivals.rate_hz(now_ms);
        h.age_ms = self.last.map(|(_, at)| now_ms.saturating_sub(at));
        h
    }
}

/// The downward DISTANCE_SENSOR from the flight controller: current, min and
/// max in cm, and when it arrived.
#[derive(Debug, Clone, Default)]
pub struct RangeTrack {
    last: Option<([u16; 3], u64)>,
}

impl RangeTrack {
    pub fn on_msg(&mut self, current_cm: u16, min_cm: u16, max_cm: u16, now_ms: u64) {
        self.last = Some(([current_cm, min_cm, max_cm], now_ms));
    }

    pub fn seen(&self) -> bool {
        self.last.is_some()
    }

    pub fn state(&self, now_ms: u64) -> RangeState {
        match self.last {
            None => RangeState::Absent,
            Some((_, at)) if now_ms.saturating_sub(at) >= spec().thresholds.range.lost_ms => {
                RangeState::Lost
            }
            Some(([cur, min, max], _)) if cur < min || cur > max => RangeState::OutOfRange,
            Some(_) => RangeState::Ok,
        }
    }

    pub fn health(&self, now_ms: u64) -> RangeHealth {
        let mut h = RangeHealth::default();
        h.state = self.state(now_ms);
        if h.state == RangeState::Ok {
            h.height_m = self.last.map(|([cur, _, _], _)| f64::from(cur) / 100.0);
        }
        h.age_ms = self.last.map(|(_, at)| now_ms.saturating_sub(at));
        h
    }
}

/// The capture service's `plugin.world-engine.lidar` report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LidarStatus {
    pub connected: bool,
    pub health: Option<LidarDriverHealth>,
    pub at_ms: u64,
}

/// The scanning lidar: scan arrivals and the driver's status report.
#[derive(Debug, Clone, Default)]
pub struct LidarTrack {
    arrivals: Arrivals,
    last_scan_ms: Option<u64>,
    status: Option<LidarStatus>,
}

impl LidarTrack {
    pub fn on_scan(&mut self, now_ms: u64) {
        self.last_scan_ms = Some(now_ms);
        self.arrivals.push(now_ms);
    }

    pub fn on_status(&mut self, payload: &serde_json::Value, now_ms: u64) {
        let Some(connected) = payload
            .get("connected")
            .and_then(serde_json::Value::as_bool)
        else {
            return;
        };
        let health = payload
            .get("health")
            .and_then(|h| serde_json::from_value(h.clone()).ok());
        self.status = Some(LidarStatus {
            connected,
            health,
            at_ms: now_ms,
        });
    }

    /// A lidar has scanned, or the capture service reports one.
    pub fn expected(&self) -> bool {
        self.last_scan_ms.is_some() || self.status.is_some()
    }

    fn fresh_status(&self, now_ms: u64) -> Option<LidarStatus> {
        self.status
            .filter(|s| now_ms.saturating_sub(s.at_ms) < LIDAR_STATUS_STALE_MS)
    }

    pub fn state(&self, now_ms: u64) -> LidarState {
        let t = &spec().thresholds.lidar;
        let status = self.fresh_status(now_ms);
        if status.is_some_and(|s| !s.connected) {
            return LidarState::Lost;
        }
        let Some(last) = self.last_scan_ms else {
            return LidarState::Absent;
        };
        if now_ms.saturating_sub(last) >= t.lost_ms {
            return LidarState::Lost;
        }
        let off_rate = self
            .arrivals
            .rate_hz(now_ms)
            .is_some_and(|r| r < t.min_hz || r > t.max_hz);
        let unhealthy = status
            .and_then(|s| s.health)
            .is_some_and(|h| matches!(h, LidarDriverHealth::Warning | LidarDriverHealth::Error));
        if off_rate || unhealthy {
            LidarState::Degraded
        } else {
            LidarState::Ok
        }
    }

    pub fn health(&self, now_ms: u64) -> LidarHealth {
        let mut h = LidarHealth::default();
        h.state = self.state(now_ms);
        h.rate_hz = self.arrivals.rate_hz(now_ms);
        h.health = self.fresh_status(now_ms).and_then(|s| s.health);
        h.age_ms = self.last_scan_ms.map(|at| now_ms.saturating_sub(at));
        h
    }
}

/// The `pilot` kind's `sensors`.
pub fn snapshot(
    flow: &FlowTrack,
    range: &RangeTrack,
    lidar: &LidarTrack,
    now_ms: u64,
) -> SensorHealth {
    let mut s = SensorHealth::default();
    s.flow = flow.health(now_ms);
    s.range = range.health(now_ms);
    s.lidar = lidar.health(now_ms);
    s
}

#[cfg(test)]
mod tests;
