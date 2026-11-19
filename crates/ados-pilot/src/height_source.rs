//! The `height_source` readiness item: in a confined place on external
//! navigation, the EKF's height comes from the downward rangefinder.
//!
//! Indoors the barometer reads the room's pressure, which steps when a door
//! or the ventilation moves air. A 30 Pa step reads as 2.55 m of altitude: a
//! bench flight on the room lanes (`factory-room-lanes-pressure-step-ados`)
//! flew 2.6 m above its EKF height for the rest of the flight on the
//! barometer. With `EK3_SRCn_POSZ` 2 the EKF fuses the rangefinder as its
//! height while the reading is fresh and within range, and falls back to the
//! barometer otherwise (ArduPilot `NavEKF3_core::selectHeightForFusion`).
//! `EK3_RNG_USE_HGT` does not cover this: ArduCopter declares the terrain
//! stable only while taking off or landing, so it never switches in cruise.
//!
//! The item shows only for an ArduPilot multirotor in a confined place with
//! a source set on external navigation and a downward rangefinder fitted; it
//! warns, and names the sets to change.

use ados_protocol::pilot::{PlaceClass, Readiness, Severity, VehicleClass};

/// `EK3_SRCn_POSZ` value for the rangefinder.
pub const POSZ_RANGEFINDER: f32 = 2.0;
/// `RNGFNDn_ORIENT` value for downward (`ROTATION_PITCH_270`).
const ORIENT_DOWN: f32 = 25.0;

/// Parameters the item reads.
pub const PARAMS: &[&str] = &[
    "EK3_SRC1_POSXY",
    "EK3_SRC2_POSXY",
    "EK3_SRC3_POSXY",
    "EK3_SRC1_POSZ",
    "EK3_SRC2_POSZ",
    "EK3_SRC3_POSZ",
    "RNGFND1_TYPE",
    "RNGFND1_ORIENT",
    "RNGFND2_TYPE",
    "RNGFND2_ORIENT",
];

/// A downward rangefinder is configured.
fn downward_rangefinder(p: &impl Fn(&str) -> Option<f32>) -> bool {
    (1..=2).any(|n| {
        p(&format!("RNGFND{n}_TYPE")).is_some_and(|t| t != 0.0)
            && p(&format!("RNGFND{n}_ORIENT")).unwrap_or(ORIENT_DOWN) == ORIENT_DOWN
    })
}

/// The source sets (1-3) on external navigation whose height is not the
/// rangefinder.
fn sets_off_rangefinder(p: &impl Fn(&str) -> Option<f32>) -> Vec<u8> {
    (1..=3u8)
        .filter(|n| {
            p(&format!("EK3_SRC{n}_POSXY"))
                .is_some_and(|v| (v - crate::batt_failsafe::POSXY_EXTERNAL_NAV).abs() < 0.5)
        })
        .filter(|n| {
            p(&format!("EK3_SRC{n}_POSZ")).is_none_or(|v| (v - POSZ_RANGEFINDER).abs() >= 0.5)
        })
        .collect()
}

/// The item, or `None` where it does not apply.
pub fn item(
    class: VehicleClass,
    place: PlaceClass,
    p: impl Fn(&str) -> Option<f32>,
) -> Option<Readiness> {
    if class != VehicleClass::Multirotor
        || place != PlaceClass::Confined
        || !crate::batt_failsafe::external_nav(&p)
        || !downward_rangefinder(&p)
    {
        return None;
    }
    let off = sets_off_rangefinder(&p);
    let (ok, msg) = if off.is_empty() {
        (
            true,
            crate::batt_failsafe::spec("readiness.height_source.ok").to_owned(),
        )
    } else {
        let names = off
            .iter()
            .map(|n| format!("EK3_SRC{n}_POSZ"))
            .collect::<Vec<_>>()
            .join(", ");
        (
            false,
            crate::batt_failsafe::spec("readiness.height_source.baro").replace("{params}", &names),
        )
    };
    Some(Readiness {
        id: "height_source".to_owned(),
        ok,
        severity: Severity::Warn,
        msg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn params(rows: &[(&str, f32)]) -> impl Fn(&str) -> Option<f32> {
        let m: HashMap<String, f32> = rows.iter().map(|(n, v)| ((*n).to_owned(), *v)).collect();
        move |n| m.get(n).copied()
    }

    /// The bench room: external nav on set 1, height from the barometer, a
    /// downward rangefinder.
    const ROOM: &[(&str, f32)] = &[
        ("EK3_SRC1_POSXY", 6.0),
        ("EK3_SRC1_POSZ", 1.0),
        ("EK3_SRC2_POSXY", 0.0),
        ("EK3_SRC2_POSZ", 1.0),
        ("RNGFND1_TYPE", 10.0),
        ("RNGFND1_ORIENT", 25.0),
    ];

    #[test]
    fn a_confined_room_on_the_barometer_warns_and_names_the_set() {
        let r = item(VehicleClass::Multirotor, PlaceClass::Confined, params(ROOM)).unwrap();
        assert_eq!((r.ok, r.severity), (false, Severity::Warn));
        assert!(r.msg.contains("EK3_SRC1_POSZ"), "{}", r.msg);
        assert!(
            !r.msg.contains("EK3_SRC2_POSZ"),
            "set 2 is not external nav: {}",
            r.msg
        );
    }

    #[test]
    fn the_rangefinder_height_on_the_external_nav_set_passes() {
        let mut rows = ROOM.to_vec();
        rows[1] = ("EK3_SRC1_POSZ", POSZ_RANGEFINDER);
        let r = item(
            VehicleClass::Multirotor,
            PlaceClass::Confined,
            params(&rows),
        )
        .unwrap();
        assert!(r.ok, "{r:?}");
    }

    #[test]
    fn open_places_no_lidar_nav_or_no_rangefinder_show_no_item() {
        assert!(item(VehicleClass::Multirotor, PlaceClass::Open, params(ROOM)).is_none());
        assert!(item(VehicleClass::Fixedwing, PlaceClass::Confined, params(ROOM)).is_none());
        let mut gps = ROOM.to_vec();
        gps[0] = ("EK3_SRC1_POSXY", 3.0);
        assert!(item(VehicleClass::Multirotor, PlaceClass::Confined, params(&gps)).is_none());
        let mut no_rng = ROOM.to_vec();
        no_rng[4] = ("RNGFND1_TYPE", 0.0);
        assert!(item(
            VehicleClass::Multirotor,
            PlaceClass::Confined,
            params(&no_rng)
        )
        .is_none());
    }
}
