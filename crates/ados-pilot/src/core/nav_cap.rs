//! The horizontal speed and acceleration cap while the EKF's horizontal
//! position does not come from GNSS. ArduPilot does not clamp guided velocity
//! targets, so the pilot enforces it on every setpoint it flies.
//!
//! The pilot knows the active EKF source set only from vision-nav's
//! `nav.state`: it keeps the last set named there (the FC keeps a set until
//! told otherwise), and the boot set 1 before any. A source-set switch made
//! by an RC switch or a ground station does not reach the pilot, which keeps
//! flying under the last set it heard of.

use ados_protocol::pilot::SpeedCap;

use super::{Pilot, TICK_MS};
use crate::geo::{self, V2};
use crate::model::{limits, Limits};

/// The EKF source set ArduPilot boots with.
pub const BOOT_SOURCE_SET: u8 = 1;
/// The parameter naming each source set's horizontal position source.
const POSXY: [&str; 3] = ["EK3_SRC1_POSXY", "EK3_SRC2_POSXY", "EK3_SRC3_POSXY"];
/// `EK3_SRCn_POSXY` value for GPS.
const POSXY_GPS: f32 = 3.0;
/// A GPS_RAW_INT older than this is ignored (the FC streams it).
pub const GPS_FIX_STALE_MS: u64 = 3_000;
/// GPS_RAW_INT `fix_type` of a 3-D fix.
const FIX_3D: u8 = 3;
/// The `speed_cap.reason` while the position source is not GNSS.
pub const REASON_GNSS_DENIED: &str = "gnss-denied";

impl Pilot {
    /// The cap in force, `None` while the EKF's horizontal position comes
    /// from GNSS ([`Pilot::nav_capped`]), for the published state.
    pub fn nav_cap(&self, now_ms: u64) -> Option<SpeedCap> {
        self.nav_capped(now_ms).then(|| {
            SpeedCap::new(
                REASON_GNSS_DENIED,
                self.settings.pilot.gnss_denied_max_speed_mps,
            )
        })
    }

    /// Whether the cap is in force, without allocating (the control loop
    /// asks several times a tick). The active set's `EK3_SRCn_POSXY` decides;
    /// a set outside 1–3 is capped. Only while that parameter is unknown (not
    /// read yet, or a firmware without it) does a fresh 3-D GPS fix stand in
    /// for it.
    pub fn nav_capped(&self, now_ms: u64) -> bool {
        let gnss = match POSXY.get(usize::from(self.nav_set).wrapping_sub(1)) {
            None => false,
            Some(name) => match self.fc.params.get(*name) {
                Some(v) => (*v - POSXY_GPS).abs() < 0.5,
                None => self.fc.gps_fix.is_some_and(|f| {
                    f.v >= FIX_3D && now_ms.saturating_sub(f.at_ms) <= GPS_FIX_STALE_MS
                }),
            },
        };
        !gnss
    }

    /// The profile's limits, with the cap applied to the horizontal speed
    /// and acceleration while one is in force.
    pub fn limits_now(&self, now_ms: u64) -> Limits {
        let mut l = limits(self.settings.pilot.profile);
        if self.nav_capped(now_ms) {
            l.v_h = l.v_h.min(self.settings.pilot.gnss_denied_max_speed_mps);
            l.a_h = l.a_h.min(self.settings.pilot.gnss_denied_max_accel_mps2);
        }
        l
    }

    /// The horizontal velocity to ask for under the cap: `v` held to the
    /// capped speed, reached at no more than the capped acceleration from the
    /// velocity flown last tick (after a gap, the measured velocity), itself
    /// held to the capped speed so a cap switching on mid-flight never asks
    /// for more. Uncapped, `v` passes through.
    ///
    /// The ramp runs before the brake and the geofence, so their cut flies at
    /// once; the caller records what it flies with [`Pilot::cap_flown`].
    pub(super) fn cap_ramp(&mut self, v: V2, now_ms: u64) -> V2 {
        if !self.nav_capped(now_ms) {
            self.cap_vel = None;
            return v;
        }
        let lim = self.limits_now(now_ms);
        let target = geo::clamp_len(v, lim.v_h);
        let from = match self.cap_vel {
            Some((prev, at)) if now_ms.saturating_sub(at) <= 2 * TICK_MS => prev,
            _ => self
                .fc
                .fresh_local(now_ms)
                .map_or([0.0; 2], |l| [l.vn, l.ve]),
        };
        let from = geo::clamp_len(from, lim.v_h);
        let step = lim.a_h * TICK_MS as f32 / 1000.0;
        geo::add(from, geo::clamp_len(geo::sub(target, from), step))
    }

    /// Record the horizontal velocity flown this tick, braked and fenced:
    /// the next tick's ramp starts from it.
    pub(super) fn cap_flown(&mut self, v: V2, now_ms: u64) {
        self.cap_vel = self.nav_capped(now_ms).then_some((v, now_ms));
    }
}
