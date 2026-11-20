//! The charge a SmartRTL return home needs: the low-battery return fires
//! once the charge left falls to it, above the fixed floor when the way home
//! is long.
//!
//! The time home is the flown path back (loops pruned, as SmartRTL prunes
//! them: [`super::trail::Trail::return_len_m`]) plus the way the vehicle
//! still flies on through the countdown, at the speed SmartRTL retraces it
//! ([`v_ret_mps`]), the descent at the FC's descent speed down to
//! `LAND_ALT_LOW`, the rest at its landing speed, the countdown and a few
//! seconds' slack. The current is an average over the flight so far
//! ([`TAU_S`]); the need is that current over the time home with a margin,
//! plus the charge the vehicle must still hold at touchdown ([`keep_mah`]):
//! the FC's low level, so its own low-battery failsafe never interrupts the
//! return (a Land low action would land partway home).
//!
//! SmartRTL does not fly home at the waypoint speed. In simulation (a rig
//! survey, 2.5 Ah, four flights) it retraced the orbit path at 2.0-2.2 m/s
//! with `WP_SPD` 4, after the mission flew it at about 2.5 m/s: its points
//! are straight segments it rounds corner by corner. Counting `WP_SPD` put
//! the time home at 68 s where the return took 66 s to the descent alone,
//! and the FC reached its critical level before touchdown.

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
/// SmartRTL's retrace speed as a share of the speed the path was flown at
/// (capped by the waypoint speed): 2.0-2.2 m/s over a path flown at 2.5 m/s
/// in simulation, rounded down.
pub const SRTL_SPEED_SHARE: f32 = 0.8;
/// The slowest retrace speed counted, m/s: a vehicle that has barely moved
/// is not taken to crawl home.
const V_RET_MIN_MPS: f32 = 1.0;
/// Horizontal speeds below this are hovering, not flying the path, m/s.
const MOVING_MPS: f32 = 0.5;
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
    /// Average horizontal speed while moving, m/s ([`TAU_S`]).
    speed_mps: Option<f32>,
    speed_ms: u64,
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

    /// Fold this tick's horizontal speed in while the vehicle moves.
    pub fn sample_speed(&mut self, v_h: f32, now_ms: u64) {
        if v_h < MOVING_MPS {
            return;
        }
        self.speed_mps = Some(match self.speed_mps {
            None => v_h,
            Some(v) => {
                let dt = now_ms.saturating_sub(self.speed_ms) as f32 / 1000.0;
                v + (dt / TAU_S).min(1.0) * (v_h - v)
            }
        });
        self.speed_ms = now_ms;
    }

    /// The average horizontal speed the path was flown at, `None` before the
    /// vehicle has moved.
    pub fn flown_mps(&self) -> Option<f32> {
        self.speed_mps
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

/// mAh to hold at touchdown: the larger of the FC's low level (`low_pct`,
/// from `BATT_LOW_MAH`, else the setup wizard's floor) and its critical one
/// (`crt_mah`, `BATT_CRT_MAH`, else [`CRT_SHARE`]), of capacity `c_mah`.
/// Arriving with only the critical level left, the rig returns tripped the
/// FC's low failsafe on the way home on every simulated flight.
pub fn keep_mah(c_mah: f32, low_pct: f32, crt_mah: Option<f32>) -> f32 {
    (low_pct / 100.0 * c_mah).max(crt_mah.unwrap_or(CRT_SHARE * c_mah))
}

/// The speed SmartRTL retraces the path at, m/s: the speed the path was
/// flown at (`flown_mps`, `None` before the vehicle moved), capped by the
/// waypoint speed, times [`SRTL_SPEED_SHARE`].
pub fn v_ret_mps(wp_mps: f32, flown_mps: Option<f32>) -> f32 {
    let v = flown_mps.map_or(wp_mps, |f| f.min(wp_mps));
    (v * SRTL_SPEED_SHARE).max(V_RET_MIN_MPS)
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
        let local = self.fc.fresh_local(now_ms);
        let v_now = local.map_or(0.0, |l| l.vn.hypot(l.ve));
        self.sup.energy.sample_speed(v_now, now_ms);
        // The countdown and the way flown on through it: none once the FC
        // returns or lands, else all of it, running or not. Counting only
        // what is left of a running one made the need fall as it ran, so a
        // countdown withdrawn for a shortfall gone (`failsafes`) restarted at
        // once with the full countdown back in the need.
        let countdown_s = if self.fc_returns_or_lands() {
            0.0
        } else {
            self.settings.pilot.low_battery_countdown_s as f32
        };
        let p = &self.fc.params;
        let get = |n: &str| p.get(n).copied().filter(|v| *v > 0.0);
        let home = self.fc.home_ne();
        let back_m = local.map_or(0.0, |l| self.sup.trail.return_len_m([l.n, l.e], home));
        let wp_mps = self.default_wp_speed().unwrap_or(V_RET_DEFAULT_MPS);
        let way = Way {
            len_m: back_m + v_now * countdown_s,
            h_m: self.fc.ekf_height(now_ms).unwrap_or(0.0),
            v_ret_mps: v_ret_mps(wp_mps, self.sup.energy.flown_mps()),
            v_dn_mps: get("WP_SPD_DN")
                .or_else(|| get("WPNAV_SPEED_DN").map(|cm| cm / 100.0))
                .unwrap_or(V_DN_DEFAULT_MPS),
            h_low_m: get("LAND_ALT_LOW_M")
                .or_else(|| get("LAND_ALT_LOW").map(|cm| cm / 100.0))
                .unwrap_or(H_LOW_DEFAULT_M),
            v_land_mps: self.land_speed(),
            countdown_s,
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
        let keep = keep_mah(c, low_pct, get("BATT_CRT_MAH"));
        let need = need_mah(i, t, keep);
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

    /// The rig survey on a 2.5 Ah pack, as flown in simulation: `WP_SPD` 4,
    /// `WP_SPD_DN` 1.5, `LAND_SPD_MS` 0.5, `LAND_ALT_LOW_M` 10, `BATT_CRT_MAH`
    /// 250, 22.7 A, 19.5 m up, the orbit flown at 2.5 m/s, the pruned path
    /// back 106.7 m when the return fired at 32 %.
    fn rig(len_m: f32, v_ret_mps: f32) -> Way {
        Way {
            len_m,
            h_m: 19.5,
            v_ret_mps,
            v_dn_mps: 1.5,
            h_low_m: 10.0,
            v_land_mps: 0.5,
            countdown_s: 10.0,
        }
    }

    #[test]
    fn a_return_that_reached_critical_is_counted_short_no_more() {
        let (c, crt, i) = (2500.0, 250.0, 22.7);
        // What the return cost: 10 s of countdown and 133 m of SmartRTL at
        // 2.0 m/s took 490 mAh to the start of the descent; the descent
        // from 19.5 m at the FC's speeds takes 26.3 s at 23.3 A, 170 mAh.
        // Arriving with the critical reserve intact needs 36.4 %.
        let cost_pct = 100.0 * (490.0 + 26.33 * 23.3 / 3.6 + crt) / c;
        // The old count: the pruned path at WP_SPD, nothing flown on in the
        // countdown. 68 s and 33 %, as published at the fire: short.
        let old = return_s(&rig(106.7, 4.0));
        assert!((old - 68.0).abs() < 0.5, "{old}");
        let old_pct = 100.0 * need_mah(i, old, crt) / c;
        assert!((32.0..33.5).contains(&old_pct), "{old_pct}");
        assert!(old_pct < cost_pct, "old {old_pct} cost {cost_pct}");
        // Now: the 25 m flown on through the countdown, at SmartRTL's
        // retrace speed from the 2.5 m/s the orbit was flown at.
        let v = v_ret_mps(4.0, Some(2.5));
        assert!((v - 2.0).abs() < 1e-4, "{v}");
        let new = return_s(&rig(106.7 + 2.5 * 10.0, v));
        // 10 s of countdown and 65.8 s of SmartRTL: 75.8 s to the descent,
        // measured 76.2 s.
        assert!((new - 26.33 - SLACK_S - 75.8).abs() < 0.2, "{new}");
        let new_pct = 100.0 * need_mah(i, new, crt) / c;
        assert!(new_pct > cost_pct, "new {new_pct} cost {cost_pct}");
    }

    #[test]
    fn the_return_keeps_the_fc_low_level_at_touchdown() {
        // The 2.5 Ah rig: BATT_LOW_MAH 500 (20 %) over BATT_CRT_MAH 250.
        assert_eq!(keep_mah(2500.0, 20.0, Some(250.0)), 500.0);
        // A critical level above the low one (a misset FC) still holds.
        assert_eq!(keep_mah(2500.0, 20.0, Some(600.0)), 600.0);
        // No mAh levels: the setup wizard's 30 % floor over 15 %.
        assert_eq!(keep_mah(5000.0, 30.0, None), 1500.0);
        // That flight's fire: 22.7 A over the 107 s home now needs 55 %,
        // landing at the 20 % low level with the margin to spare.
        let pct = 100.0 * need_mah(22.7, 107.2, keep_mah(2500.0, 20.0, Some(250.0))) / 2500.0;
        assert!((54.5..55.5).contains(&pct), "{pct}");
    }

    #[test]
    fn the_floor_variant_return_is_covered_too() {
        // A floor-basis flight in simulation: published 61 s at the fire,
        // so 78 m back; the return then flew 103 m in 46.9 s (2.2 m/s).
        let old = return_s(&Way {
            h_m: 19.8,
            ..rig(78.0, 4.0)
        });
        assert!((old - 61.0).abs() < 0.5, "{old}");
        let new = return_s(&Way {
            h_m: 19.8,
            ..rig(78.0 + 25.0, v_ret_mps(4.0, Some(2.5)))
        });
        let flown_s = new - 10.0 - SLACK_S - 9.8 / 1.5 - 10.0 / 0.5;
        assert!(flown_s >= 46.9, "{flown_s}");
    }

    #[test]
    fn the_retrace_speed_is_the_flown_speed_capped_by_wp_spd() {
        // Flown faster than WP_SPD: SmartRTL flies at most WP_SPD.
        assert!((v_ret_mps(4.0, Some(7.0)) - 3.2).abs() < 1e-4);
        // Not moved yet: WP_SPD.
        assert!((v_ret_mps(5.0, None) - 4.0).abs() < 1e-4);
        // A crawl is not counted below the floor.
        assert_eq!(v_ret_mps(4.0, Some(0.6)), V_RET_MIN_MPS);
    }

    #[test]
    fn the_flown_speed_ignores_hovering() {
        let mut e = Energy::default();
        e.sample_speed(0.2, 0);
        assert_eq!(e.flown_mps(), None);
        for k in 1..=100u64 {
            e.sample_speed(2.5, k * 100);
        }
        // Hovering for a minute leaves the flown speed where it was.
        for k in 101..=700u64 {
            e.sample_speed(0.1, k * 100);
        }
        assert!((e.flown_mps().unwrap() - 2.5).abs() < 1e-4);
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
