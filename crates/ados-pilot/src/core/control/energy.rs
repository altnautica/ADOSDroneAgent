//! The charge a SmartRTL return home needs: the low-battery return fires
//! once the charge left falls to it, above the fixed floor when the way home
//! is long.
//!
//! The time home is the flown path back (loops pruned, as SmartRTL prunes
//! them: [`super::trail::Trail::return_len_m`]) at the waypoint speed, the
//! descent at the FC's descent speed down to `LAND_ALT_LOW`, the rest at its
//! landing speed, the countdown and a few seconds' slack. The current is an
//! average over the flight so far ([`TAU_S`]); the need is that current over
//! the time home with a margin, plus the FC's critical reserve.

use ados_protocol::pilot::BatteryReserve;

use super::super::Pilot;
use crate::fc::Stamped;

/// The current average's time constant, seconds.
pub const TAU_S: f32 = 30.0;
/// Airborne this long before the current average counts, ms.
pub const VALID_AFTER_MS: u64 = 10_000;
/// A current sample older than this leaves the estimate unused, ms.
const CURRENT_FRESH_MS: u64 = 5_000;
/// The margin on the charge the time home takes.
pub const MARGIN: f32 = 1.3;
/// The critical reserve without `BATT_CRT_MAH`: this share of the capacity.
pub const CRT_SHARE: f32 = 0.15;
/// Seconds of slack on the time home (the mode change, the turn home).
pub const SLACK_S: f32 = 5.0;
/// ArduCopter's default waypoint speed, m/s.
const V_RET_DEFAULT_MPS: f32 = 5.0;
/// ArduCopter's default descent speed, m/s.
const V_DN_DEFAULT_MPS: f32 = 1.5;
/// ArduCopter's default `LAND_ALT_LOW`, m.
const H_LOW_DEFAULT_M: f32 = 10.0;

/// The current average while airborne.
#[derive(Debug, Default, Clone)]
pub struct Energy {
    current_a: Option<f32>,
    sample_ms: u64,
    since_ms: Option<u64>,
    /// This tick's reserve, airborne.
    pub reserve: Option<Reserve>,
    /// The need the low-battery countdown started at: it re-arms 5 % above.
    pub latch_pct: Option<f32>,
}

impl Energy {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Fold the FC's newest current sample in, once.
    pub fn sample(&mut self, s: Stamped<f32>, now_ms: u64) {
        self.since_ms.get_or_insert(now_ms);
        if s.at_ms == self.sample_ms && self.current_a.is_some() {
            return;
        }
        self.current_a = Some(match self.current_a {
            None => s.v,
            Some(i) => {
                let dt = s.at_ms.saturating_sub(self.sample_ms) as f32 / 1000.0;
                i + (dt / TAU_S).min(1.0) * (s.v - i)
            }
        });
        self.sample_ms = s.at_ms;
    }

    /// The average current, once airborne [`VALID_AFTER_MS`] with a fresh
    /// sample.
    pub fn current_a(&self, now_ms: u64) -> Option<f32> {
        let since = self.since_ms?;
        (now_ms >= since + VALID_AFTER_MS
            && now_ms.saturating_sub(self.sample_ms) <= CURRENT_FRESH_MS)
            .then_some(self.current_a?)
    }
}

/// How the way home is flown.
#[derive(Debug, Clone, Copy)]
pub struct Way {
    pub len_m: f32,
    pub h_m: f32,
    pub v_ret_mps: f32,
    pub v_dn_mps: f32,
    pub h_low_m: f32,
    pub v_land_mps: f32,
    pub countdown_s: f32,
}

/// Seconds a SmartRTL return home takes, the countdown and slack included.
pub fn return_s(w: &Way) -> f32 {
    let h = w.h_m.max(0.0);
    w.len_m / w.v_ret_mps
        + (h - w.h_low_m).max(0.0) / w.v_dn_mps
        + h.min(w.h_low_m) / w.v_land_mps
        + w.countdown_s
        + SLACK_S
}

/// mAh the return needs: `current_a` over `t_s` with the margin, plus the
/// critical reserve.
pub fn need_mah(current_a: f32, t_s: f32, crt_mah: f32) -> f32 {
    MARGIN * current_a * t_s / 3.6 + crt_mah
}

/// This tick's reserve: the need and whether it fires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reserve {
    /// The charge level the return needs, %, never below the floor.
    pub need_pct: f32,
    /// The need came from the energy estimate (else the floor).
    pub energy: bool,
    /// The charge left is at or below the estimate's need.
    pub short: bool,
    pub return_s: f32,
}

impl Reserve {
    pub fn basis(&self) -> &'static str {
        if self.energy {
            "energy"
        } else {
            "floor"
        }
    }

    pub fn wire(&self) -> BatteryReserve {
        BatteryReserve::new(
            self.need_pct.round().max(0.0) as u32,
            self.basis(),
            self.return_s.round().max(0.0) as u32,
        )
    }
}

impl Pilot {
    /// The reserve for a return from here, airborne, at charge `pct` over a
    /// floor of `low_pct`.
    pub(in crate::core) fn battery_reserve(
        &mut self,
        pct: f32,
        low_pct: f32,
        now_ms: u64,
    ) -> Reserve {
        if let Some(s) = self.fc.battery_current_a {
            self.sup.energy.sample(s, now_ms);
        }
        let p = &self.fc.params;
        let get = |n: &str| p.get(n).copied().filter(|v| *v > 0.0);
        let pos = self.fc.fresh_local(now_ms).map(|l| [l.n, l.e]);
        let home = self.fc.home_ne();
        let way = Way {
            len_m: pos.map_or(0.0, |pos| self.sup.trail.return_len_m(pos, home)),
            h_m: self.fc.ekf_height(now_ms).unwrap_or(0.0),
            v_ret_mps: self.default_wp_speed().unwrap_or(V_RET_DEFAULT_MPS),
            v_dn_mps: get("WP_SPD_DN")
                .or_else(|| get("WPNAV_SPEED_DN").map(|cm| cm / 100.0))
                .unwrap_or(V_DN_DEFAULT_MPS),
            h_low_m: get("LAND_ALT_LOW").map_or(H_LOW_DEFAULT_M, |cm| cm / 100.0),
            v_land_mps: self.land_speed(),
            countdown_s: self.settings.pilot.low_battery_countdown_s as f32,
        };
        let t = return_s(&way);
        let floor = Reserve {
            need_pct: low_pct,
            energy: false,
            short: false,
            return_s: t,
        };
        let (Some(c), Some(i)) = (get("BATT_CAPACITY"), self.sup.energy.current_a(now_ms)) else {
            return floor;
        };
        let crt = get("BATT_CRT_MAH").unwrap_or(CRT_SHARE * c);
        let need = need_mah(i, t, crt);
        let left = self
            .fc
            .battery_consumed_mah
            .map_or(c * pct / 100.0, |used| c - used);
        let need_pct = (100.0 * need / c).ceil();
        Reserve {
            need_pct: need_pct.max(low_pct),
            energy: need_pct > low_pct,
            short: left <= need,
            return_s: t,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn way(len_m: f32, h_m: f32) -> Way {
        Way {
            len_m,
            h_m,
            v_ret_mps: 5.0,
            v_dn_mps: 2.0,
            h_low_m: 10.0,
            v_land_mps: 0.5,
            countdown_s: 10.0,
        }
    }

    #[test]
    fn the_time_home_counts_every_leg() {
        // 500 m at 5, 20 m down at 2 to 10 m, 10 m at 0.5, 10 s, 5 s.
        let t = return_s(&way(500.0, 30.0));
        assert!((t - (100.0 + 10.0 + 20.0 + 10.0 + 5.0)).abs() < 1e-3, "{t}");
        // Below LAND_ALT_LOW: only the landing speed.
        let t = return_s(&way(0.0, 4.0));
        assert!((t - (8.0 + 15.0)).abs() < 1e-3, "{t}");
    }

    #[test]
    fn the_need_is_the_charge_home_with_a_margin_and_the_reserve() {
        // 20 A for 180 s: 1000 mAh, 1300 with the margin, plus 500.
        assert!((need_mah(20.0, 180.0, 500.0) - 1800.0).abs() < 1e-2);
    }

    #[test]
    fn the_average_counts_after_ten_seconds_airborne() {
        let mut e = Energy::default();
        for k in 0..=100u64 {
            let at = k * 100;
            e.sample(Stamped { v: 20.0, at_ms: at }, at);
        }
        assert_eq!(e.current_a(9_999), None);
        assert_eq!(e.current_a(10_000), Some(20.0));
        // A step to 50 A moves the average by dt/τ a sample.
        e.sample(
            Stamped {
                v: 50.0,
                at_ms: 13_000,
            },
            13_000,
        );
        let i = e.current_a(13_000).unwrap();
        assert!((i - 23.0).abs() < 1e-3, "{i}");
        // Stale current: unused.
        assert_eq!(e.current_a(18_001), None);
    }
}
