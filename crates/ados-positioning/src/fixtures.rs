//! Test fixtures: byte-exact receiver frames with correct checksums.

use crate::stream::{crc24q, ubx_frame};
use crate::ubx;

/// `$body*hh\r\n`.
pub fn nmea(body: &str) -> Vec<u8> {
    let ck = body.bytes().fold(0u8, |a, c| a ^ c);
    format!("${body}*{ck:02X}\r\n").into_bytes()
}

/// An RTCM3 frame of message `msg` with `extra` filler payload bytes.
pub fn rtcm(msg: u16, extra: usize) -> Vec<u8> {
    let mut payload = vec![(msg >> 4) as u8, ((msg & 0x0F) << 4) as u8];
    payload.extend((0..extra).map(|i| i as u8));
    let len = payload.len();
    let mut f = vec![0xD3, (len >> 8) as u8 & 0x03, len as u8];
    f.extend_from_slice(&payload);
    let crc = crc24q(&f);
    f.extend_from_slice(&[(crc >> 16) as u8, (crc >> 8) as u8, crc as u8]);
    f
}

fn put_u16(p: &mut [u8], o: usize, v: u16) {
    p[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(p: &mut [u8], o: usize, v: u32) {
    p[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_i32(p: &mut [u8], o: usize, v: i32) {
    p[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// NAV-PVT fields a test sets.
#[derive(Debug, Clone, Copy)]
pub struct Pvt {
    /// `fixType`.
    pub fix_type: u8,
    /// `flags` byte (bit 0 `gnssFixOK`, bit 1 `diffSoln`, bits 6..7 `carrSoln`).
    pub flags: u8,
    /// Satellites used.
    pub num_sv: u8,
    /// Latitude, 1e-7 degrees.
    pub lat_e7: i32,
    /// Longitude, 1e-7 degrees.
    pub lon_e7: i32,
    /// Height above the ellipsoid, millimetres.
    pub height_mm: i32,
    /// Height above mean sea level, millimetres.
    pub hmsl_mm: i32,
    /// Position DOP, 0.01 units.
    pub pdop_centi: u16,
    /// `(year, month, day, hour, min, sec, nano)`; `None` leaves time invalid.
    pub utc: Option<(u16, u8, u8, u8, u8, u8, i32)>,
}

impl Default for Pvt {
    fn default() -> Self {
        Self {
            fix_type: 3,
            flags: 0x01,
            num_sv: 14,
            lat_e7: 129_716_000,
            lon_e7: 775_946_000,
            height_mm: 920_400,
            hmsl_mm: 1_006_100,
            pdop_centi: 120,
            utc: None,
        }
    }
}

/// A NAV-PVT frame carrying `v`.
pub fn nav_pvt(v: Pvt) -> Vec<u8> {
    let mut p = vec![0u8; 92];
    if let Some((y, mo, d, h, mi, s, nano)) = v.utc {
        put_u16(&mut p, 4, y);
        p[6] = mo;
        p[7] = d;
        p[8] = h;
        p[9] = mi;
        p[10] = s;
        p[11] = 0x07;
        put_i32(&mut p, 16, nano);
    }
    p[20] = v.fix_type;
    p[21] = v.flags;
    p[23] = v.num_sv;
    put_i32(&mut p, 24, v.lon_e7);
    put_i32(&mut p, 28, v.lat_e7);
    put_i32(&mut p, 32, v.height_mm);
    put_i32(&mut p, 36, v.hmsl_mm);
    put_u16(&mut p, 76, v.pdop_centi);
    ubx_frame(ubx::CLS_NAV, ubx::NAV_PVT, &p)
}

/// A NAV-DOP frame with the horizontal DOP in 0.01 units.
pub fn nav_dop(hdop_centi: u16) -> Vec<u8> {
    let mut p = vec![0u8; 18];
    put_u16(&mut p, 12, hdop_centi);
    ubx_frame(ubx::CLS_NAV, ubx::NAV_DOP, &p)
}

/// NAV-SVIN with the mean in cm and accuracy in 0.1 mm.
pub fn nav_svin(
    dur_s: u32,
    mean_cm: [i32; 3],
    acc_01mm: u32,
    valid: bool,
    active: bool,
) -> Vec<u8> {
    let mut p = vec![0u8; 40];
    put_u32(&mut p, 8, dur_s);
    put_i32(&mut p, 12, mean_cm[0]);
    put_i32(&mut p, 16, mean_cm[1]);
    put_i32(&mut p, 20, mean_cm[2]);
    put_u32(&mut p, 28, acc_01mm);
    p[36] = valid as u8;
    p[37] = active as u8;
    ubx_frame(ubx::CLS_NAV, ubx::NAV_SVIN, &p)
}

/// NAV-RELPOSNED v1: heading and accuracy in 1e-5 deg, length in cm.
pub fn nav_relposned(heading_e5: i32, acc_e5: u32, length_cm: i32, flags: u32) -> Vec<u8> {
    let mut p = vec![0u8; 64];
    p[0] = 0x01;
    put_i32(&mut p, 20, length_cm);
    put_i32(&mut p, 24, heading_e5);
    put_u32(&mut p, 52, acc_e5);
    put_u32(&mut p, 60, flags);
    ubx_frame(ubx::CLS_NAV, ubx::NAV_RELPOSNED, &p)
}

/// A TIM-TP frame.
pub fn tim_tp(tow_ms: u32, sub_ms: u32, week: u16, flags: u8) -> Vec<u8> {
    let mut p = vec![0u8; 16];
    put_u32(&mut p, 0, tow_ms);
    put_u32(&mut p, 4, sub_ms);
    put_u16(&mut p, 12, week);
    p[14] = flags;
    ubx_frame(ubx::CLS_TIM, ubx::TIM_TP, &p)
}

/// A MON-VER frame with hardware version `hw` and extension strings.
pub fn mon_ver(hw: &str, extensions: &[&str]) -> Vec<u8> {
    let mut p = vec![0u8; 40 + 30 * extensions.len()];
    p[..9].copy_from_slice(b"EXT CORE ");
    p[30..30 + hw.len()].copy_from_slice(hw.as_bytes());
    for (i, e) in extensions.iter().enumerate() {
        let o = 40 + 30 * i;
        p[o..o + e.len()].copy_from_slice(e.as_bytes());
    }
    ubx_frame(ubx::CLS_MON, ubx::MON_VER, &p)
}

/// An ACK-ACK (`ok`) or ACK-NAK frame for `(class, id)`.
pub fn ack(ok: bool, class: u8, id: u8) -> Vec<u8> {
    let msg = if ok { ubx::ACK_ACK } else { ubx::ACK_NAK };
    ubx_frame(ubx::CLS_ACK, msg, &[class, id])
}

/// Split one UBX frame back into (class, id, payload), checking the checksum.
pub fn unframe(f: &[u8]) -> (u8, u8, Vec<u8>) {
    let mut s = crate::stream::Splitter::new();
    match s.push(f).pop() {
        Some(crate::stream::Frame::Ubx { class, id, payload }) => (class, id, payload),
        other => panic!("not one UBX frame: {other:?}"),
    }
}
