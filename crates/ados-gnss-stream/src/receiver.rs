//! Per-receiver navigation state, fed by decoded frames.

use serde::Serialize;

use crate::nmea::{Gga, Rmc, Sentence};
use crate::ubx::NavPvt;

/// A solution older than this reads as no fix.
pub const STALE_NS: u64 = 3_000_000_000;

/// Reported fix quality, serialized as `none`, `2d`, `3d`, `dgps`,
/// `rtk_float` or `rtk_fixed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub enum Fix {
    /// No usable fix.
    #[default]
    #[serde(rename = "none")]
    None,
    /// Two-dimensional fix.
    #[serde(rename = "2d")]
    TwoD,
    /// Three-dimensional fix.
    #[serde(rename = "3d")]
    ThreeD,
    /// Differential (SBAS or DGNSS) fix.
    #[serde(rename = "dgps")]
    Dgps,
    /// RTK with float ambiguities.
    #[serde(rename = "rtk_float")]
    RtkFloat,
    /// RTK with fixed ambiguities.
    #[serde(rename = "rtk_fixed")]
    RtkFixed,
}

/// RTK state alone, serialized as `none`, `float` or `fixed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Rtk {
    /// No RTK solution.
    #[default]
    None,
    /// Float ambiguities.
    Float,
    /// Fixed ambiguities.
    Fixed,
}

impl Fix {
    /// The RTK part of this fix.
    pub fn rtk(self) -> Rtk {
        match self {
            Fix::RtkFloat => Rtk::Float,
            Fix::RtkFixed => Rtk::Fixed,
            _ => Rtk::None,
        }
    }
}

/// Map NAV-PVT to the reported fix: carrier solution first, then the
/// differential flag, then the fix type. Dead-reckoning-only and time-only
/// solutions, or `gnssFixOK` clear, are no fix.
pub fn fix_from_pvt(p: &NavPvt) -> Fix {
    if !p.gnss_fix_ok {
        return Fix::None;
    }
    let base = match p.fix_type {
        2 => Fix::TwoD,
        3 | 4 => Fix::ThreeD,
        _ => return Fix::None,
    };
    match p.carr_soln {
        2 => Fix::RtkFixed,
        1 => Fix::RtkFloat,
        _ if p.diff_soln => Fix::Dgps,
        _ => base,
    }
}

/// The solution reported for one receiver.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Solution {
    /// Fix quality.
    pub fix: Fix,
    /// Satellites used.
    pub sats: u8,
    /// Horizontal dilution of precision, when known.
    pub hdop: Option<f64>,
    /// Latitude, degrees.
    pub lat_deg: Option<f64>,
    /// Longitude, degrees.
    pub lon_deg: Option<f64>,
    /// Height above the ellipsoid, metres.
    pub h_ellipsoid_m: Option<f64>,
    /// Height above mean sea level, metres.
    pub h_msl_m: Option<f64>,
}

/// One receiver's decoded state.
#[derive(Debug, Clone, Default)]
pub struct ReceiverState {
    /// Receiver model, when known.
    pub model: Option<String>,
    pvt: Option<(u64, NavPvt)>,
    dop: Option<(u64, f64)>,
    gga: Option<(u64, Gga)>,
    rmc: Option<(u64, Rmc)>,
}

fn fresh<T>(v: &Option<(u64, T)>, now: u64) -> Option<&T> {
    v.as_ref()
        .filter(|(t, _)| now.saturating_sub(*t) <= STALE_NS)
        .map(|(_, x)| x)
}

impl ReceiverState {
    /// An empty state for a receiver of `model`.
    pub fn new(model: Option<String>) -> Self {
        Self {
            model,
            ..Self::default()
        }
    }

    /// Record a NAV-PVT received at `now` (monotonic ns).
    pub fn on_pvt(&mut self, now: u64, pvt: NavPvt) {
        self.pvt = Some((now, pvt));
    }

    /// Record a NAV-DOP horizontal DOP received at `now` (monotonic ns).
    pub fn on_dop(&mut self, now: u64, hdop: f64) {
        self.dop = Some((now, hdop));
    }

    /// Record an NMEA sentence received at `now` (monotonic ns).
    pub fn on_nmea(&mut self, now: u64, s: Sentence) {
        match s {
            Sentence::Gga(g) => self.gga = Some((now, g)),
            Sentence::Rmc(r) => self.rmc = Some((now, r)),
        }
    }

    /// The current solution: UBX when fresh, else NMEA GGA, else RMC.
    pub fn solution(&self, now: u64) -> Solution {
        if let Some(p) = fresh(&self.pvt, now) {
            let fix = fix_from_pvt(p);
            let has = fix != Fix::None;
            // NAV-PVT carries only the position DOP; hdop comes from NAV-DOP.
            let hdop = fresh(&self.dop, now).copied();
            return Solution {
                fix,
                sats: p.num_sv,
                hdop,
                lat_deg: has.then_some(p.lat_deg),
                lon_deg: has.then_some(p.lon_deg),
                h_ellipsoid_m: has.then_some(p.h_ellipsoid_m),
                h_msl_m: has.then_some(p.h_msl_m),
            };
        }
        if let Some(g) = fresh(&self.gga, now) {
            return Solution {
                fix: g.fix,
                sats: g.sats,
                hdop: g.hdop,
                lat_deg: g.lat_deg,
                lon_deg: g.lon_deg,
                h_ellipsoid_m: g.h_ellipsoid_m,
                h_msl_m: g.h_msl_m,
            };
        }
        if let Some(r) = fresh(&self.rmc, now) {
            return Solution {
                fix: r.fix,
                lat_deg: r.lat_deg,
                lon_deg: r.lon_deg,
                ..Solution::default()
            };
        }
        Solution::default()
    }
}
