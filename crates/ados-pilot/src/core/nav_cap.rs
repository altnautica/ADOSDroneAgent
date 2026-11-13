//! The horizontal speed and acceleration cap while the EKF's horizontal
//! position does not come from GNSS. ArduPilot does not clamp guided velocity
//! targets, so the pilot enforces it on every setpoint it flies.

use ados_protocol::pilot::SpeedCap;

use super::{Pilot, TICK_MS};
use crate::geo::{self, V2};
use crate::model::{limits, Limits};

/// A `nav.state` or GPS_RAW_INT older than this is ignored (vision-nav
/// publishes the first at 1 Hz or faster, the FC streams the second).
pub const NAV_STATE_STALE_MS: u64 = 3_000;
/// `EK3_SRCn_POSXY` value for GPS.
const POSXY_GPS: f32 = 3.0;
/// GPS_RAW_INT `fix_type` of a 3-D fix.
const FIX_3D: u8 = 3;
/// The `speed_cap.reason` while the position source is not GNSS.
pub const REASON_GNSS_DENIED: &str = "gnss-denied";

impl Pilot {
    /// The cap in force, `None` while the EKF's horizontal position comes
    /// from GNSS. Without a fresh `nav.state` naming the active source set,
    /// only a 3-D GPS fix counts as GNSS: anything else fails closed.
    pub fn nav_cap(&self, now_ms: u64) -> Option<SpeedCap> {
        let active_set = self
            .nav_state
            .filter(|(_, at)| now_ms.saturating_sub(*at) <= NAV_STATE_STALE_MS)
            .and_then(|(set, _)| set);
        let gnss = match active_set {
            Some(set) => self
                .fc
                .params
                .get(&format!("EK3_SRC{set}_POSXY"))
                .is_some_and(|v| (*v - POSXY_GPS).abs() < 0.5),
            None => self.fc.gps_fix.is_some_and(|f| {
                f.v >= FIX_3D && now_ms.saturating_sub(f.at_ms) <= NAV_STATE_STALE_MS
            }),
        };
        (!gnss).then(|| {
            SpeedCap::new(
                REASON_GNSS_DENIED,
                self.settings.pilot.gnss_denied_max_speed_mps,
            )
        })
    }

    /// The profile's limits, with the cap applied to the horizontal speed
    /// and acceleration while one is in force.
    pub fn limits_now(&self, now_ms: u64) -> Limits {
        let mut l = limits(self.settings.pilot.profile);
        if let Some(cap) = self.nav_cap(now_ms) {
            l.v_h = l.v_h.min(cap.v_h_mps);
            l.a_h = l.a_h.min(self.settings.pilot.gnss_denied_max_accel_mps2);
        }
        l
    }

    /// A horizontal velocity setpoint under the cap: no faster than the
    /// capped speed, and changed from the last one by no more than the capped
    /// acceleration over a tick. Uncapped, `v` passes through.
    pub(super) fn cap_velocity(&mut self, v: V2, now_ms: u64) -> V2 {
        if self.nav_cap(now_ms).is_none() {
            self.cap_vel = None;
            return v;
        }
        let lim = self.limits_now(now_ms);
        let target = geo::clamp_len(v, lim.v_h);
        // A setpoint from more than two ticks ago no longer describes what
        // the FC flies: start from its measured velocity.
        let from = match self.cap_vel {
            Some((prev, at)) if now_ms.saturating_sub(at) <= 2 * TICK_MS => prev,
            _ => self
                .fc
                .fresh_local(now_ms)
                .map_or([0.0; 2], |l| [l.vn, l.ve]),
        };
        let step = lim.a_h * TICK_MS as f32 / 1000.0;
        let out = geo::add(from, geo::clamp_len(geo::sub(target, from), step));
        self.cap_vel = Some((out, now_ms));
        out
    }
}
