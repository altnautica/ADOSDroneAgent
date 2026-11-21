//! What the pilot reports ahead of the vehicle on its state frame: the
//! charge a return home needs ([`BatteryReserve`]) and the leaves overhead
//! along a skill's way ([`OverheadAhead`]).

use serde::{Deserialize, Serialize};

/// The charge a return home needs, published every tick airborne.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BatteryReserve {
    /// The charge level the low-battery return fires at, whole %: the
    /// energy estimate of the way home, never below the fixed floor.
    pub need_pct: u32,
    /// `energy` (the estimate of the way home sets `need_pct`) or `floor`
    /// (the fixed floor does: no current measured, no capacity set, or the
    /// way home is short).
    pub basis: String,
    /// Seconds the return home takes, countdown included.
    pub return_s: u32,
}

impl BatteryReserve {
    pub fn new(need_pct: u32, basis: impl Into<String>, return_s: u32) -> Self {
        Self {
            need_pct,
            basis: basis.into(),
            return_s,
        }
    }
}

/// The leaves overhead along the way to the subject a skill's goal names
/// (`overhead_ahead` on the state frame, while a lease goal names one; the
/// plugin host passes it to the lease holder as the `flight.overhead`
/// event). A planar lidar fixed to the frame reaches a canopy's underside
/// only while the vehicle leans; the pilot keeps those returns as overhead
/// hits, not walls, for 10 s, and reports the lowest one within 1 m of the
/// way from the vehicle to the subject as the height to fly under it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OverheadAhead {
    /// The lease whose goal the way was taken from.
    pub lease_id: u64,
    /// The highest height over the ground (m, on the pilot's height: the
    /// downward rangefinder while it reads) at which the swept band (the
    /// airframe's top plus 0.3 m of clearance) stays under that return;
    /// `None` while nothing overhead lies along the way.
    pub fly_under_agl_m: Option<f32>,
    /// How far along the way it lies, horizontal m; `None` with it.
    pub ahead_m: Option<f32>,
}

impl OverheadAhead {
    pub fn new(lease_id: u64, fly_under_agl_m: Option<f32>, ahead_m: Option<f32>) -> Self {
        Self {
            lease_id,
            fly_under_agl_m,
            ahead_m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overhead_report_reads_as_written() {
        let o = OverheadAhead::new(7, Some(3.87), Some(2.5));
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["lease_id"], 7);
        assert!((v["fly_under_agl_m"].as_f64().unwrap() - 3.87).abs() < 1e-6);
        let back: OverheadAhead = serde_json::from_value(v).unwrap();
        assert_eq!(back, o);
        let clear = serde_json::to_value(OverheadAhead::new(7, None, None)).unwrap();
        assert!(clear["fly_under_agl_m"].is_null());
    }
}
