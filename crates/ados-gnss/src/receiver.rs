//! Per-receiver navigation state, fed by decoded frames.

use serde::Serialize;

use crate::nmea::{Gga, Rmc, Sentence};
use crate::ubx::NavPvt;

/// A solution older than this reads as no fix.
pub const STALE_NS: u64 = 3_000_000_000;

/// The `fix` field of `/api/v1/gnss`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub enum Fix {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "2d")]
    TwoD,
    #[serde(rename = "3d")]
    ThreeD,
    #[serde(rename = "dgps")]
    Dgps,
    #[serde(rename = "rtk_float")]
    RtkFloat,
    #[serde(rename = "rtk_fixed")]
    RtkFixed,
}

/// The `rtk` field of `/api/v1/gnss`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Rtk {
    #[default]
    None,
    Float,
    Fixed,
}

impl Fix {
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

/// What the status surfaces report for one receiver.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Solution {
    pub fix: Fix,
    pub sats: u8,
    pub hdop: Option<f64>,
    pub lat_deg: Option<f64>,
    pub lon_deg: Option<f64>,
    pub h_ellipsoid_m: Option<f64>,
    pub h_msl_m: Option<f64>,
}

/// One receiver's decoded state.
#[derive(Debug, Clone, Default)]
pub struct ReceiverState {
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
    pub fn new(model: Option<String>) -> Self {
        Self {
            model,
            ..Self::default()
        }
    }

    pub fn on_pvt(&mut self, now: u64, pvt: NavPvt) {
        self.pvt = Some((now, pvt));
    }

    pub fn on_dop(&mut self, now: u64, hdop: f64) {
        self.dop = Some((now, hdop));
    }

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
