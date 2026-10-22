//! UBX message decode for the navigation, timing, acknowledgement and version
//! messages a host typically consumes. Offsets follow the u-blox F9 interface
//! description; every decoder checks the payload length first and returns
//! `None` for a short or foreign payload.

/// UBX class NAV (navigation results).
pub const CLS_NAV: u8 = 0x01;
/// UBX class ACK (acknowledgements).
pub const CLS_ACK: u8 = 0x05;
/// UBX class CFG (configuration).
pub const CLS_CFG: u8 = 0x06;
/// UBX class MON (monitoring).
pub const CLS_MON: u8 = 0x0A;
/// UBX class TIM (timing).
pub const CLS_TIM: u8 = 0x0D;

/// Message id of NAV-DOP.
pub const NAV_DOP: u8 = 0x04;
/// Message id of NAV-PVT.
pub const NAV_PVT: u8 = 0x07;
/// Message id of NAV-SVIN.
pub const NAV_SVIN: u8 = 0x3B;
/// Message id of NAV-RELPOSNED.
pub const NAV_RELPOSNED: u8 = 0x3C;
/// Message id of ACK-NAK.
pub const ACK_NAK: u8 = 0x00;
/// Message id of ACK-ACK.
pub const ACK_ACK: u8 = 0x01;
/// Message id of CFG-VALSET.
pub const CFG_VALSET: u8 = 0x8A;
/// Message id of MON-VER.
pub const MON_VER: u8 = 0x04;
/// Message id of TIM-TP.
pub const TIM_TP: u8 = 0x01;

/// Seconds between the Unix epoch and the GPS epoch (1980-01-06).
const GPS_EPOCH_UNIX_S: i64 = 315_964_800;
/// GPS − UTC leap seconds, constant since 2017-01-01.
pub const GPS_UTC_LEAP_S: i64 = 18;

fn u16_at(p: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([p[o], p[o + 1]])
}
fn u32_at(p: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]])
}
fn i32_at(p: &[u8], o: usize) -> i32 {
    u32_at(p, o) as i32
}

/// UBX-NAV-PVT.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavPvt {
    /// GNSS fix type (0 none, 1 dead reckoning, 2 2D, 3 3D, 4 combined, 5 time only).
    pub fix_type: u8,
    /// The fix is within the receiver's accuracy masks.
    pub gnss_fix_ok: bool,
    /// Differential corrections were applied.
    pub diff_soln: bool,
    /// 0 none, 1 float, 2 fixed.
    pub carr_soln: u8,
    /// Satellites used in the solution.
    pub num_sv: u8,
    /// Latitude, degrees.
    pub lat_deg: f64,
    /// Longitude, degrees.
    pub lon_deg: f64,
    /// Height above the ellipsoid, metres.
    pub h_ellipsoid_m: f64,
    /// Height above mean sea level, metres.
    pub h_msl_m: f64,
    /// UTC of the solution epoch, when date and time are valid and resolved.
    pub utc_ns: Option<i64>,
}

/// Decode a NAV-PVT payload.
pub fn nav_pvt(p: &[u8]) -> Option<NavPvt> {
    if p.len() < 92 {
        return None;
    }
    let flags = p[21];
    let valid = p[11];
    let utc_ns = (valid & 0x07 == 0x07)
        .then(|| {
            let days = days_from_civil(u16_at(p, 4) as i64, p[6] as i64, p[7] as i64);
            let secs = days * 86_400 + p[8] as i64 * 3600 + p[9] as i64 * 60 + p[10] as i64;
            secs * 1_000_000_000 + i32_at(p, 16) as i64
        })
        .filter(|_| (1..=12).contains(&p[6]) && (1..=31).contains(&p[7]));
    Some(NavPvt {
        fix_type: p[20],
        gnss_fix_ok: flags & 0x01 != 0,
        diff_soln: flags & 0x02 != 0,
        carr_soln: (flags >> 6) & 0x03,
        num_sv: p[23],
        lon_deg: i32_at(p, 24) as f64 * 1e-7,
        lat_deg: i32_at(p, 28) as f64 * 1e-7,
        h_ellipsoid_m: i32_at(p, 32) as f64 / 1000.0,
        h_msl_m: i32_at(p, 36) as f64 / 1000.0,
        utc_ns,
    })
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// UBX-NAV-DOP: the horizontal DOP.
pub fn nav_dop_hdop(p: &[u8]) -> Option<f64> {
    (p.len() >= 18).then(|| u16_at(p, 12) as f64 / 100.0)
}

/// UBX-NAV-SVIN.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavSvin {
    /// Survey-in duration so far, seconds.
    pub dur_s: u32,
    /// Mean position, ECEF metres.
    pub mean_ecef_m: [f64; 3],
    /// Mean position accuracy, metres.
    pub mean_acc_m: f64,
    /// The survey-in met its limits and the position is valid.
    pub valid: bool,
    /// A survey-in is in progress.
    pub active: bool,
}

/// Decode a NAV-SVIN payload.
pub fn nav_svin(p: &[u8]) -> Option<NavSvin> {
    if p.len() < 40 {
        return None;
    }
    let axis = |o: usize, hp: usize| i32_at(p, o) as f64 * 0.01 + (p[hp] as i8) as f64 * 1e-4;
    Some(NavSvin {
        dur_s: u32_at(p, 8),
        mean_ecef_m: [axis(12, 24), axis(16, 25), axis(20, 26)],
        mean_acc_m: u32_at(p, 28) as f64 * 1e-4,
        valid: p[36] != 0,
        active: p[37] != 0,
    })
}

/// UBX-NAV-RELPOSNED (version 1, F9).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelPosNed {
    /// Heading of the relative position vector, degrees.
    pub heading_deg: f64,
    /// Heading accuracy, degrees.
    pub acc_heading_deg: f64,
    /// Length of the relative position vector, metres.
    pub length_m: f64,
    /// The fix is within the receiver's accuracy masks.
    pub gnss_fix_ok: bool,
    /// The relative position components are valid.
    pub rel_pos_valid: bool,
    /// The receiver runs in moving-base mode.
    pub is_moving: bool,
    /// The heading is valid.
    pub heading_valid: bool,
}

/// Decode a NAV-RELPOSNED (version 1) payload.
pub fn nav_relposned(p: &[u8]) -> Option<RelPosNed> {
    if p.len() < 64 || p[0] != 0x01 {
        return None;
    }
    let flags = u32_at(p, 60);
    Some(RelPosNed {
        heading_deg: i32_at(p, 24) as f64 * 1e-5,
        acc_heading_deg: u32_at(p, 52) as f64 * 1e-5,
        length_m: i32_at(p, 20) as f64 * 0.01 + (p[35] as i8) as f64 * 1e-4,
        gnss_fix_ok: flags & (1 << 0) != 0,
        rel_pos_valid: flags & (1 << 2) != 0,
        is_moving: flags & (1 << 5) != 0,
        heading_valid: flags & (1 << 8) != 0,
    })
}

/// UBX-TIM-TP: the UTC (Unix ns) of the next time pulse.
pub fn tim_tp_utc_ns(p: &[u8]) -> Option<i64> {
    if p.len() < 16 {
        return None;
    }
    let tow_ms = u32_at(p, 0) as i64;
    let sub_ms = u32_at(p, 4) as i64; // 2^-32 ms
    let week = u16_at(p, 12) as i64;
    let utc_base = p[14] & 0x01 != 0;
    let mut ns = (GPS_EPOCH_UNIX_S + week * 604_800) * 1_000_000_000
        + tow_ms * 1_000_000
        + ((sub_ms * 1_000_000) >> 32);
    if !utc_base {
        ns -= GPS_UTC_LEAP_S * 1_000_000_000;
    }
    Some(ns)
}

/// UBX-MON-VER: the module name (`MOD=` extension), else the hardware
/// version string.
pub fn mon_ver_model(p: &[u8]) -> Option<String> {
    if p.len() < 40 {
        return None;
    }
    let text = |b: &[u8]| {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).trim().to_string()
    };
    let module = p[40..]
        .as_chunks::<30>()
        .0
        .iter()
        .map(|c| text(c.as_slice()))
        .find_map(|e| e.strip_prefix("MOD=").map(str::to_string));
    Some(match module {
        Some(m) => format!("u-blox {m}"),
        None => format!("u-blox hw {}", text(&p[30..40])),
    })
}

/// UBX-ACK-ACK / ACK-NAK: the acknowledged (class, id).
pub fn ack_target(p: &[u8]) -> Option<(u8, u8)> {
    (p.len() >= 2).then(|| (p[0], p[1]))
}
