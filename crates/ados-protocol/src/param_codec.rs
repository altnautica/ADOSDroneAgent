//! The value encoding of MAVLink `PARAM_SET` and `PARAM_VALUE`.
//!
//! Both messages carry the value in one 4-byte float field. ArduPilot casts
//! every value to float, integer parameters included. PX4 instead copies an
//! integer parameter's bytes into the field (bytewise encoding), so the float
//! it sends for the INT32 value `5` is the bit pattern of the integer 5, not
//! `5.0`. Reading or writing a PX4 integer parameter as a float corrupts it:
//! `5` written as a float lands as `1084227584`. The router's decode and the
//! control surface's encode share this one codec so the two never disagree.

use crate::flight_modes::MAV_AUTOPILOT_PX4;

/// The `MAV_PARAM_TYPE` values the codec distinguishes.
pub const MAV_PARAM_TYPE_UINT8: u8 = 1;
pub const MAV_PARAM_TYPE_INT8: u8 = 2;
pub const MAV_PARAM_TYPE_UINT16: u8 = 3;
pub const MAV_PARAM_TYPE_INT16: u8 = 4;
pub const MAV_PARAM_TYPE_UINT32: u8 = 5;
pub const MAV_PARAM_TYPE_INT32: u8 = 6;
pub const MAV_PARAM_TYPE_UINT64: u8 = 7;
pub const MAV_PARAM_TYPE_INT64: u8 = 8;
pub const MAV_PARAM_TYPE_REAL32: u8 = 9;
pub const MAV_PARAM_TYPE_REAL64: u8 = 10;

/// Whether a vehicle whose HEARTBEAT names `autopilot` packs integer
/// parameters bytewise. PX4 does; ArduPilot casts.
#[must_use]
pub fn uses_bytewise(autopilot: i64) -> bool {
    autopilot == MAV_AUTOPILOT_PX4
}

/// Whether `param_type` is a defined `MAV_PARAM_TYPE` (UINT8 through REAL64).
#[must_use]
pub fn is_known_type(param_type: u8) -> bool {
    (MAV_PARAM_TYPE_UINT8..=MAV_PARAM_TYPE_REAL64).contains(&param_type)
}

/// The inclusive range an integer `param_type` can hold, or `None` for a float
/// type. The 64-bit types are bounded at 2^53, the largest integer the request's
/// JSON number carries exactly.
fn integer_range(param_type: u8) -> Option<(f64, f64)> {
    const EXACT: f64 = 9_007_199_254_740_992.0;
    Some(match param_type {
        MAV_PARAM_TYPE_UINT8 => (0.0, f64::from(u8::MAX)),
        MAV_PARAM_TYPE_INT8 => (f64::from(i8::MIN), f64::from(i8::MAX)),
        MAV_PARAM_TYPE_UINT16 => (0.0, f64::from(u16::MAX)),
        MAV_PARAM_TYPE_INT16 => (f64::from(i16::MIN), f64::from(i16::MAX)),
        MAV_PARAM_TYPE_UINT32 => (0.0, f64::from(u32::MAX)),
        MAV_PARAM_TYPE_INT32 => (f64::from(i32::MIN), f64::from(i32::MAX)),
        MAV_PARAM_TYPE_UINT64 => (0.0, EXACT),
        MAV_PARAM_TYPE_INT64 => (-EXACT, EXACT),
        _ => return None,
    })
}

/// The value a `PARAM_VALUE` reported, decoded from its wire field. With
/// `bytewise`, UINT8 through INT32 are read from the field's low bytes as that
/// integer type; every other case is the float itself.
#[must_use]
pub fn decode(raw: f32, param_type: u8, bytewise: bool) -> f64 {
    if bytewise {
        let b = raw.to_le_bytes();
        match param_type {
            MAV_PARAM_TYPE_UINT8 => return f64::from(b[0]),
            MAV_PARAM_TYPE_INT8 => return f64::from(i8::from_le_bytes([b[0]])),
            MAV_PARAM_TYPE_UINT16 => return f64::from(u16::from_le_bytes([b[0], b[1]])),
            MAV_PARAM_TYPE_INT16 => return f64::from(i16::from_le_bytes([b[0], b[1]])),
            MAV_PARAM_TYPE_UINT32 => return f64::from(u32::from_le_bytes(b)),
            MAV_PARAM_TYPE_INT32 => return f64::from(i32::from_le_bytes(b)),
            _ => {}
        }
    }
    f64::from(raw)
}

/// A value ready for a `PARAM_SET`: the float field to send, and the value the
/// vehicle will hold once it applies it (what its `PARAM_VALUE` echo decodes to).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Encoded {
    pub wire: f32,
    pub stored: f64,
}

/// Encode `value` for a parameter of `param_type`. An integer type takes the
/// rounded value and refuses one outside the type's range; with `bytewise`,
/// UINT8 through INT32 are written into the field's low bytes (the rest zero).
/// A float type refuses a value that overflows a 32-bit float.
pub fn encode(value: f64, param_type: u8, bytewise: bool) -> Result<Encoded, String> {
    if !value.is_finite() {
        return Err("value must be a finite number".to_string());
    }
    if let Some((min, max)) = integer_range(param_type) {
        let n = value.round();
        if n < min || n > max {
            return Err(format!(
                "value {value} is outside the range of this parameter's type ({min} to {max})"
            ));
        }
        if bytewise && param_type <= MAV_PARAM_TYPE_INT32 {
            let mut b = [0u8; 4];
            match param_type {
                MAV_PARAM_TYPE_UINT8 => b[0] = n as u8,
                MAV_PARAM_TYPE_INT8 => b[0] = (n as i8).to_le_bytes()[0],
                MAV_PARAM_TYPE_UINT16 => b[..2].copy_from_slice(&(n as u16).to_le_bytes()),
                MAV_PARAM_TYPE_INT16 => b[..2].copy_from_slice(&(n as i16).to_le_bytes()),
                MAV_PARAM_TYPE_UINT32 => b = (n as u32).to_le_bytes(),
                _ => b = (n as i32).to_le_bytes(),
            }
            return Ok(Encoded {
                wire: f32::from_le_bytes(b),
                stored: n,
            });
        }
        // A casting firmware stores the float it was sent, converted back to
        // an integer, so the echo is that float.
        let wire = n as f32;
        return Ok(Encoded {
            wire,
            stored: f64::from(wire),
        });
    }
    let wire = value as f32;
    if !wire.is_finite() {
        return Err(format!("value {value} does not fit a 32-bit float"));
    }
    Ok(Encoded {
        wire,
        stored: f64::from(wire),
    })
}

/// Whether a cached echo matches the value a write expects the vehicle to hold.
/// The echo travelled as a 32-bit float, so equality is judged at that
/// precision rather than at f64's.
#[must_use]
pub fn echo_matches(cached: f64, stored: f64) -> bool {
    (cached - stored).abs() <= stored.abs().max(1.0) * f64::from(f32::EPSILON)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_px4_integer_travels_as_its_bytes_not_as_a_float() {
        let e = encode(5.0, MAV_PARAM_TYPE_INT32, true).unwrap();
        assert_eq!(e.wire.to_le_bytes(), 5i32.to_le_bytes());
        assert_eq!(e.stored, 5.0);
        assert_eq!(decode(e.wire, MAV_PARAM_TYPE_INT32, true), 5.0);
        // The float reading of those bytes is the corruption the codec prevents.
        assert_ne!(f64::from(e.wire), 5.0);
    }

    #[test]
    fn narrow_and_signed_px4_integers_fill_only_their_low_bytes() {
        let e = encode(-1.0, MAV_PARAM_TYPE_INT8, true).unwrap();
        assert_eq!(e.wire.to_le_bytes(), [0xFF, 0, 0, 0]);
        assert_eq!(decode(e.wire, MAV_PARAM_TYPE_INT8, true), -1.0);
        let e = encode(-300.0, MAV_PARAM_TYPE_INT16, true).unwrap();
        assert_eq!(&e.wire.to_le_bytes()[..2], &(-300i16).to_le_bytes());
        assert_eq!(&e.wire.to_le_bytes()[2..], &[0, 0]);
        assert_eq!(decode(e.wire, MAV_PARAM_TYPE_INT16, true), -300.0);
        let e = encode(4_000_000_000.0, MAV_PARAM_TYPE_UINT32, true).unwrap();
        assert_eq!(decode(e.wire, MAV_PARAM_TYPE_UINT32, true), 4_000_000_000.0);
    }

    #[test]
    fn an_ardupilot_integer_is_the_float_of_the_rounded_value() {
        let e = encode(4.6, MAV_PARAM_TYPE_INT32, false).unwrap();
        assert_eq!(e.wire, 5.0);
        assert_eq!(e.stored, 5.0);
        assert_eq!(decode(e.wire, MAV_PARAM_TYPE_INT32, false), 5.0);
    }

    #[test]
    fn a_float_param_is_the_same_on_either_firmware() {
        for bytewise in [false, true] {
            let e = encode(0.135, MAV_PARAM_TYPE_REAL32, bytewise).unwrap();
            assert_eq!(e.wire, 0.135f32);
            assert_eq!(
                decode(e.wire, MAV_PARAM_TYPE_REAL32, bytewise),
                f64::from(0.135f32)
            );
        }
    }

    #[test]
    fn a_value_outside_the_type_is_refused_not_wrapped() {
        assert!(encode(256.0, MAV_PARAM_TYPE_UINT8, true).is_err());
        assert!(encode(-1.0, MAV_PARAM_TYPE_UINT8, false).is_err());
        assert!(encode(128.0, MAV_PARAM_TYPE_INT8, true).is_err());
        assert!(encode(2_147_483_648.0, MAV_PARAM_TYPE_INT32, true).is_err());
        assert!(encode(1e39, MAV_PARAM_TYPE_REAL32, false).is_err());
        assert!(encode(f64::NAN, MAV_PARAM_TYPE_REAL32, false).is_err());
        // The range edges themselves are writable.
        assert!(encode(255.0, MAV_PARAM_TYPE_UINT8, true).is_ok());
        assert!(encode(-128.0, MAV_PARAM_TYPE_INT8, true).is_ok());
    }

    #[test]
    fn a_landed_float_write_matches_its_echo_at_f32_precision() {
        // 100.1 is 100.09999847 as an f32: an f64 comparison at 1e-6 calls that
        // a miss although the vehicle holds exactly what was sent.
        let e = encode(100.1, MAV_PARAM_TYPE_REAL32, false).unwrap();
        assert!(echo_matches(f64::from(100.1f32), e.stored));
        assert!(!echo_matches(100.2, e.stored));
        assert!(echo_matches(
            0.0,
            encode(0.0, MAV_PARAM_TYPE_REAL32, false).unwrap().stored
        ));
        assert!(!echo_matches(1.0, 2.0));
    }

    #[test]
    fn only_px4_packs_bytewise() {
        assert!(uses_bytewise(12));
        assert!(!uses_bytewise(3));
        assert!(!uses_bytewise(0));
        assert!(is_known_type(6) && is_known_type(9) && !is_known_type(0) && !is_known_type(11));
    }
}
