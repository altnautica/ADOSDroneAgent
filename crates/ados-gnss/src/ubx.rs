//! UBX message decode for the messages this service consumes. Offsets follow
//! the u-blox F9 interface description; every decoder checks the payload
//! length first and returns `None` for a short or foreign payload.

pub const CLS_NAV: u8 = 0x01;
pub const CLS_ACK: u8 = 0x05;
pub const CLS_CFG: u8 = 0x06;
pub const CLS_MON: u8 = 0x0A;
pub const CLS_TIM: u8 = 0x0D;

pub const NAV_DOP: u8 = 0x04;
pub const NAV_PVT: u8 = 0x07;
pub const NAV_SVIN: u8 = 0x3B;
pub const NAV_RELPOSNED: u8 = 0x3C;
pub const ACK_NAK: u8 = 0x00;
pub const ACK_ACK: u8 = 0x01;
pub const CFG_VALSET: u8 = 0x8A;
pub const MON_VER: u8 = 0x04;
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
    pub fix_type: u8,
    pub gnss_fix_ok: bool,
    pub diff_soln: bool,
    /// 0 none, 1 float, 2 fixed.
    pub carr_soln: u8,
    pub num_sv: u8,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub h_ellipsoid_m: f64,
    pub h_msl_m: f64,
    pub pdop: f64,
    /// UTC of the solution epoch, when date and time are valid and resolved.
    pub utc_ns: Option<i64>,
}

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
        pdop: u16_at(p, 76) as f64 / 100.0,
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
    pub dur_s: u32,
    /// Mean position, ECEF metres.
    pub mean_ecef_m: [f64; 3],
    pub mean_acc_m: f64,
    pub valid: bool,
    pub active: bool,
}

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
    pub heading_deg: f64,
    pub acc_heading_deg: f64,
    pub length_m: f64,
    pub gnss_fix_ok: bool,
    pub rel_pos_valid: bool,
    pub is_moving: bool,
    pub heading_valid: bool,
}

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
