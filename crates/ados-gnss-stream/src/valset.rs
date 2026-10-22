//! UBX-CFG-VALSET builders for the RTK base (u-blox F9 configuration keys).
//!
//! A key id carries its value size in bits 28..30 (1 = one bit stored in a
//! byte, 2 = one byte, 3 = two, 4 = four, 5 = eight); [`Item`] holds the value
//! and writes exactly that many little-endian bytes.

use crate::stream::ubx_frame;
use crate::ubx::{CFG_VALSET, CLS_CFG};

/// CFG-TMODE-MODE: 0 disabled, 1 survey-in, 2 fixed.
pub const TMODE_MODE: u32 = 0x2003_0001;
/// CFG-TMODE-POS_TYPE: 0 ECEF, 1 LLH.
pub const TMODE_POS_TYPE: u32 = 0x2003_0002;
/// CFG-TMODE-LAT: fixed latitude, 1e-7 degrees.
pub const TMODE_LAT: u32 = 0x4003_0009;
/// CFG-TMODE-LON: fixed longitude, 1e-7 degrees.
pub const TMODE_LON: u32 = 0x4003_000A;
/// CFG-TMODE-HEIGHT: fixed ellipsoid height, centimetres.
pub const TMODE_HEIGHT: u32 = 0x4003_000B;
/// CFG-TMODE-LAT_HP: high-precision latitude, 1e-9 degrees.
pub const TMODE_LAT_HP: u32 = 0x2003_000C;
/// CFG-TMODE-LON_HP: high-precision longitude, 1e-9 degrees.
pub const TMODE_LON_HP: u32 = 0x2003_000D;
/// CFG-TMODE-HEIGHT_HP: high-precision height, 0.1 mm.
pub const TMODE_HEIGHT_HP: u32 = 0x2003_000E;
/// CFG-TMODE-SVIN_MIN_DUR: survey-in minimum duration, seconds.
pub const TMODE_SVIN_MIN_DUR: u32 = 0x4003_0010;
/// CFG-TMODE-SVIN_ACC_LIMIT: survey-in accuracy limit, 0.1 mm.
pub const TMODE_SVIN_ACC_LIMIT: u32 = 0x4003_0011;

/// `CFG-VALSET` layer bit for RAM (applied now, lost on power cycle).
pub const LAYER_RAM: u8 = 0x01;

/// The receiver port the host is attached to, which selects the per-port
/// CFG-MSGOUT keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RxPort {
    /// UART1.
    #[default]
    Uart1,
    /// USB.
    Usb,
}

impl RxPort {
    /// Parse `uart1` or `usb`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "uart1" => Some(Self::Uart1),
            "usb" => Some(Self::Usb),
            _ => None,
        }
    }
}

/// CFG-MSGOUT key ids `(uart1, usb)` for the RTCM3 base messages
/// 1005, 1074, 1084, 1094, 1124 and 1230.
const RTCM_MSGOUT: [(u16, u32, u32); 6] = [
    (1005, 0x2091_02BE, 0x2091_02C0),
    (1074, 0x2091_035F, 0x2091_0361),
    (1084, 0x2091_0364, 0x2091_0366),
    (1094, 0x2091_0369, 0x2091_036B),
    (1124, 0x2091_036E, 0x2091_0370),
    (1230, 0x2091_0304, 0x2091_0306),
];
/// CFG-MSGOUT-UBX_NAV_PVT key ids `(uart1, usb)`.
pub const NAV_PVT_MSGOUT: (u32, u32) = (0x2091_0007, 0x2091_0009);
/// CFG-MSGOUT-UBX_NAV_DOP key ids `(uart1, usb)`.
pub const NAV_DOP_MSGOUT: (u32, u32) = (0x2091_0039, 0x2091_003B);
/// CFG-MSGOUT-UBX_NAV_SVIN key ids `(uart1, usb)`.
pub const NAV_SVIN_MSGOUT: (u32, u32) = (0x2091_0089, 0x2091_008B);
/// CFG-MSGOUT-UBX_NAV_RELPOSNED key ids `(uart1, usb)`.
pub const NAV_RELPOSNED_MSGOUT: (u32, u32) = (0x2091_008E, 0x2091_0090);
/// CFG-MSGOUT-UBX_TIM_TP key ids `(uart1, usb)`.
pub const TIM_TP_MSGOUT: (u32, u32) = (0x2091_017E, 0x2091_0180);

fn port_key(keys: (u32, u32), port: RxPort) -> u32 {
    match port {
        RxPort::Uart1 => keys.0,
        RxPort::Usb => keys.1,
    }
}

/// One key/value pair; the value is truncated to the key's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item(pub u32, pub i64);

fn value_len(key: u32) -> usize {
    match (key >> 28) & 0x07 {
        1 | 2 => 1,
        3 => 2,
        4 => 4,
        _ => 8,
    }
}

/// A CFG-VALSET frame applying `items` to `layers`.
pub fn valset(layers: u8, items: &[Item]) -> Vec<u8> {
    let mut p = vec![0x00, layers, 0x00, 0x00];
    for &Item(key, value) in items {
        p.extend_from_slice(&key.to_le_bytes());
        p.extend_from_slice(&value.to_le_bytes()[..value_len(key)]);
    }
    ubx_frame(CLS_CFG, CFG_VALSET, &p)
}

/// RTCM3 base message output on `port` at `rate` (0 disables).
fn rtcm_items(port: RxPort, rate: i64) -> impl Iterator<Item = Item> {
    RTCM_MSGOUT
        .into_iter()
        .map(move |(_, uart, usb)| Item(port_key((uart, usb), port), rate))
}

/// Output rate 1 for the RTCM3 base messages and NAV-SVIN on `port`.
fn msgout_items(port: RxPort) -> Vec<Item> {
    let mut v: Vec<Item> = rtcm_items(port, 1).collect();
    v.push(Item(port_key(NAV_SVIN_MSGOUT, port), 1));
    v
}

/// Enable, once per solution, the UBX messages this crate decodes on
/// `port`: NAV-PVT and NAV-DOP always; TIM-TP and NAV-SVIN on the base
/// receiver; NAV-RELPOSNED on a heading rover. A factory-default receiver
/// emits only NMEA, so this runs every time the port opens.
pub fn nav_outputs(port: RxPort, base: bool, heading: bool) -> Vec<u8> {
    let mut keys = vec![NAV_PVT_MSGOUT, NAV_DOP_MSGOUT];
    if base {
        keys.extend([TIM_TP_MSGOUT, NAV_SVIN_MSGOUT]);
    }
    if heading {
        keys.push(NAV_RELPOSNED_MSGOUT);
    }
    let items: Vec<Item> = keys
        .into_iter()
        .map(|k| Item(port_key(k, port), 1))
        .collect();
    valset(LAYER_RAM, &items)
}

/// Start a survey-in (TMODE 1) and enable the base output.
pub fn survey_in(port: RxPort, min_dur_s: u32, accuracy_m: f64) -> Vec<u8> {
    let acc = (accuracy_m * 10_000.0).round().clamp(1.0, u32::MAX as f64) as i64;
    let mut items = vec![
        Item(TMODE_MODE, 1),
        Item(TMODE_SVIN_MIN_DUR, min_dur_s as i64),
        Item(TMODE_SVIN_ACC_LIMIT, acc),
    ];
    items.extend(msgout_items(port));
    valset(LAYER_RAM, &items)
}

/// Split a value into the standard part (`unit`) and a high-precision rest
/// (`unit / 100`), the TMODE LLH encoding.
fn split_hp(value: f64, unit_per: f64) -> (i64, i64) {
    let scaled = value * unit_per;
    let main = scaled.trunc();
    let hp = ((scaled - main) * 100.0).round().clamp(-99.0, 99.0);
    (main as i64, hp as i64)
}

/// Fixed base position (TMODE 2, LLH) and enable the base output.
pub fn fixed(port: RxPort, lat_deg: f64, lon_deg: f64, h_ellipsoid_m: f64) -> Vec<u8> {
    let (lat, lat_hp) = split_hp(lat_deg, 1e7);
    let (lon, lon_hp) = split_hp(lon_deg, 1e7);
    let (h, h_hp) = split_hp(h_ellipsoid_m, 100.0);
    let mut items = vec![
        Item(TMODE_MODE, 2),
        Item(TMODE_POS_TYPE, 1),
        Item(TMODE_LAT, lat),
        Item(TMODE_LON, lon),
        Item(TMODE_HEIGHT, h),
        Item(TMODE_LAT_HP, lat_hp),
        Item(TMODE_LON_HP, lon_hp),
        Item(TMODE_HEIGHT_HP, h_hp),
    ];
    items.extend(msgout_items(port));
    valset(LAYER_RAM, &items)
}

/// Leave base mode (TMODE 0) and stop the RTCM3 base output on `port`.
pub fn off(port: RxPort) -> Vec<u8> {
    let mut items = vec![Item(TMODE_MODE, 0)];
    items.extend(rtcm_items(port, 0));
    valset(LAYER_RAM, &items)
}

/// Poll UBX-MON-VER (empty payload).
pub fn poll_mon_ver() -> Vec<u8> {
    ubx_frame(crate::ubx::CLS_MON, crate::ubx::MON_VER, &[])
}

#[cfg(test)]
mod tests;
