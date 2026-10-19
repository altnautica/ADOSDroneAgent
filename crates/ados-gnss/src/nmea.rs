//! NMEA GGA / RMC decode, the fallback when a receiver sends no UBX.

use crate::receiver::Fix;

/// A decoded GGA sentence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gga {
    pub fix: Fix,
    pub sats: u8,
    pub hdop: Option<f64>,
    pub lat_deg: Option<f64>,
    pub lon_deg: Option<f64>,
    pub h_msl_m: Option<f64>,
    pub h_ellipsoid_m: Option<f64>,
}

/// A decoded RMC sentence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rmc {
    pub fix: Fix,
    pub lat_deg: Option<f64>,
    pub lon_deg: Option<f64>,
}

/// A decoded sentence this service uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sentence {
    Gga(Gga),
    Rmc(Rmc),
}

/// Parse a checksum-verified sentence (`$GNGGA,...*hh`).
pub fn parse(line: &str) -> Option<Sentence> {
    let body = line.strip_prefix('$')?;
    let body = body.rsplit_once('*').map_or(body, |(b, _)| b);
    let f: Vec<&str> = body.split(',').collect();
    let kind = f.first()?.get(2..)?;
    match kind {
        "GGA" if f.len() >= 12 => Some(Sentence::Gga(gga(&f))),
        "RMC" if f.len() >= 7 => Some(Sentence::Rmc(rmc(&f))),
        _ => None,
    }
}

fn num(s: &str) -> Option<f64> {
    s.parse().ok().filter(|v: &f64| v.is_finite())
}

/// `ddmm.mmmm` / `dddmm.mmmm` with a hemisphere letter.
fn coord(v: &str, hemi: &str) -> Option<f64> {
    let raw = num(v)?;
    let deg = (raw / 100.0).trunc();
    let val = deg + (raw - deg * 100.0) / 60.0;
    match hemi {
        "N" | "E" => Some(val),
        "S" | "W" => Some(-val),
        _ => None,
    }
}

fn gga(f: &[&str]) -> Gga {
    let fix = match f[6] {
        "1" => Fix::ThreeD,
        "2" => Fix::Dgps,
        "4" => Fix::RtkFixed,
        "5" => Fix::RtkFloat,
        _ => Fix::None,
    };
    let h_msl = num(f[9]);
    let sep = num(f[11]);
    let has_fix = fix != Fix::None;
    Gga {
        fix,
        sats: f[7].parse().unwrap_or(0),
        hdop: num(f[8]),
        lat_deg: coord(f[2], f[3]).filter(|_| has_fix),
        lon_deg: coord(f[4], f[5]).filter(|_| has_fix),
        h_msl_m: h_msl.filter(|_| has_fix),
        h_ellipsoid_m: h_msl.zip(sep).map(|(h, s)| h + s).filter(|_| has_fix),
    }
}

fn rmc(f: &[&str]) -> Rmc {
    // RMC cannot tell 2D from 3D; a valid autonomous fix reads as 2d.
    let mode = f.get(12).copied().unwrap_or("A");
    let fix = match (f[2], mode) {
        ("A", "D") => Fix::Dgps,
        ("A", "R") => Fix::RtkFixed,
        ("A", "F") => Fix::RtkFloat,
        ("A", "N") => Fix::None,
        ("A", _) => Fix::TwoD,
        _ => Fix::None,
    };
    let has_fix = fix != Fix::None;
    Rmc {
        fix,
        lat_deg: coord(f[3], f[4]).filter(|_| has_fix),
        lon_deg: coord(f[5], f[6]).filter(|_| has_fix),
    }
}
