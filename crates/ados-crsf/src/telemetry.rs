//! Telemetry frame codecs: link statistics (0x14), GPS (0x02), battery
//! (0x08), attitude (0x1E), and flight mode (0x21).
//!
//! Field order, widths, and unit conventions follow the CRSF telemetry frame
//! set. Multi-byte fields travel big-endian on the wire (the CRSF network
//! order for telemetry payloads); the link-statistics frame is all
//! single-byte fields, so the lane's health signal has no byte-order
//! ambiguity. Scaled interpretations of GPS/battery/attitude values are
//! documented per field and bench-verified against a real module before any
//! consumer trusts them; the structs carry the raw wire integers.

use crate::frame::{
    build_frame, FrameError, RawFrame, TYPE_ATTITUDE, TYPE_BATTERY, TYPE_FLIGHT_MODE, TYPE_GPS,
    TYPE_LINK_STATISTICS,
};

/// Link statistics (type 0x14, 10-byte payload) — the received-side proof the
/// lane's liveness verdict keys on. All fields are one byte wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkStatistics {
    /// Uplink RSSI, antenna 1, dBm (negative).
    pub uplink_rssi_ant1: i8,
    /// Uplink RSSI, antenna 2, dBm (negative).
    pub uplink_rssi_ant2: i8,
    /// Uplink link quality, 0..=100.
    pub uplink_lq: u8,
    /// Uplink SNR, dB.
    pub uplink_snr: i8,
    /// Active antenna index (0 or 1).
    pub active_antenna: u8,
    /// RF mode (packet-rate index; the mapping to Hz is module-specific).
    pub rf_mode: u8,
    /// Uplink TX power level: an INDEX into the CRSF power table, NOT a dBm or
    /// mW value. Map it with [`Self::uplink_tx_power_mw`] to report real power;
    /// the table is 0→0 mW, 1→10, 2→25, 3→100, 4→500, 5→1000, 6→2000, 7→250,
    /// 8→50 (mW).
    pub uplink_tx_power: u8,
    /// Downlink RSSI, dBm (negative).
    pub downlink_rssi: i8,
    /// Downlink link quality, 0..=100.
    pub downlink_lq: u8,
    /// Downlink SNR, dB.
    pub downlink_snr: i8,
}

impl LinkStatistics {
    pub const PAYLOAD_SIZE: usize = 10;

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        if payload.len() != Self::PAYLOAD_SIZE {
            return Err(FrameError::PayloadSizeMismatch {
                frame_type: TYPE_LINK_STATISTICS,
                expected: Self::PAYLOAD_SIZE,
                got: payload.len(),
            });
        }
        Ok(Self {
            uplink_rssi_ant1: payload[0] as i8,
            uplink_rssi_ant2: payload[1] as i8,
            uplink_lq: payload[2],
            uplink_snr: payload[3] as i8,
            active_antenna: payload[4],
            rf_mode: payload[5],
            uplink_tx_power: payload[6],
            downlink_rssi: payload[7] as i8,
            downlink_lq: payload[8],
            downlink_snr: payload[9] as i8,
        })
    }

    pub fn encode_payload(&self) -> [u8; Self::PAYLOAD_SIZE] {
        [
            self.uplink_rssi_ant1 as u8,
            self.uplink_rssi_ant2 as u8,
            self.uplink_lq,
            self.uplink_snr as u8,
            self.active_antenna,
            self.rf_mode,
            self.uplink_tx_power,
            self.downlink_rssi as u8,
            self.downlink_lq,
            self.downlink_snr as u8,
        ]
    }

    /// The uplink RSSI of the currently active antenna.
    pub fn active_uplink_rssi(&self) -> i8 {
        if self.active_antenna == 1 {
            self.uplink_rssi_ant2
        } else {
            self.uplink_rssi_ant1
        }
    }

    /// The uplink TX power in milliwatts, mapping the wire `uplink_tx_power`
    /// power-level INDEX through the CRSF link-statistics power table. Returns
    /// `None` for an index outside the defined table — an honest "unknown",
    /// never a fabricated figure. The table (index → mW): 0→0, 1→10, 2→25,
    /// 3→100, 4→500, 5→1000, 6→2000, 7→250, 8→50.
    pub fn uplink_tx_power_mw(&self) -> Option<u16> {
        Some(match self.uplink_tx_power {
            0 => 0,
            1 => 10,
            2 => 25,
            3 => 100,
            4 => 500,
            5 => 1000,
            6 => 2000,
            7 => 250,
            8 => 50,
            _ => return None,
        })
    }
}

/// GPS telemetry (type 0x02, 15-byte payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gps {
    /// Latitude, degrees × 1e7.
    pub lat_1e7: i32,
    /// Longitude, degrees × 1e7.
    pub lon_1e7: i32,
    /// Ground speed, raw wire units.
    pub ground_speed: u16,
    /// Heading, 0.01 degree units.
    pub heading: u16,
    /// Altitude, meters with a +1000 offset (0 = −1000 m).
    pub altitude: u16,
    /// Satellite count.
    pub satellites: u8,
}

impl Gps {
    pub const PAYLOAD_SIZE: usize = 15;

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        if payload.len() != Self::PAYLOAD_SIZE {
            return Err(FrameError::PayloadSizeMismatch {
                frame_type: TYPE_GPS,
                expected: Self::PAYLOAD_SIZE,
                got: payload.len(),
            });
        }
        Ok(Self {
            lat_1e7: i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
            lon_1e7: i32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]),
            ground_speed: u16::from_be_bytes([payload[8], payload[9]]),
            heading: u16::from_be_bytes([payload[10], payload[11]]),
            altitude: u16::from_be_bytes([payload[12], payload[13]]),
            satellites: payload[14],
        })
    }

    pub fn encode_payload(&self) -> [u8; Self::PAYLOAD_SIZE] {
        let mut out = [0u8; Self::PAYLOAD_SIZE];
        out[0..4].copy_from_slice(&self.lat_1e7.to_be_bytes());
        out[4..8].copy_from_slice(&self.lon_1e7.to_be_bytes());
        out[8..10].copy_from_slice(&self.ground_speed.to_be_bytes());
        out[10..12].copy_from_slice(&self.heading.to_be_bytes());
        out[12..14].copy_from_slice(&self.altitude.to_be_bytes());
        out[14] = self.satellites;
        out
    }
}

/// Battery telemetry (type 0x08, 8-byte payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// Voltage, 0.1 V units.
    pub voltage: u16,
    /// Current, 0.1 A units.
    pub current: u16,
    /// Capacity used, mAh (24-bit on the wire; the top byte must be 0).
    pub capacity_used: u32,
    /// Remaining charge, percent.
    pub remaining: u8,
}

impl Battery {
    pub const PAYLOAD_SIZE: usize = 8;

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        if payload.len() != Self::PAYLOAD_SIZE {
            return Err(FrameError::PayloadSizeMismatch {
                frame_type: TYPE_BATTERY,
                expected: Self::PAYLOAD_SIZE,
                got: payload.len(),
            });
        }
        Ok(Self {
            voltage: u16::from_be_bytes([payload[0], payload[1]]),
            current: u16::from_be_bytes([payload[2], payload[3]]),
            capacity_used: u32::from_be_bytes([0, payload[4], payload[5], payload[6]]),
            remaining: payload[7],
        })
    }

    pub fn encode_payload(&self) -> [u8; Self::PAYLOAD_SIZE] {
        let cap = self.capacity_used.to_be_bytes();
        [
            self.voltage.to_be_bytes()[0],
            self.voltage.to_be_bytes()[1],
            self.current.to_be_bytes()[0],
            self.current.to_be_bytes()[1],
            cap[1],
            cap[2],
            cap[3],
            self.remaining,
        ]
    }
}

/// Attitude telemetry (type 0x1E, 6-byte payload). Raw wire integers; the
/// angle scale is bench-verified before a consumer renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attitude {
    pub pitch: i16,
    pub roll: i16,
    pub yaw: i16,
}

impl Attitude {
    pub const PAYLOAD_SIZE: usize = 6;

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        if payload.len() != Self::PAYLOAD_SIZE {
            return Err(FrameError::PayloadSizeMismatch {
                frame_type: TYPE_ATTITUDE,
                expected: Self::PAYLOAD_SIZE,
                got: payload.len(),
            });
        }
        Ok(Self {
            pitch: i16::from_be_bytes([payload[0], payload[1]]),
            roll: i16::from_be_bytes([payload[2], payload[3]]),
            yaw: i16::from_be_bytes([payload[4], payload[5]]),
        })
    }

    pub fn encode_payload(&self) -> [u8; Self::PAYLOAD_SIZE] {
        let mut out = [0u8; Self::PAYLOAD_SIZE];
        out[0..2].copy_from_slice(&self.pitch.to_be_bytes());
        out[2..4].copy_from_slice(&self.roll.to_be_bytes());
        out[4..6].copy_from_slice(&self.yaw.to_be_bytes());
        out
    }
}

/// Flight mode telemetry (type 0x21): a null-terminated ASCII string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlightMode(pub String);

impl FlightMode {
    /// Decode a null-terminated string payload. Bytes after the first NUL are
    /// ignored; a payload with no NUL is taken whole. Non-UTF-8 bytes are
    /// replaced, never fatal — a garbled mode string must not kill the parser.
    pub fn decode(payload: &[u8]) -> Self {
        let end = payload
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(payload.len());
        Self(String::from_utf8_lossy(&payload[..end]).into_owned())
    }

    /// Encode as a null-terminated payload.
    pub fn encode_payload(&self) -> Vec<u8> {
        let mut out = self.0.as_bytes().to_vec();
        out.push(0);
        out
    }
}

/// A typed view over a validated [`RawFrame`]'s telemetry payload. Unknown
/// types pass through untyped so pass-through consumers still see them.
#[derive(Debug, Clone, PartialEq)]
pub enum Telemetry {
    LinkStatistics(LinkStatistics),
    Gps(Gps),
    Battery(Battery),
    Attitude(Attitude),
    FlightMode(FlightMode),
}

/// Decode a raw frame into its typed telemetry, `None` for non-telemetry or
/// unknown frame types (the caller keeps the raw frame for pass-through).
pub fn decode_telemetry(frame: &RawFrame) -> Option<Result<Telemetry, FrameError>> {
    match frame.frame_type {
        TYPE_LINK_STATISTICS => {
            Some(LinkStatistics::decode(&frame.payload).map(Telemetry::LinkStatistics))
        }
        TYPE_GPS => Some(Gps::decode(&frame.payload).map(Telemetry::Gps)),
        TYPE_BATTERY => Some(Battery::decode(&frame.payload).map(Telemetry::Battery)),
        TYPE_ATTITUDE => Some(Attitude::decode(&frame.payload).map(Telemetry::Attitude)),
        TYPE_FLIGHT_MODE => Some(Ok(Telemetry::FlightMode(FlightMode::decode(
            &frame.payload,
        )))),
        _ => None,
    }
}

/// Build a complete telemetry wire frame with the given device address.
pub fn build_telemetry_frame(sync: u8, telemetry: &Telemetry) -> Result<Vec<u8>, FrameError> {
    match telemetry {
        Telemetry::LinkStatistics(v) => {
            build_frame(sync, TYPE_LINK_STATISTICS, &v.encode_payload())
        }
        Telemetry::Gps(v) => build_frame(sync, TYPE_GPS, &v.encode_payload()),
        Telemetry::Battery(v) => build_frame(sync, TYPE_BATTERY, &v.encode_payload()),
        Telemetry::Attitude(v) => build_frame(sync, TYPE_ATTITUDE, &v.encode_payload()),
        Telemetry::FlightMode(v) => build_frame(sync, TYPE_FLIGHT_MODE, &v.encode_payload()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{parse_frame, ADDR_FLIGHT_CONTROLLER};

    fn roundtrip(t: Telemetry) {
        let wire = build_telemetry_frame(ADDR_FLIGHT_CONTROLLER, &t).unwrap();
        let (frame, consumed) = parse_frame(&wire).unwrap();
        assert_eq!(consumed, wire.len());
        let decoded = decode_telemetry(&frame).unwrap().unwrap();
        assert_eq!(decoded, t);
    }

    #[test]
    fn link_statistics_roundtrip_and_size() {
        let stats = LinkStatistics {
            uplink_rssi_ant1: -51,
            uplink_rssi_ant2: -60,
            uplink_lq: 99,
            uplink_snr: 8,
            active_antenna: 0,
            rf_mode: 4,
            uplink_tx_power: 20,
            downlink_rssi: -55,
            downlink_lq: 97,
            downlink_snr: 6,
        };
        assert_eq!(stats.encode_payload().len(), 10);
        roundtrip(Telemetry::LinkStatistics(stats));
    }

    #[test]
    fn link_statistics_field_positions_match_the_layout() {
        // Byte positions follow the documented field order.
        let payload = [
            0xCD, // -51 as i8: uplink RSSI ant1
            0xC4, // -60: uplink RSSI ant2
            99,   // uplink LQ
            0x08, // uplink SNR
            1,    // active antenna
            4,    // rf mode
            20,   // uplink tx power
            0xC9, // -55: downlink RSSI
            97,   // downlink LQ
            0xFA, // -6: downlink SNR
        ];
        let s = LinkStatistics::decode(&payload).unwrap();
        assert_eq!(s.uplink_rssi_ant1, -51);
        assert_eq!(s.uplink_rssi_ant2, -60);
        assert_eq!(s.uplink_lq, 99);
        assert_eq!(s.uplink_snr, 8);
        assert_eq!(s.active_antenna, 1);
        assert_eq!(s.rf_mode, 4);
        assert_eq!(s.uplink_tx_power, 20);
        assert_eq!(s.downlink_rssi, -55);
        assert_eq!(s.downlink_lq, 97);
        assert_eq!(s.downlink_snr, -6);
        // Active antenna 1 selects the ant2 reading.
        assert_eq!(s.active_uplink_rssi(), -60);
    }

    #[test]
    fn uplink_tx_power_index_maps_to_the_crsf_power_table_milliwatts() {
        // The wire uplink_tx_power is a power-level INDEX; each entry maps to the
        // CRSF link-statistics power table (mW), and an out-of-table index is an
        // honest None, never a fabricated figure.
        let mut s = LinkStatistics {
            uplink_rssi_ant1: -51,
            uplink_rssi_ant2: -60,
            uplink_lq: 99,
            uplink_snr: 8,
            active_antenna: 0,
            rf_mode: 4,
            uplink_tx_power: 0,
            downlink_rssi: -55,
            downlink_lq: 97,
            downlink_snr: 6,
        };
        for (index, mw) in [
            (0u8, 0u16),
            (1, 10),
            (2, 25),
            (3, 100),
            (4, 500),
            (5, 1000),
            (6, 2000),
            (7, 250),
            (8, 50),
        ] {
            s.uplink_tx_power = index;
            assert_eq!(s.uplink_tx_power_mw(), Some(mw), "index {index}");
        }
        // Indices past the table are unknown, not a value.
        s.uplink_tx_power = 9;
        assert_eq!(s.uplink_tx_power_mw(), None);
        s.uplink_tx_power = 255;
        assert_eq!(s.uplink_tx_power_mw(), None);
    }

    #[test]
    fn gps_roundtrip_and_size() {
        let gps = Gps {
            lat_1e7: 129_752_610, // 12.975261 degrees
            lon_1e7: 775_909_780, // 77.590978 degrees
            ground_speed: 1234,
            heading: 27_015,
            altitude: 1980, // 980 m with the +1000 offset
            satellites: 14,
        };
        assert_eq!(gps.encode_payload().len(), 15);
        roundtrip(Telemetry::Gps(gps));
        // Negative coordinates survive too.
        roundtrip(Telemetry::Gps(Gps {
            lat_1e7: -337_722_000,
            lon_1e7: -703_500_000,
            ..gps
        }));
    }

    #[test]
    fn battery_roundtrip_and_24_bit_capacity() {
        let batt = Battery {
            voltage: 168,            // 16.8 V
            current: 254,            // 25.4 A
            capacity_used: 0xABCDEF, // full 24-bit range
            remaining: 73,
        };
        assert_eq!(batt.encode_payload().len(), 8);
        roundtrip(Telemetry::Battery(batt));
        // The 24-bit wire field truncates the top byte; the codec round-trips
        // only in-range values, so the masked value is what comes back.
        let over = Battery {
            capacity_used: 0x01_00_00_01,
            ..batt
        };
        let wire =
            build_telemetry_frame(ADDR_FLIGHT_CONTROLLER, &Telemetry::Battery(over)).unwrap();
        let (frame, _) = parse_frame(&wire).unwrap();
        match decode_telemetry(&frame).unwrap().unwrap() {
            Telemetry::Battery(b) => assert_eq!(b.capacity_used, 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn attitude_roundtrip_and_size() {
        let att = Attitude {
            pitch: -3141,
            roll: 1570,
            yaw: -31415,
        };
        assert_eq!(att.encode_payload().len(), 6);
        roundtrip(Telemetry::Attitude(att));
    }

    #[test]
    fn flight_mode_roundtrip_and_null_termination() {
        roundtrip(Telemetry::FlightMode(FlightMode("ACRO".into())));
        // Bytes after the NUL are ignored on decode.
        let decoded = FlightMode::decode(b"STAB\0garbage");
        assert_eq!(decoded.0, "STAB");
        // A payload with no NUL is taken whole.
        assert_eq!(FlightMode::decode(b"RTL").0, "RTL");
        // Non-UTF-8 bytes never panic.
        let lossy = FlightMode::decode(&[0xFF, 0xFE, 0x00]);
        assert!(!lossy.0.is_empty());
    }

    #[test]
    fn unknown_frame_types_are_not_telemetry() {
        let frame = RawFrame {
            sync: ADDR_FLIGHT_CONTROLLER,
            frame_type: 0x7F,
            payload: vec![1, 2, 3],
        };
        assert!(decode_telemetry(&frame).is_none());
    }

    #[test]
    fn wrong_size_payload_is_a_decode_error() {
        let frame = RawFrame {
            sync: ADDR_FLIGHT_CONTROLLER,
            frame_type: crate::frame::TYPE_GPS,
            payload: vec![0; 14],
        };
        assert!(decode_telemetry(&frame).unwrap().is_err());
    }
}
