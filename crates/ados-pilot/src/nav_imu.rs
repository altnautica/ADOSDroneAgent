//! The flight controller's IMU feeding the lidar planar pose, from the World
//! Engine `plugin.navigation.map_status` topic (`imu_feed`), and its two
//! readiness items: `nav_imu`, the FC IMU feed itself, and `nav_heading`, the
//! lidar map's heading state the same field carries while the feed measures.
//! Both hold a pilot-flown launch only while Navigation flies external
//! navigation from that pose (its `plugin.navigation.state` mode).

use ados_protocol::pilot::{ControlMode, Readiness, Severity};
use serde::Deserialize;

use crate::sensor_spec::spec;

/// Navigation's `plugin.navigation.state` mode while the flight controller fuses the lidar
/// pose as external navigation.
const EXTERNAL_NAV: &str = "external_nav";

/// `imu_feed.reason` words about the lidar map's heading, not the feed: the
/// map reports them only while the FC IMU feed itself measures (a feed fault
/// takes precedence), so the feed is healthy under them.
const HEADING_REASONS: [&str; 2] = ["heading_unconfirmed", "north_unknown"];

/// `map_status.imu_feed`: present only while the lidar pose uses the FC IMU.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ImuFeed {
    /// `ok`, `degraded` or `lost`; any word but `ok` is not ok.
    pub state: String,
    /// `attitude_stand_in`, `stalled`, `starved`, a heading reason
    /// ([`HEADING_REASONS`]) or `None`.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub rate_hz: Option<f64>,
}

/// The latest `imu_feed` and when its report arrived, and whether the lidar
/// pose is flown on.
#[derive(Debug, Clone, Default)]
pub struct NavImuTrack {
    last: Option<(ImuFeed, u64)>,
    /// Whether Navigation's last `plugin.navigation.state` named [`EXTERNAL_NAV`]; false
    /// before any (nothing fuses the lidar pose without Navigation).
    external_nav: bool,
    /// Whether any Navigation report has named the FC's active EKF source
    /// set (`active_set`); until one does, the pilot's set is only assumed.
    set_reported: bool,
}

impl NavImuTrack {
    /// Fold a `map_status` report in. A null or absent `imu_feed` means the
    /// pose no longer uses the IMU: the items go away.
    pub fn on_status(&mut self, payload: &serde_json::Value, now_ms: u64) {
        self.last = payload
            .get("imu_feed")
            .filter(|f| !f.is_null())
            .and_then(|f| ImuFeed::deserialize(f).ok())
            .map(|f| (f, now_ms));
    }

    /// Fold a Navigation `plugin.navigation.state` report in. A report naming no mode
    /// keeps the last one.
    pub fn on_nav_state(&mut self, payload: &serde_json::Value) {
        if let Some(mode) = payload.get("mode").and_then(serde_json::Value::as_str) {
            self.external_nav = mode == EXTERNAL_NAV;
        }
        if payload
            .get("active_set")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        {
            self.set_reported = true;
        }
    }

    /// Whether Navigation has named the FC's active EKF source set.
    pub fn set_reported(&self) -> bool {
        self.set_reported
    }

    /// The `nav_imu` and `nav_heading` items: neither while the pose does not
    /// use the IMU, and `nav_heading` only while a fresh report carries a
    /// heading reason. They block in ADOS control mode while the lidar pose
    /// is flown on, and warn otherwise.
    pub fn items(&self, mode: ControlMode, now_ms: u64) -> [Option<Readiness>; 2] {
        let Some((feed, at)) = self.last.as_ref() else {
            return [None, None];
        };
        let s = spec();
        let t = &s.thresholds.nav_imu;
        let severity = if mode == ControlMode::Ados && self.external_nav {
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
        let heading = reason.filter(|r| HEADING_REASONS.contains(r));
        let feed_ok = feed.state == "ok" || heading.is_some();
        let ok = !stale && feed_ok && rate.is_some_and(|r| r >= t.min_hz);
        let msg = if ok {
            s.readiness.nav_imu.ok.clone()
        } else {
            let rate = rate.map_or_else(|| "--".to_owned(), |r| format!("{r:.1}"));
            let why = if heading.is_some() { None } else { reason };
            format!(
                "{} ({}, {rate} Hz)",
                s.readiness.nav_imu.degraded,
                why.unwrap_or("--")
            )
        };
        let imu = Readiness {
            id: "nav_imu".to_owned(),
            ok,
            severity,
            msg,
        };
        let heading = heading.map(|r| Readiness {
            id: "nav_heading".to_owned(),
            ok: false,
            severity,
            msg: format!("{} ({r})", s.readiness.nav_heading.unconfirmed),
        });
        [Some(imu), heading]
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    type Item = (bool, Severity, String);

    fn items(t: &NavImuTrack, mode: ControlMode, now: u64) -> [Option<Item>; 2] {
        t.items(mode, now)
            .map(|r| r.map(|r| (r.ok, r.severity, r.msg)))
    }

    fn item(t: &NavImuTrack, mode: ControlMode, now: u64) -> Option<Item> {
        let [imu, _] = items(t, mode, now);
        imu
    }

    /// A track whose pose Navigation flies on.
    fn flown() -> NavImuTrack {
        let mut t = NavImuTrack::default();
        t.on_nav_state(&json!({"mode": "external_nav", "active_set": 1}));
        t
    }

    #[test]
    fn hidden_until_the_pose_uses_the_imu() {
        let mut t = NavImuTrack::default();
        assert_eq!(items(&t, ControlMode::Ados, 0), [None, None]);
        t.on_status(&json!({"v": 1, "imu_feed": null}), 0);
        assert_eq!(items(&t, ControlMode::Ados, 0), [None, None]);
        t.on_status(&json!({"v": 1}), 0);
        assert_eq!(items(&t, ControlMode::Ados, 0), [None, None]);
    }

    #[test]
    fn ok_needs_state_ok_and_the_minimum_rate() {
        let texts = &spec().readiness.nav_imu;
        let mut t = flown();
        t.on_status(
            &json!({"imu_feed": {"state": "ok", "reason": null, "rate_hz": 50.0}}),
            1000,
        );
        assert_eq!(
            items(&t, ControlMode::Ados, 1000),
            [Some((true, Severity::Block, texts.ok.clone())), None]
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
        let mut t = flown();
        t.on_status(
            &json!({"imu_feed": {"state": "lost", "reason": "stalled", "rate_hz": 100.0}}),
            0,
        );
        assert_eq!(
            items(&t, ControlMode::Ados, 0),
            [
                Some((
                    false,
                    Severity::Block,
                    format!("{} (stalled, 100.0 Hz)", texts.degraded)
                )),
                None
            ]
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

    /// The map's heading words say the feed measures: `nav_imu` reads the
    /// feed's rate alone, and `nav_heading` carries the word.
    #[test]
    fn a_heading_reason_is_the_map_s_not_the_imu_s() {
        let s = spec();
        let mut t = flown();
        for reason in ["heading_unconfirmed", "north_unknown"] {
            t.on_status(
                &json!({"imu_feed": {"state": "lost", "reason": reason, "rate_hz": 100.0}}),
                0,
            );
            assert_eq!(
                items(&t, ControlMode::Ados, 0),
                [
                    Some((true, Severity::Block, s.readiness.nav_imu.ok.clone())),
                    Some((
                        false,
                        Severity::Block,
                        format!("{} ({reason})", s.readiness.nav_heading.unconfirmed)
                    )),
                ]
            );
        }
        // A starved rate under a heading word is still the feed's fault, and
        // the word stays off the IMU's message.
        t.on_status(
            &json!({"imu_feed": {"state": "lost", "reason": "north_unknown", "rate_hz": 12.0}}),
            0,
        );
        let [imu, heading] = items(&t, ControlMode::Ados, 0);
        assert_eq!(
            imu,
            Some((
                false,
                Severity::Block,
                format!("{} (--, 12.0 Hz)", s.readiness.nav_imu.degraded)
            ))
        );
        assert!(heading.is_some());
    }

    /// The lidar pose holds a launch only while Navigation flies external
    /// navigation from it; a report without a mode keeps the last one.
    #[test]
    fn blocks_only_while_the_pose_is_flown_on() {
        let mut t = NavImuTrack::default();
        t.on_status(
            &json!({"imu_feed": {"state": "lost", "reason": "north_unknown", "rate_hz": 100.0}}),
            0,
        );
        let severities = |t: &NavImuTrack| items(t, ControlMode::Ados, 0).map(|i| i.map(|i| i.1));
        let warn = [Some(Severity::Warn), Some(Severity::Warn)];
        let block = [Some(Severity::Block), Some(Severity::Block)];
        assert_eq!(severities(&t), warn, "before any plugin.navigation.state");
        t.on_nav_state(&json!({"mode": "off", "active_set": null}));
        assert_eq!(severities(&t), warn);
        t.on_nav_state(&json!({"mode": "external_nav", "active_set": 1}));
        assert_eq!(severities(&t), block);
        t.on_nav_state(&json!({"active_set": 2}));
        assert_eq!(severities(&t), block, "kept without a mode");
        t.on_nav_state(&json!({"mode": "optical_flow"}));
        assert_eq!(severities(&t), warn);
    }

    #[test]
    fn a_silent_map_status_is_stalled() {
        let texts = &spec().readiness.nav_imu;
        let stale = spec().thresholds.nav_imu.stale_ms;
        let mut t = flown();
        t.on_status(
            &json!({"imu_feed": {"state": "lost", "reason": "north_unknown", "rate_hz": 100.0}}),
            1000,
        );
        assert_eq!(
            item(&t, ControlMode::Ados, 1000 + stale - 1).map(|i| i.0),
            Some(true)
        );
        assert_eq!(
            items(&t, ControlMode::Ados, 1000 + stale),
            [
                Some((
                    false,
                    Severity::Block,
                    format!("{} (stalled, -- Hz)", texts.degraded)
                )),
                None
            ]
        );
    }

    #[test]
    fn the_source_set_counts_as_reported_once_a_report_names_it() {
        let mut t = NavImuTrack::default();
        assert!(!t.set_reported());
        t.on_nav_state(&json!({"mode": "gps"}));
        assert!(!t.set_reported());
        t.on_nav_state(&json!({"active_set": 2}));
        assert!(t.set_reported());
        // A later report naming none keeps the last set, as the pilot does.
        t.on_nav_state(&json!({"mode": "gps"}));
        assert!(t.set_reported());
    }
}
