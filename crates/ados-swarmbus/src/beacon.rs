//! The 20-byte swarm beacon: the only message a drone radiates on the swarm bus
//! in steady state.
//!
//! Sized deliberately between the three shipped cooperative-awareness formats it
//! is descended from — ADS-B (14 B), FLARM (~24 B) and ASTM Remote ID (25 B) —
//! because the airtime budget at N=24 is the whole reason the swarm fits on one
//! 20 MHz channel (see [`crate::AIRTIME_BUDGET`]). Every field earns its bytes:
//!
//! - **Position and velocity** use the MAVLink `GLOBAL_POSITION_INT` encodings
//!   (degrees × 1e7, decimetres, cm/s) verbatim, so filling a beacon from the
//!   flight controller's fused state is a shift, never a lossy re-quantisation.
//! - **Heading is NOT transmitted.** Every reference implementation derives it as
//!   `atan2(v_east, v_north)`; carrying it would cost 2 bytes to restate
//!   information the velocity already holds. [`SwarmBeacon::heading_deg`] is that
//!   derivation.
//! - **`seq_ms`** is the sender's uptime truncated to 16 bits. It is the
//!   dead-reckoning epoch and the staleness signal in one, and it deliberately
//!   does NOT carry wall-clock: two drones with unsynchronised clocks must still
//!   agree on which of two beacons is newer, and a 65.5 s wrap is far longer than
//!   [`crate::NEIGHBOR_STALE`].
//! - **`status`** packs five independent conditions plus the active mode
//!   precedence into one byte. Each bit is a separate condition, never blended:
//!   an armed drone with a bad GPS fix and an armed drone in emergency are
//!   different situations and the operator surface renders them as different
//!   glyphs.

use crate::ModePrecedence;

/// The beacon's exact on-air length. `size_of::<SwarmBeacon>()` is **24** —
/// `repr(C)` pads `slot`/`seq_ms` to a 4-byte boundary and tail-pads `status` —
/// so the wire is written and read field-by-field by
/// [`SwarmBeacon::encode`]/[`SwarmBeacon::decode`] and the struct is NEVER
/// transmuted. `repr(C)` is kept only to pin field order to the wire order, so
/// the two can be read side by side. A test asserts the 20.
pub const BEACON_WIRE_LEN: usize = 20;

/// `status` bit 0: the flight controller reports the vehicle armed.
pub const STATUS_ARMED: u8 = 1 << 0;
/// `status` bit 1: the vehicle is in a mode that accepts offboard setpoints
/// (GUIDED). The onboard autonomy layer only commands an FC that reports this.
pub const STATUS_GUIDED: u8 = 1 << 1;
/// `status` bit 2: the separation layer has taken over (a neighbour inside the
/// hard-separation radius). Raised on the next beacon after the override fires.
pub const STATUS_EMERGENCY: u8 = 1 << 2;
/// `status` bit 3: the GPS fix is usable (3D or better). A beacon with this bit
/// clear carries a position no other drone may separate against.
pub const STATUS_GPS_OK: u8 = 1 << 3;
/// `status` bit 4: this drone is the operator-selected hero and is streaming full
/// video. Exclusive fleet-wide. The UI renders THIS bit as the truth about hero
/// state rather than its own optimistic local guess, so a demotion that failed
/// over the air is visible instead of assumed.
pub const STATUS_HERO: u8 = 1 << 4;

/// `status` bits 5-7: the sender's currently-active mode-precedence level, as
/// [`ModePrecedence`]. Three bits hold all five levels with room to spare, so the
/// per-neighbour precedence truth the operator surface needs costs zero extra
/// bytes and zero extra airtime.
///
/// [`ModePrecedence::Hold`] is discriminant 0, so a node with no autonomy layer
/// running radiates `000` here and every reader decodes `hold` — the honest
/// answer for a drone that is not being flown by the swarm layer.
pub const STATUS_PRECEDENCE_MASK: u8 = 0b1110_0000;

/// How far to shift a [`ModePrecedence`] discriminant into `status`.
pub const STATUS_PRECEDENCE_SHIFT: u32 = 5;

/// One node's cooperative-awareness broadcast. Little-endian on the wire; see
/// [`BEACON_WIRE_LEN`] for why the struct is never transmuted.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SwarmBeacon {
    /// The sender's fleet slot, `1..=ados_radio::config::FLEET_MAX_SLOTS`. Slot 0
    /// is the ground station, which is not an aircraft and never beacons.
    pub slot: u8,
    /// Sender uptime in milliseconds, truncated to 16 bits. Wraps every 65.536 s.
    pub seq_ms: u16,
    /// Latitude, degrees × 1e7 (`GLOBAL_POSITION_INT.lat`).
    pub lat: i32,
    /// Longitude, degrees × 1e7 (`GLOBAL_POSITION_INT.lon`).
    pub lon: i32,
    /// Altitude relative to home, decimetres. ±3.2 km of range at 0.1 m
    /// resolution — an order of magnitude finer than any separation threshold.
    pub alt_dm: i16,
    /// North velocity, cm/s (NED).
    pub vx_cms: i16,
    /// East velocity, cm/s (NED).
    pub vy_cms: i16,
    /// Down velocity, cm/s (NED). Positive is descending.
    pub vz_cms: i16,
    /// Condition bits plus the mode-precedence field; see the `STATUS_*`
    /// constants.
    pub status: u8,
}

impl SwarmBeacon {
    /// Serialize to the exact [`BEACON_WIRE_LEN`] bytes, little-endian.
    pub fn encode(&self) -> [u8; BEACON_WIRE_LEN] {
        let mut out = [0u8; BEACON_WIRE_LEN];
        out[0] = self.slot;
        out[1..3].copy_from_slice(&self.seq_ms.to_le_bytes());
        out[3..7].copy_from_slice(&self.lat.to_le_bytes());
        out[7..11].copy_from_slice(&self.lon.to_le_bytes());
        out[11..13].copy_from_slice(&self.alt_dm.to_le_bytes());
        out[13..15].copy_from_slice(&self.vx_cms.to_le_bytes());
        out[15..17].copy_from_slice(&self.vy_cms.to_le_bytes());
        out[17..19].copy_from_slice(&self.vz_cms.to_le_bytes());
        out[19] = self.status;
        out
    }

    /// Parse a beacon body. Returns `None` for anything that is not exactly
    /// [`BEACON_WIRE_LEN`] bytes: the length is fixed, so a short or long body is
    /// a different message and must never be read as a partially-valid position.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() != BEACON_WIRE_LEN {
            return None;
        }
        Some(Self {
            slot: buf[0],
            seq_ms: u16::from_le_bytes([buf[1], buf[2]]),
            lat: i32::from_le_bytes([buf[3], buf[4], buf[5], buf[6]]),
            lon: i32::from_le_bytes([buf[7], buf[8], buf[9], buf[10]]),
            alt_dm: i16::from_le_bytes([buf[11], buf[12]]),
            vx_cms: i16::from_le_bytes([buf[13], buf[14]]),
            vy_cms: i16::from_le_bytes([buf[15], buf[16]]),
            vz_cms: i16::from_le_bytes([buf[17], buf[18]]),
            status: buf[19],
        })
    }

    /// Latitude in degrees.
    pub fn lat_deg(&self) -> f64 {
        self.lat as f64 / 1e7
    }

    /// Longitude in degrees.
    pub fn lon_deg(&self) -> f64 {
        self.lon as f64 / 1e7
    }

    /// Home-relative altitude in metres.
    pub fn alt_m(&self) -> f64 {
        self.alt_dm as f64 / 10.0
    }

    /// North velocity in m/s.
    pub fn vx_ms(&self) -> f64 {
        self.vx_cms as f64 / 100.0
    }

    /// East velocity in m/s.
    pub fn vy_ms(&self) -> f64 {
        self.vy_cms as f64 / 100.0
    }

    /// Down velocity in m/s (positive descending).
    pub fn vz_ms(&self) -> f64 {
        self.vz_cms as f64 / 100.0
    }

    /// Course over ground in degrees clockwise from north, `[0, 360)`, derived
    /// as `atan2(east, north)` rather than transmitted.
    ///
    /// A stationary vehicle has no course, and `atan2(0, 0)` is defined as 0, so
    /// a hovering drone reads 0° (due north) — the same convention every ADS-B
    /// and FLARM consumer applies. A consumer that must distinguish "pointing
    /// north" from "not moving" checks the velocity magnitude, not the heading.
    pub fn heading_deg(&self) -> f64 {
        let deg = self.vy_ms().atan2(self.vx_ms()).to_degrees();
        if deg < 0.0 {
            deg + 360.0
        } else {
            deg
        }
    }

    /// The vehicle reports armed ([`STATUS_ARMED`]).
    pub fn armed(&self) -> bool {
        self.status & STATUS_ARMED != 0
    }

    /// The vehicle accepts offboard setpoints ([`STATUS_GUIDED`]).
    pub fn guided(&self) -> bool {
        self.status & STATUS_GUIDED != 0
    }

    /// The separation layer has taken over ([`STATUS_EMERGENCY`]).
    pub fn emergency(&self) -> bool {
        self.status & STATUS_EMERGENCY != 0
    }

    /// The GPS fix is usable ([`STATUS_GPS_OK`]).
    pub fn gps_ok(&self) -> bool {
        self.status & STATUS_GPS_OK != 0
    }

    /// This drone is the operator-selected hero ([`STATUS_HERO`]).
    pub fn hero(&self) -> bool {
        self.status & STATUS_HERO != 0
    }

    /// The sender's active mode-precedence level, decoded from `status` bits 5-7.
    pub fn precedence(&self) -> ModePrecedence {
        ModePrecedence::from_status_bits(self.status)
    }

    /// Overwrite the mode-precedence field, leaving the five condition bits
    /// untouched. The onboard autonomy layer calls this each control tick with
    /// the level that actually governed the vehicle, not the commanded one.
    pub fn set_precedence(&mut self, level: ModePrecedence) {
        self.status = (self.status & !STATUS_PRECEDENCE_MASK) | level.as_status_bits();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A beacon with every field distinct, so a swapped or overlapping offset in
    /// `encode`/`decode` cannot round-trip. Deliberately uses negative values in
    /// every signed field: a sign-extension bug (reading an `i16` as `u16`) is
    /// the most plausible codec fault and only a negative value catches it.
    fn sample() -> SwarmBeacon {
        SwarmBeacon {
            slot: 7,
            seq_ms: 0xBEEF,
            lat: 129_716_000,
            lon: -775_946_000,
            alt_dm: -325,
            vx_cms: 120,
            vy_cms: -40,
            vz_cms: -7,
            status: STATUS_ARMED | STATUS_GUIDED | STATUS_GPS_OK,
        }
    }

    #[test]
    fn the_wire_is_exactly_twenty_bytes() {
        assert_eq!(BEACON_WIRE_LEN, 20);
        assert_eq!(sample().encode().len(), 20);
    }

    #[test]
    fn encode_decode_round_trips_byte_exactly() {
        let b = sample();
        let wire = b.encode();
        assert_eq!(SwarmBeacon::decode(&wire), Some(b));
    }

    /// Pins the byte layout itself, not just the round-trip. A round-trip test
    /// passes even if encode and decode agree on a WRONG layout; this fails the
    /// moment a field moves, changes width, or flips endianness — which would
    /// silently desync two agent versions in one fleet.
    #[test]
    fn the_byte_layout_is_pinned_little_endian() {
        let wire = sample().encode();
        assert_eq!(wire[0], 7, "slot at offset 0");
        assert_eq!(&wire[1..3], &[0xEF, 0xBE], "seq_ms LE at 1..3");
        assert_eq!(&wire[3..7], &129_716_000i32.to_le_bytes(), "lat LE at 3..7");
        assert_eq!(
            &wire[7..11],
            &(-775_946_000i32).to_le_bytes(),
            "lon LE at 7..11"
        );
        assert_eq!(
            &wire[11..13],
            &(-325i16).to_le_bytes(),
            "alt_dm LE at 11..13"
        );
        assert_eq!(&wire[13..15], &120i16.to_le_bytes(), "vx_cms LE at 13..15");
        assert_eq!(
            &wire[15..17],
            &(-40i16).to_le_bytes(),
            "vy_cms LE at 15..17"
        );
        assert_eq!(&wire[17..19], &(-7i16).to_le_bytes(), "vz_cms LE at 17..19");
        assert_eq!(wire[19], STATUS_ARMED | STATUS_GUIDED | STATUS_GPS_OK);
    }

    #[test]
    fn a_short_or_long_body_is_rejected_rather_than_partially_read() {
        let wire = sample().encode();
        assert_eq!(SwarmBeacon::decode(&wire[..19]), None);
        let mut long = wire.to_vec();
        long.push(0);
        assert_eq!(SwarmBeacon::decode(&long), None);
        assert_eq!(SwarmBeacon::decode(&[]), None);
    }

    #[test]
    fn scaled_accessors_undo_the_mavlink_encodings() {
        let b = sample();
        assert!((b.lat_deg() - 12.9716).abs() < 1e-9);
        assert!((b.lon_deg() - -77.5946).abs() < 1e-9);
        assert!((b.alt_m() - -32.5).abs() < 1e-9);
        assert!((b.vx_ms() - 1.20).abs() < 1e-9);
        assert!((b.vy_ms() - -0.40).abs() < 1e-9);
        assert!((b.vz_ms() - -0.07).abs() < 1e-9);
    }

    /// Heading is derived, so a wrong argument order (`atan2(vx, vy)`) or a
    /// missing wrap would silently mirror every drone's icon on the map. Each
    /// case pins a cardinal or diagonal the two bugs disagree on.
    #[test]
    fn heading_is_atan2_east_north_wrapped_to_a_full_circle() {
        let with = |vx: i16, vy: i16| {
            SwarmBeacon {
                vx_cms: vx,
                vy_cms: vy,
                ..SwarmBeacon::default()
            }
            .heading_deg()
        };
        assert!((with(100, 0) - 0.0).abs() < 1e-6, "due north");
        assert!((with(0, 100) - 90.0).abs() < 1e-6, "due east");
        assert!((with(-100, 0) - 180.0).abs() < 1e-6, "due south");
        // The wrap: atan2 returns -90 here, which must present as 270.
        assert!((with(0, -100) - 270.0).abs() < 1e-6, "due west");
        assert!((with(100, 100) - 45.0).abs() < 1e-6, "north-east");
        assert!((with(100, -100) - 315.0).abs() < 1e-6, "north-west");
        // Every derived heading is inside the half-open circle.
        for (vx, vy) in [(0, 0), (1, -1), (-1, -1), (-1, 1), (32767, -32768)] {
            let h = with(vx, vy);
            assert!((0.0..360.0).contains(&h), "heading {h} out of [0,360)");
        }
    }

    #[test]
    fn each_status_bit_reads_independently() {
        let flag = |bit: u8| SwarmBeacon {
            status: bit,
            ..SwarmBeacon::default()
        };
        assert!(flag(STATUS_ARMED).armed());
        assert!(flag(STATUS_GUIDED).guided());
        assert!(flag(STATUS_EMERGENCY).emergency());
        assert!(flag(STATUS_GPS_OK).gps_ok());
        assert!(flag(STATUS_HERO).hero());
        // Every bit set in isolation leaves the other four clear — no blending.
        let armed = flag(STATUS_ARMED);
        assert!(!armed.guided() && !armed.emergency() && !armed.gps_ok() && !armed.hero());
        let hero = flag(STATUS_HERO);
        assert!(!hero.armed() && !hero.guided() && !hero.emergency() && !hero.gps_ok());
    }

    /// The five condition bits and the three precedence bits must not overlap: an
    /// overlap would make setting a precedence level silently arm a drone.
    #[test]
    fn condition_bits_and_the_precedence_field_are_disjoint() {
        let conditions =
            STATUS_ARMED | STATUS_GUIDED | STATUS_EMERGENCY | STATUS_GPS_OK | STATUS_HERO;
        assert_eq!(conditions & STATUS_PRECEDENCE_MASK, 0);
        assert_eq!(conditions | STATUS_PRECEDENCE_MASK, 0xFF, "no wasted bit");
        assert_eq!(STATUS_PRECEDENCE_MASK >> STATUS_PRECEDENCE_SHIFT, 0b111);
    }

    /// A node with no autonomy layer running leaves bits 5-7 at zero and every
    /// reader decodes `hold` — the pre-Phase-5 steady state.
    #[test]
    fn a_beacon_with_no_precedence_source_reads_hold_with_the_field_zeroed() {
        let b = SwarmBeacon {
            status: STATUS_ARMED | STATUS_GPS_OK,
            ..SwarmBeacon::default()
        };
        assert_eq!(b.status & STATUS_PRECEDENCE_MASK, 0);
        assert_eq!(b.precedence(), ModePrecedence::Hold);
        assert_eq!(b.precedence().as_wire(), "hold");
    }

    #[test]
    fn set_precedence_replaces_the_field_and_preserves_the_conditions() {
        let mut b = SwarmBeacon {
            status: STATUS_ARMED | STATUS_HERO,
            ..SwarmBeacon::default()
        };
        b.set_precedence(ModePrecedence::Formation);
        assert_eq!(b.precedence(), ModePrecedence::Formation);
        assert!(
            b.armed() && b.hero(),
            "conditions survive a precedence write"
        );
        // Overwriting is idempotent in the condition bits and total in the field.
        b.set_precedence(ModePrecedence::HardSeparation);
        assert_eq!(b.precedence(), ModePrecedence::HardSeparation);
        assert!(b.armed() && b.hero());
        // And it survives the wire.
        let wire = b.encode();
        assert_eq!(
            SwarmBeacon::decode(&wire).unwrap().precedence(),
            ModePrecedence::HardSeparation
        );
    }
}
