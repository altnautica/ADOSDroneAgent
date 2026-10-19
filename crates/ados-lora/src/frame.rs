//! The lane's own frame: `ver | node_id (u16 BE) | kind | len | payload (<= 48) |
//! crc16 (CCITT-FALSE, BE, over every preceding byte)`.

use ados_sensorhub_proto::crc::crc16;
use thiserror::Error;

pub const VERSION: u8 = 1;
pub const MAX_PAYLOAD: usize = 48;
const HEADER: usize = 5;
const TRAILER: usize = 2;
/// Largest encoded frame.
pub const MAX_FRAME: usize = HEADER + MAX_PAYLOAD + TRAILER;
/// Longest mode string a beacon carries.
pub const MAX_MODE: usize = 12;
/// The longest beacon frame: position, altitude, battery and a full mode.
pub const BEACON_MAX_FRAME_LEN: usize = HEADER + 12 + MAX_MODE + TRAILER;
/// A status frame.
pub const STATUS_FRAME_LEN: usize = HEADER + 4 + TRAILER;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Beacon = 1,
    Status = 2,
    Text = 3,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Beacon => "beacon",
            Kind::Status => "status",
            Kind::Text => "text",
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Kind::Beacon),
            2 => Some(Kind::Status),
            3 => Some(Kind::Text),
            _ => None,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame too short")]
    Short,
    #[error("unsupported frame version {0}")]
    Version(u8),
    #[error("unknown frame kind {0}")]
    Kind(u8),
    #[error("payload over {MAX_PAYLOAD} bytes")]
    TooLong,
    #[error("crc mismatch")]
    Crc,
    #[error("malformed payload")]
    Payload,
}

/// A decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub node_id: u16,
    pub kind: Kind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(FrameError::TooLong);
        }
        let mut out = Vec::with_capacity(HEADER + self.payload.len() + TRAILER);
        out.push(VERSION);
        out.extend_from_slice(&self.node_id.to_be_bytes());
        out.push(self.kind as u8);
        out.push(self.payload.len() as u8);
        out.extend_from_slice(&self.payload);
        let crc = crc16(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        Ok(out)
    }

    /// Decode one frame from the front of `buf`; returns the frame and the
    /// bytes it used.
    pub fn decode(buf: &[u8]) -> Result<(Frame, usize), FrameError> {
        if buf.len() < HEADER + TRAILER {
            return Err(FrameError::Short);
        }
        if buf[0] != VERSION {
            return Err(FrameError::Version(buf[0]));
        }
        let len = buf[4] as usize;
        if len > MAX_PAYLOAD {
            return Err(FrameError::TooLong);
        }
        let total = HEADER + len + TRAILER;
        if buf.len() < total {
            return Err(FrameError::Short);
        }
        let crc = u16::from_be_bytes([buf[total - 2], buf[total - 1]]);
        if crc16(&buf[..total - 2]) != crc {
            return Err(FrameError::Crc);
        }
        let kind = Kind::from_u8(buf[3]).ok_or(FrameError::Kind(buf[3]))?;
        Ok((
            Frame {
                node_id: u16::from_be_bytes([buf[1], buf[2]]),
                kind,
                payload: buf[HEADER..HEADER + len].to_vec(),
            },
            total,
        ))
    }
}

/// A beacon: position, battery and flight mode of the sending node.
#[derive(Debug, Clone, PartialEq)]
pub struct Beacon {
    /// `None` when the node has no position.
    pub position: Option<(f64, f64)>,
    pub alt_m: Option<f64>,
    pub battery_pct: Option<u8>,
    pub mode: String,
}

const NO_POS: i32 = i32::MIN;
const NO_ALT: i16 = i16::MIN;
const NO_BATT: u8 = 0xFF;

impl Beacon {
    pub fn to_payload(&self) -> Vec<u8> {
        let (lat, lon) = self.position.map_or((NO_POS, NO_POS), |(la, lo)| {
            ((la * 1e7).round() as i32, (lo * 1e7).round() as i32)
        });
        let alt = self
            .alt_m
            .map_or(NO_ALT, |a| a.round().clamp(-32767.0, 32767.0) as i16);
        let mode: Vec<u8> = self
            .mode
            .bytes()
            .filter(u8::is_ascii_graphic)
            .take(MAX_MODE)
            .collect();
        let mut p = Vec::with_capacity(12 + mode.len());
        p.extend_from_slice(&lat.to_be_bytes());
        p.extend_from_slice(&lon.to_be_bytes());
        p.extend_from_slice(&alt.to_be_bytes());
        p.push(self.battery_pct.map_or(NO_BATT, |b| b.min(100)));
        p.push(mode.len() as u8);
        p.extend_from_slice(&mode);
        p
    }

    pub fn from_payload(p: &[u8]) -> Result<Self, FrameError> {
        if p.len() < 12 {
            return Err(FrameError::Payload);
        }
        let lat = i32::from_be_bytes([p[0], p[1], p[2], p[3]]);
        let lon = i32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        let alt = i16::from_be_bytes([p[8], p[9]]);
        let batt = p[10];
        let mlen = p[11] as usize;
        if mlen > MAX_MODE || p.len() < 12 + mlen {
            return Err(FrameError::Payload);
        }
        let mode = std::str::from_utf8(&p[12..12 + mlen]).map_err(|_| FrameError::Payload)?;
        Ok(Beacon {
            position: (lat != NO_POS && lon != NO_POS)
                .then(|| (f64::from(lat) / 1e7, f64::from(lon) / 1e7)),
            alt_m: (alt != NO_ALT).then_some(f64::from(alt)),
            battery_pct: (batt != NO_BATT).then_some(batt),
            mode: mode.to_string(),
        })
    }
}

/// A status report: how well the sender heard `about_node`'s last beacon.
/// Feeds adaptive power control on the node it is about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Status {
    pub about_node: u16,
    pub rssi_dbm: i8,
    pub snr_db: f32,
}

impl Status {
    pub fn to_payload(&self) -> Vec<u8> {
        let mut p = self.about_node.to_be_bytes().to_vec();
        p.push(self.rssi_dbm as u8);
        p.push(((self.snr_db * 4.0).round().clamp(-128.0, 127.0) as i8) as u8);
        p
    }

    pub fn from_payload(p: &[u8]) -> Result<Self, FrameError> {
        if p.len() < 4 {
            return Err(FrameError::Payload);
        }
        Ok(Status {
            about_node: u16::from_be_bytes([p[0], p[1]]),
            rssi_dbm: p[2] as i8,
            snr_db: f32::from(p[3] as i8) / 4.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_round_trips_through_a_frame() {
        let b = Beacon {
            position: Some((12.971_6, 77.594_6)),
            alt_m: Some(920.0),
            battery_pct: Some(81),
            mode: "AUTO".into(),
        };
        let f = Frame {
            node_id: 17,
            kind: Kind::Beacon,
            payload: b.to_payload(),
        };
        let bytes = f.encode().unwrap();
        assert_eq!(bytes.len(), 23);
        let (back, used) = Frame::decode(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(back, f);
        let bb = Beacon::from_payload(&back.payload).unwrap();
        assert_eq!(bb.mode, "AUTO");
        assert_eq!(bb.battery_pct, Some(81));
        let (la, lo) = bb.position.unwrap();
        assert!((la - 12.971_6).abs() < 1e-7 && (lo - 77.594_6).abs() < 1e-7);
    }

    #[test]
    fn a_flipped_bit_fails_the_crc() {
        let f = Frame {
            node_id: 1,
            kind: Kind::Text,
            payload: b"hi".to_vec(),
        };
        let mut bytes = f.encode().unwrap();
        bytes[5] ^= 1;
        assert_eq!(Frame::decode(&bytes), Err(FrameError::Crc));
    }

    #[test]
    fn missing_fields_round_trip_as_none() {
        let b = Beacon {
            position: None,
            alt_m: None,
            battery_pct: None,
            mode: String::new(),
        };
        assert_eq!(Beacon::from_payload(&b.to_payload()).unwrap(), b);
    }

    #[test]
    fn status_round_trips() {
        let s = Status {
            about_node: 42,
            rssi_dbm: -97,
            snr_db: 6.5,
        };
        assert_eq!(Status::from_payload(&s.to_payload()).unwrap(), s);
    }

    #[test]
    fn oversize_payload_is_refused() {
        let f = Frame {
            node_id: 1,
            kind: Kind::Text,
            payload: vec![0; MAX_PAYLOAD + 1],
        };
        assert_eq!(f.encode(), Err(FrameError::TooLong));
    }
}
