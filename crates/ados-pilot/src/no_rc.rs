//! Flight with no RC transmitter (`pilot.rc_receiver: absent`): the flight
//! controller parameter posture and the rules for declaring it.
//!
//! With no receiver, ArduCopter's RC failsafe has nothing to watch and blocks
//! arming: with `FS_THR_ENABLE` above 0 a board that never saw a receiver
//! fails the pre-arm check "RC not found", and one whose RC was lost stays in
//! radio failsafe (`AP_Arming_Copter.cpp`, `radio.cpp`). So the declared
//! posture turns it off and, because no independent path to the vehicle
//! remains, adds the FC's own fence as the backstop that holds when the
//! companion and the link are gone, the companion is wrong, or the FC flies a
//! mode of its own. The fence sits 20 m outside the pilot's distance cap and
//! 10 m above its height cap ([`crate::config::NO_RC_MAX_DIST_M`],
//! [`crate::config::NO_RC_MAX_ALT_M`]), so it acts only when the pilot does
//! not. Its avoidance stays off (`AVOID_ENABLE=0` in the profile): it never
//! fights the pilot's setpoints.

use ados_protocol::pilot::{ControlMode, PlaceClass, RcReceiver, VehicleClass};

use crate::config::{NO_RC_MAX_ALT_M, NO_RC_MAX_DIST_M};
use crate::fc_profile::{fc_profile, p, Firmware, ProfileParam};

/// The FC fence's circle radius from home with no RC transmitter, metres:
/// the pilot's distance cap plus 20 m, which holds the pilot's stopping
/// distance at the cap (a 6 m/s follow at 2.5 m/s² stops in about 7 m),
/// GNSS drift and wind. ArduPilot's range is 30 to 10000.
pub const FENCE_RADIUS_M: f32 = NO_RC_MAX_DIST_M + 20.0;
/// The FC fence's ceiling above home with no RC transmitter, metres: the
/// pilot's height cap plus 10 m (`FENCE_ALT_MAX_TP` 1, above home, is the
/// firmware default).
pub const FENCE_ALT_MAX_M: f32 = NO_RC_MAX_ALT_M + 10.0;

/// `FS_THR_ENABLE`: the throttle (RC) failsafe, 0 = off.
const FS_THR: &str = "FS_THR_ENABLE";

/// ArduCopter's fence rows for flight with no RC transmitter (parameters
/// and values from `libraries/AC_Fence/AC_Fence.cpp`, ArduPilot fe92274).
const COPTER_FENCE: &[ProfileParam] = &[
    p("FENCE_ENABLE", &[], 1.0)
        .absent_blocks("No fence in this firmware: no backstop with no RC transmitter"),
    // Bit 0 maximum altitude, bit 1 circle centred on home.
    p("FENCE_TYPE", &[], 3.0),
    p("FENCE_RADIUS", &[], FENCE_RADIUS_M),
    p("FENCE_ALT_MAX", &[], FENCE_ALT_MAX_M),
    // 5: SmartRTL or Land, back along the flown path like every FC-side
    // return in the profile.
    p("FENCE_ACTION", &[], 5.0),
];

/// The FC profile for `fw`, `class` and the RC declaration: with `absent`,
/// the RC failsafe off and the fence rows added (ArduCopter only; the
/// declaration is refused elsewhere, see [`declare_refusal`]).
pub fn profile(
    fw: Firmware,
    class: VehicleClass,
    rc: RcReceiver,
) -> impl Iterator<Item = ProfileParam> {
    let copter = fw == Firmware::Ardupilot && class == VehicleClass::Multirotor;
    let no_rc = copter && !rc.is_present();
    let fence: &[ProfileParam] = if no_rc { COPTER_FENCE } else { &[] };
    fc_profile(fw, class)
        .iter()
        .map(move |r| {
            if no_rc && r.name == FS_THR {
                ProfileParam { value: 0.0, ..*r }
            } else {
                *r
            }
        })
        .chain(fence.iter().copied())
}

/// The profile rows the RC declaration decides: the RC failsafe, and with
/// `absent` the fence. The setup step that takes the declaration writes
/// them, so the profile written before it settles under the new one.
pub fn declaration_rows(
    fw: Firmware,
    class: VehicleClass,
    rc: RcReceiver,
) -> impl Iterator<Item = ProfileParam> {
    profile(fw, class, rc).filter(|r| r.name == FS_THR || r.name.starts_with("FENCE_"))
}

/// Every parameter name the profile may read, under either declaration.
pub fn all_names(fw: Firmware, class: VehicleClass) -> impl Iterator<Item = &'static str> {
    let fence = (fw == Firmware::Ardupilot && class == VehicleClass::Multirotor)
        .then_some(COPTER_FENCE)
        .unwrap_or(&[]);
    crate::fc_profile::all_names(fw, class).chain(fence.iter().map(|r| r.name))
}

/// Why the vehicle may not be declared to fly with no RC transmitter: only
/// disarmed, in ADOS control, on ArduCopter, in an open place. (The circle
/// fence makes the FC itself refuse to arm without a position, and the
/// `position` readiness item already blocks arming without one.)
pub fn declare_refusal(
    armed: bool,
    mode: ControlMode,
    fw: Option<Firmware>,
    class: VehicleClass,
    place: PlaceClass,
) -> Option<&'static str> {
    if armed {
        Some("armed")
    } else if mode != ControlMode::Ados {
        Some("control_mode")
    } else if fw != Some(Firmware::Ardupilot) || class != VehicleClass::Multirotor {
        Some("firmware")
    } else if place == PlaceClass::Confined {
        Some("confined")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fc_profile::{check_row, RowCheck};

    fn copter(rc: RcReceiver) -> Vec<ProfileParam> {
        profile(Firmware::Ardupilot, VehicleClass::Multirotor, rc).collect()
    }

    fn checks(rows: &[ProfileParam], params: &[(&str, f32)]) -> Vec<(&'static str, RowCheck)> {
        rows.iter()
            .map(|r| {
                let lookup = |n: &str| params.iter().find(|(k, _)| *k == n).map(|(_, v)| *v);
                (r.name, check_row(r, PlaceClass::Open, lookup))
            })
            .collect()
    }

    fn fence_ok() -> Vec<(&'static str, f32)> {
        vec![
            ("FENCE_ENABLE", 1.0),
            ("FENCE_TYPE", 3.0),
            ("FENCE_RADIUS", 70.0),
            ("FENCE_ALT_MAX", 50.0),
            ("FENCE_ACTION", 5.0),
        ]
    }

    #[test]
    fn absent_wants_the_rc_failsafe_off_and_the_fence_on() {
        let rows = copter(RcReceiver::Absent);
        let thr = rows.iter().find(|r| r.name == FS_THR).unwrap();
        assert_eq!(thr.value, 0.0);
        let mut params = fence_ok();
        params.push((FS_THR, 0.0));
        let c = checks(&rows, &params);
        for name in [
            "FS_THR_ENABLE",
            "FENCE_ENABLE",
            "FENCE_TYPE",
            "FENCE_RADIUS",
            "FENCE_ALT_MAX",
            "FENCE_ACTION",
        ] {
            assert_eq!(
                c.iter().find(|(n, _)| *n == name).unwrap().1,
                RowCheck::Ok,
                "{name}"
            );
        }
    }

    #[test]
    fn absent_refuses_the_rc_failsafe_on_or_no_fence() {
        let rows = copter(RcReceiver::Absent);
        let c = checks(&rows, &[(FS_THR, 5.0)]);
        assert!(matches!(
            c.iter().find(|(n, _)| *n == FS_THR).unwrap().1,
            RowCheck::Mismatch { .. }
        ));
        let mut params = fence_ok();
        params.retain(|(n, _)| *n != "FENCE_ENABLE");
        let c = checks(&rows, &params);
        assert_eq!(
            c.iter().find(|(n, _)| *n == "FENCE_ENABLE").unwrap().1,
            RowCheck::Absent
        );
        let fence_row = rows.iter().find(|r| r.name == "FENCE_ENABLE").unwrap();
        assert!(fence_row.absent_blocks.is_some(), "a missing fence blocks");
    }

    #[test]
    fn present_wants_the_rc_failsafe_on_and_checks_no_fence() {
        let rows = copter(RcReceiver::Present);
        let c = checks(&rows, &[(FS_THR, 0.0)]);
        assert!(matches!(
            c.iter().find(|(n, _)| *n == FS_THR).unwrap().1,
            RowCheck::Mismatch { .. }
        ));
        assert!(rows.iter().all(|r| !r.name.starts_with("FENCE_")));
    }

    #[test]
    fn the_fence_sits_outside_the_pilot_caps() {
        assert_eq!(FENCE_RADIUS_M, 70.0);
        assert_eq!(FENCE_ALT_MAX_M, 50.0);
        assert!((30.0..=10_000.0).contains(&FENCE_RADIUS_M));
    }

    #[test]
    fn the_declaration_needs_ados_arducopter_and_an_open_place() {
        let ok = |armed, mode, fw, class, place| declare_refusal(armed, mode, fw, class, place);
        let (a, mc, open) = (
            Some(Firmware::Ardupilot),
            VehicleClass::Multirotor,
            PlaceClass::Open,
        );
        assert_eq!(ok(false, ControlMode::Ados, a, mc, open), None);
        assert_eq!(ok(true, ControlMode::Ados, a, mc, open), Some("armed"));
        assert_eq!(
            ok(false, ControlMode::FcManual, a, mc, open),
            Some("control_mode")
        );
        assert_eq!(
            ok(false, ControlMode::Ados, Some(Firmware::Px4), mc, open),
            Some("firmware")
        );
        assert_eq!(
            ok(false, ControlMode::Ados, a, VehicleClass::Vtol, open),
            Some("firmware")
        );
        assert_eq!(
            ok(false, ControlMode::Ados, a, mc, PlaceClass::Confined),
            Some("confined")
        );
    }

    #[test]
    fn every_fence_name_is_read() {
        let names: Vec<_> = all_names(Firmware::Ardupilot, VehicleClass::Multirotor).collect();
        assert!(names.contains(&"FENCE_ENABLE") && names.contains(&"FS_THR_ENABLE"));
    }
}
