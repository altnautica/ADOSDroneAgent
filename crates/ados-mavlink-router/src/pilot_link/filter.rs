//! The client-frame filter while the pilot owns motion: stick and override
//! frames dropped, gotos redirected to the pilot.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage};

use super::{MSG_MANUAL_CONTROL, MSG_RC_CHANNELS_OVERRIDE};

/// The vehicle's current global and local position, to turn a global target
/// into the FC's local frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoRef {
    pub lat: f64,
    pub lon: f64,
    /// Altitude above home, metres.
    pub alt_rel: f64,
    /// Local NED position at the same moment.
    pub local: [f64; 3],
}

impl GeoRef {
    /// Local NED of a global point (`alt_rel` above home).
    pub fn to_local(&self, lat: f64, lon: f64, alt_rel: f64) -> [f32; 3] {
        const R: f64 = 6_378_137.0;
        let dn = (lat - self.lat).to_radians() * R;
        let de = (lon - self.lon).to_radians() * R * self.lat.to_radians().cos();
        let dd = -(alt_rel - self.alt_rel);
        [
            (self.local[0] + dn) as f32,
            (self.local[1] + de) as f32,
            (self.local[2] + dd) as f32,
        ]
    }
}

/// What happens to one client frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Pass,
    Drop,
    Goto([f32; 3], Option<f32>),
}

const POS_IGNORE: u16 = 0b111;
const YAW_IGNORE: u16 = 1 << 10;

/// Classify one decoded client frame while the pilot owns motion.
pub fn classify(msg_id: u32, msg: Option<&MavMessage>, geo: Option<GeoRef>) -> Verdict {
    if msg_id == MSG_MANUAL_CONTROL || msg_id == MSG_RC_CHANNELS_OVERRIDE {
        return Verdict::Drop;
    }
    let Some(msg) = msg else { return Verdict::Pass };
    let global = |lat: f64, lon: f64, alt: f64| geo.map(|g| g.to_local(lat, lon, alt));
    let target = match msg {
        MavMessage::COMMAND_LONG(c) if c.command == MavCmd::MAV_CMD_DO_REPOSITION => Some(
            global(
                f64::from(c.param5),
                f64::from(c.param6),
                f64::from(c.param7),
            )
            .map(|p| (p, yaw_param(c.param4))),
        ),
        MavMessage::COMMAND_INT(c) if c.command == MavCmd::MAV_CMD_DO_REPOSITION => Some(
            global(f64::from(c.x) / 1e7, f64::from(c.y) / 1e7, f64::from(c.z))
                .map(|p| (p, yaw_param(c.param4))),
        ),
        MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => {
            let mask = s.type_mask.bits();
            if mask & POS_IGNORE != 0 {
                return Verdict::Drop;
            }
            Some(Some((
                [s.x, s.y, s.z],
                (mask & YAW_IGNORE == 0).then_some(s.yaw),
            )))
        }
        MavMessage::SET_POSITION_TARGET_GLOBAL_INT(s) => {
            let mask = s.type_mask.bits();
            if mask & POS_IGNORE != 0 {
                return Verdict::Drop;
            }
            let p = global(
                f64::from(s.lat_int) / 1e7,
                f64::from(s.lon_int) / 1e7,
                f64::from(s.alt),
            );
            Some(p.map(|p| (p, (mask & YAW_IGNORE == 0).then_some(s.yaw))))
        }
        _ => None,
    };
    match target {
        None => Verdict::Pass,
        Some(Some((p, yaw))) if p.iter().all(|v| v.is_finite()) => Verdict::Goto(p, yaw),
        Some(_) => Verdict::Drop,
    }
}

pub(super) fn yaw_param(p4: f32) -> Option<f32> {
    p4.is_finite().then(|| p4.to_radians())
}

/// Total length of the frame at `buf[0]`, if its header is complete.
pub(super) fn frame_len(buf: &[u8]) -> Option<usize> {
    match buf.first()? {
        0xFD if buf.len() >= 3 => Some(12 + buf[1] as usize + if buf[2] & 1 != 0 { 13 } else { 0 }),
        0xFE if buf.len() >= 2 => Some(8 + buf[1] as usize),
        _ => None,
    }
}

pub(super) fn frame_id(f: &[u8]) -> u32 {
    if f[0] == 0xFD {
        u32::from_le_bytes([f[7], f[8], f[9], 0])
    } else {
        u32::from(f[5])
    }
}

/// A redirected goto: local NED and optional yaw (radians).
pub type Redirect = ([f32; 3], Option<f32>);

/// A client `DO_REPOSITION` the filter took off the FC stream. The FC never
/// sees it, so the router owes its sender the `COMMAND_ACK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reposition {
    /// The sender's system and component id.
    pub sys: u8,
    pub comp: u8,
    /// Sent to the pilot as a goto; `false` when the target could not be
    /// placed (no position yet, or not a finite point) and it was dropped.
    pub redirected: bool,
}

/// What [`filter`] made of one client write.
#[derive(Debug, Default, PartialEq)]
pub struct Filtered {
    /// The bytes to forward to the FC.
    pub kept: Vec<u8>,
    /// The gotos to send the pilot.
    pub gotos: Vec<Redirect>,
    /// The motion frames dropped.
    pub dropped: u64,
    /// The repositions to answer.
    pub repositions: Vec<Reposition>,
}

/// Filter a client write: the bytes to forward, and the gotos to redirect.
/// Bytes that are not whole frames pass through untouched.
pub fn filter(data: &[u8], geo: Option<GeoRef>) -> Filtered {
    let mut out = Filtered {
        kept: Vec::with_capacity(data.len()),
        ..Filtered::default()
    };
    let mut i = 0;
    while i < data.len() {
        let rest = &data[i..];
        let Some(n) = frame_len(rest).filter(|&n| n <= rest.len() && n >= 8) else {
            out.kept.extend_from_slice(rest);
            break;
        };
        let frame = &rest[..n];
        let decoded = ados_protocol::mavlink::parse_any(frame).ok();
        let msg = decoded.as_ref().map(|(_, m)| m);
        let verdict = classify(frame_id(frame), msg, geo);
        if let Some((h, m)) = &decoded {
            if is_reposition(m) {
                out.repositions.push(Reposition {
                    sys: h.system_id,
                    comp: h.component_id,
                    redirected: matches!(verdict, Verdict::Goto(..)),
                });
            }
        }
        match verdict {
            Verdict::Pass => out.kept.extend_from_slice(frame),
            Verdict::Drop => out.dropped += 1,
            Verdict::Goto(p, yaw) => out.gotos.push((p, yaw)),
        }
        i += n;
    }
    out
}

fn is_reposition(m: &MavMessage) -> bool {
    match m {
        MavMessage::COMMAND_LONG(c) => c.command == MavCmd::MAV_CMD_DO_REPOSITION,
        MavMessage::COMMAND_INT(c) => c.command == MavCmd::MAV_CMD_DO_REPOSITION,
        _ => false,
    }
}
