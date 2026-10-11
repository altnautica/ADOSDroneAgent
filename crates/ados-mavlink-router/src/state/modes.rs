//! Flight-mode naming and the firmware family.

use super::*;

/// PX4 `(main_mode, sub_mode)` -> mode name. PX4 packs the mode into
/// `custom_mode` differently from ArduPilot (a `(main << 16) | (sub << 24)`
/// union, mavros `px4_custom_mode.h`), so it needs its own decode path keyed on
/// `HEARTBEAT.autopilot == MAV_AUTOPILOT_PX4`, not on MAV_TYPE. `sub` is 0 for
/// the manual/assisted modes and non-zero only under AUTO.
pub(super) const PX4_MODES: &[(u8, u8, &str)] = &[
    (1, 0, "MANUAL"),
    (2, 0, "ALTCTL"),
    (3, 0, "POSCTL"),
    (5, 0, "ACRO"),
    (6, 0, "OFFBOARD"),
    (7, 0, "STABILIZED"),
    (8, 0, "RATTITUDE"),
    (4, 1, "AUTO.READY"),
    (4, 2, "AUTO.TAKEOFF"),
    (4, 3, "AUTO.LOITER"),
    (4, 4, "AUTO.MISSION"),
    (4, 5, "AUTO.RTL"),
    (4, 6, "AUTO.LAND"),
    (4, 7, "AUTO.RTGS"),
    (4, 8, "AUTO.FOLLOW_TARGET"),
    (4, 9, "AUTO.PRECLAND"),
];

/// Decode a PX4 `custom_mode` (main in bits 16-23, sub in bits 24-31). Falls
/// back to the main-mode name when the sub-mode is unmapped, then to `MODE_<n>`.
pub(super) fn px4_mode_name(custom_mode: u32) -> String {
    let main = ((custom_mode >> 16) & 0xff) as u8;
    let sub = ((custom_mode >> 24) & 0xff) as u8;
    if let Some((_, _, name)) = PX4_MODES.iter().find(|(m, s, _)| *m == main && *s == sub) {
        return (*name).to_string();
    }
    // Unmapped sub under a known main: report the main mode.
    if let Some((_, _, name)) = PX4_MODES.iter().find(|(m, s, _)| *m == main && *s == 0) {
        return (*name).to_string();
    }
    format!("MODE_{custom_mode}")
}

/// Select the mode table for a vehicle, then resolve the custom mode. PX4 is
/// decoded by its packed-union scheme (keyed on `autopilot`); ArduPilot through
/// the shared per-firmware tables, the same ones the control surface encodes
/// `DO_SET_MODE` with: the firmware the banner named, or until the banner
/// arrives the one the MAV_TYPE suggests. The suggestion is display-only: the
/// command route never encodes from it (a copter-typed heartbeat may be a
/// QuadPlane), and the router requests the banner until it has one. An
/// unmapped type/mode reads `MODE_<n>`.
pub(super) fn mode_name(
    autopilot: i64,
    firmware: Option<ArduPilotFirmware>,
    mav_type: i64,
    custom_mode: u32,
) -> String {
    if autopilot == MAV_AUTOPILOT_PX4 {
        return px4_mode_name(custom_mode);
    }
    firmware
        .or_else(|| ArduPilotFirmware::from_mav_type(mav_type))
        .and_then(|fw| fw.mode_name(custom_mode))
        .map_or_else(|| format!("MODE_{custom_mode}"), str::to_string)
}

/// The canonical FC firmware family, derived only from the verified signals the
/// router already holds — never guessed. An MSP FC is named by its
/// USB-descriptor variant (`betaflight` / `inav`); a MAVLink FC is named, only
/// while its heartbeat is live, by the decoded `HEARTBEAT.autopilot`
/// discriminator (3 = ArduPilot, 12 = PX4). Anything without a verified
/// identity — an MSP board with an unrecognised descriptor, a MAVLink FC whose
/// link is not alive (so the last autopilot code is stale), or an unrecognised
/// autopilot code — reports `unknown`. No FC payload is parsed: the inputs are
/// the same passive signals the byte-pipe already captures.
pub fn firmware_family(
    msp_variant: Option<&str>,
    mavlink_alive: bool,
    autopilot: i64,
) -> &'static str {
    match msp_variant {
        Some("betaflight") => "betaflight",
        Some("inav") => "inav",
        // An MSP board whose USB descriptor names no known family.
        Some(_) => "unknown",
        // A MAVLink FC: trust the autopilot code only while the link is live, so
        // a stale heartbeat after link loss never keeps claiming a family.
        None if mavlink_alive => match autopilot {
            3 => "ardupilot",
            12 => "px4",
            _ => "unknown",
        },
        None => "unknown",
    }
}
