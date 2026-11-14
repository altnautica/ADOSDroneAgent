//! The flight controller's IMU feeding the lidar planar pose, from the World
//! Engine `plugin.world-engine.map_status` topic (`imu_feed`), and its
//! `nav_imu` readiness item.

use ados_protocol::pilot::{ControlMode, Readiness, Severity};
use serde::Deserialize;

use crate::sensor_spec::spec;

/// `map_status.imu_feed`: present only while the lidar pose uses the FC IMU.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ImuFeed {
    /// `ok`, `degraded` or `lost`; any word but `ok` is not ok.
    pub state: String,
    /// `attitude_stand_in`, `stalled`, `starved` or `None`.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub rate_hz: Option<f64>,
}

/// The latest `imu_feed` and when its report arrived.
#[derive(Debug, Clone, Default)]
pub struct NavImuTrack {
    last: Option<(ImuFeed, u64)>,
}

impl NavImuTrack {
    /// Fold a `map_status` report in. A null or absent `imu_feed` means the
    /// pose no longer uses the IMU: the item goes away.
    pub fn on_status(&mut self, payload: &serde_json::Value, now_ms: u64) {
        self.last = payload
            .get("imu_feed")
            .filter(|f| !f.is_null())
            .and_then(|f| ImuFeed::deserialize(f).ok())
            .map(|f| (f, now_ms));
    }

    /// The `nav_imu` item, `None` while the pose does not use the IMU.
    pub fn item(&self, mode: ControlMode, now_ms: u64) -> Option<Readiness> {
        let (feed, at) = self.last.as_ref()?;
        let s = spec();
        let t = &s.thresholds.nav_imu;
        let severity = if mode == ControlMode::Ados {
            Severity::Block
        } else {
            Severity::Warn
        };
        let stale = now_ms.saturating_sub(*at) >= t.stale_ms;
        let (reason, rate) = if stale {
            (Some("stalled"), None)
        } else {
            (feed.reason.as_deref(), feed.rate_hz)
        };
        let ok = !stale && feed.state == "ok" && rate.is_some_and(|r| r >= t.min_hz);
        let msg = if ok {
            s.readiness.nav_imu.ok.clone()
        } else {
            let rate = rate.map_or_else(|| "--".to_owned(), |r| format!("{r:.1}"));
            format!(
                "{} ({}, {rate} Hz)",
                s.readiness.nav_imu.degraded,
                reason.unwrap_or("--")
            )
        };
        Some(Readiness {
            id: "nav_imu".to_owned(),
            ok,
            severity,
            msg,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn item(t: &NavImuTrack, mode: ControlMode, now: u64) -> Option<(bool, Severity, String)> {
        t.item(mode, now).map(|r| (r.ok, r.severity, r.msg))
    }

    #[test]
    fn hidden_until_the_pose_uses_the_imu() {
        let mut t = NavImuTrack::default();
        assert_eq!(item(&t, ControlMode::Ados, 0), None);
        t.on_status(&json!({"v": 1, "imu_feed": null}), 0);
        assert_eq!(item(&t, ControlMode::Ados, 0), None);
        t.on_status(&json!({"v": 1}), 0);
        assert_eq!(item(&t, ControlMode::Ados, 0), None);
    }

    #[test]
    fn ok_needs_state_ok_and_the_minimum_rate() {
        let texts = &spec().readiness.nav_imu;
        let mut t = NavImuTrack::default();
        t.on_status(
            &json!({"imu_feed": {"state": "ok", "reason": null, "rate_hz": 50.0}}),
            1000,
        );
        assert_eq!(
            item(&t, ControlMode::Ados, 1000),
            Some((true, Severity::Block, texts.ok.clone()))
        );
        t.on_status(
            &json!({"imu_feed": {"state": "ok", "reason": null, "rate_hz": 39.94}}),
            1000,
        );
        assert_eq!(
            item(&t, ControlMode::Ados, 1000),
            Some((
                false,
                Severity::Block,
                format!("{} (--, 39.9 Hz)", texts.degraded)
            ))
        );
        t.on_status(
            &json!({"imu_feed": {"state": "degraded", "reason": "attitude_stand_in", "rate_hz": null}}),
            1000,
        );
        assert_eq!(
            item(&t, ControlMode::FcManual, 1000),
            Some((
                false,
                Severity::Warn,
                format!("{} (attitude_stand_in, -- Hz)", texts.degraded)
            ))
        );
    }

    #[test]
    fn lost_or_an_unknown_state_is_not_ok_even_at_full_rate() {
        let texts = &spec().readiness.nav_imu;
        let mut t = NavImuTrack::default();
        t.on_status(
            &json!({"imu_feed": {"state": "lost", "reason": "stalled", "rate_hz": 100.0}}),
            0,
        );
        assert_eq!(
            item(&t, ControlMode::Ados, 0),
            Some((
                false,
                Severity::Block,
                format!("{} (stalled, 100.0 Hz)", texts.degraded)
            ))
        );
        assert_eq!(
            item(&t, ControlMode::Elrs, 0).map(|i| (i.0, i.1)),
            Some((false, Severity::Warn))
        );
        t.on_status(
            &json!({"imu_feed": {"state": "resyncing", "reason": null, "rate_hz": 100.0}}),
            0,
        );
        assert_eq!(item(&t, ControlMode::Ados, 0).map(|i| i.0), Some(false));
    }

    #[test]
    fn a_silent_map_status_is_stalled() {
        let texts = &spec().readiness.nav_imu;
        let stale = spec().thresholds.nav_imu.stale_ms;
        let mut t = NavImuTrack::default();
        t.on_status(
            &json!({"imu_feed": {"state": "ok", "reason": null, "rate_hz": 100.0}}),
            1000,
        );
        assert_eq!(
            item(&t, ControlMode::Ados, 1000 + stale - 1).map(|i| i.0),
            Some(true)
        );
        assert_eq!(
            item(&t, ControlMode::Ados, 1000 + stale),
            Some((
                false,
                Severity::Block,
                format!("{} (stalled, -- Hz)", texts.degraded)
            ))
        );
    }
}
