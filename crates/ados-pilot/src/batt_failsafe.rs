//! `battery_failsafe` readiness item: whether the flight controller acts on a
//! low or critical battery, and acts safely where the vehicle flies. ArduPilot's
//! default for `BATT_FS_LOW_ACT` and `BATT_FS_CRT_ACT` is 0, which only warns:
//! the battery runs flat in flight. Worded by the spec's
//! `readiness.battery_failsafe.*` strings (`cockpit-spec/setup-strings.json`).
//!
//! - no battery monitor (`BATT_MONITOR` 0): neither the FC nor the pilot can
//!   act on the battery, graded like a wrong action (block under ADOS control,
//!   warn otherwise). The parameters not read yet: unknown (warn);
//! - the actions differ from [`recommended`]: block under ADOS control and
//!   warn otherwise. This item owns the ADOS profile's `BATT_FS_*_ACT` rows;
//!   `fc_profile` leaves them out, so the two never disagree. A multirotor
//!   needs low = SmartRTL or Land (4) and critical = Land, except one that
//!   lands on a low battery ([`lands_on_low`]: it may navigate without GNSS,
//!   in an open place or without external nav), which needs Land for both:
//!   RTL climbs to the RTL altitude, into a roof indoors. In a confined place
//!   on external nav the low battery retraces the flown path, which the route
//!   check cleared, rather than landing beside the walls and furniture it
//!   flew past. Fixed-wing and VTOL need the profile's low action (ArduPlane
//!   RTL); they have no critical row;
//! - a level with neither a voltage nor a capacity threshold: warn;
//! - no pack capacity (`BATT_CAPACITY` 10 mAh or less): warn. ArduPilot then
//!   reports no remaining charge, so the capacity levels and the pilot's
//!   reserve never act.
//!
//! The setup wizard's `ados_profile` step writes [`recommended`] actions,
//! from the same [`lands_on_low`] rule, so a fix always settles the item.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use ados_protocol::pilot::{ControlMode, PlaceClass, Readiness, Severity, VehicleClass};
use serde::Deserialize;

use crate::fc::FcState;
use crate::fc_profile::{fc_profile, Firmware};

pub const ID: &str = "battery_failsafe";

/// The two action parameters; this item owns them, so the `fc_profile`
/// readiness check leaves them out.
pub const ACTIONS: [&str; 2] = ["BATT_FS_LOW_ACT", "BATT_FS_CRT_ACT"];

/// Parameters the item reads (current ArduCopter names).
pub const PARAMS: &[&str] = &[
    "BATT_MONITOR",
    "BATT_FS_LOW_ACT",
    "BATT_FS_CRT_ACT",
    "BATT_LOW_VOLT",
    "BATT_LOW_MAH",
    "BATT_CRT_VOLT",
    "BATT_CRT_MAH",
    "BATT_CAPACITY",
    "GPS1_TYPE",
    "EK3_SRC1_POSXY",
    "EK3_SRC2_POSXY",
    "EK3_SRC3_POSXY",
];

/// The same parameter's name before ArduCopter 4.7.
pub const LEGACY_PARAMS: &[&str] = &["GPS_TYPE"];

/// ArduCopter battery actions.
const ACT_NONE: f32 = 0.0;
const ACT_LAND: f32 = 1.0;
/// SmartRTL, or Land when SmartRTL has no path: the low action the ADOS
/// profile writes on a copter that returns on a low battery (it retraces the
/// flown path and never climbs to the RTL altitude).
const ACT_SMART_RTL_LAND: f32 = 4.0;
/// Actions that may fly RTL and so climb to the RTL altitude: RTL, SmartRTL
/// or RTL, Auto DO_LAND_START or RTL.
const ACT_CLIMBS: [f32; 3] = [2.0, 3.0, 6.0];
/// `EK3_SRCn_POSXY` value for GPS.
const POSXY_GPS: f32 = 3.0;
/// `EK3_SRCn_POSXY` value for external nav.
const POSXY_EXTERNAL_NAV: f32 = 6.0;
/// `EK3_SRCn_POSXY` values that position without GNSS: Beacon, ExternalNav.
const POSXY_GNSS_FREE: [f32; 2] = [4.0, POSXY_EXTERNAL_NAV];
/// ArduPilot treats a `BATT_CAPACITY` of this or less as no capacity and
/// reports no remaining charge
/// (`AP_BattMonitor_Backend::capacity_remaining_pct`).
pub const CAPACITY_MIN_MAH: f32 = 10.0;

const SETUP_STRINGS_JSON: &str =
    include_str!("../../../web-shared/src/cockpit-spec/setup-strings.json");

#[derive(Deserialize)]
struct Table {
    strings: BTreeMap<String, String>,
}

static STRINGS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str::<Table>(SETUP_STRINGS_JSON)
        .expect("setup-strings.json is the spec string table")
        .strings
});

/// The spec text for `readiness.battery_failsafe.<key>`.
pub fn text(key: &str) -> &'static str {
    spec(&format!("readiness.battery_failsafe.{key}"))
}

/// The spec text for the full key `key`.
pub fn spec(key: &str) -> &'static str {
    STRINGS
        .get(key)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("setup-strings.json lacks {key}"))
}

/// The vehicle may navigate without GNSS: GPS disabled
/// (`GPS1_TYPE`/`GPS_TYPE` 0), the boot EKF source set (1) takes horizontal
/// position from something other than GPS (external nav, optical flow
/// alone, none), or any source set (1–3) positions from a beacon or
/// external nav. The FC's battery action is fixed while the source set may
/// switch in flight (Navigation moves to its external-nav set when GPS
/// fails), so every set the vehicle may fly counts, not the active one.
/// The setup wizard and this item read the same rule.
pub fn gnss_free(p: impl Fn(&str) -> Option<f32>) -> bool {
    if p("GPS1_TYPE").or_else(|| p("GPS_TYPE")) == Some(0.0) {
        return true;
    }
    if p("EK3_SRC1_POSXY").is_some_and(|v| (v - POSXY_GPS).abs() >= 0.5) {
        return true;
    }
    ["EK3_SRC1_POSXY", "EK3_SRC2_POSXY", "EK3_SRC3_POSXY"]
        .iter()
        .filter_map(|n| p(n))
        .any(|v| POSXY_GNSS_FREE.iter().any(|f| (v - f).abs() < 0.5))
}

/// Some EKF source set (1–3) takes horizontal position from external nav.
pub fn external_nav(p: impl Fn(&str) -> Option<f32>) -> bool {
    ["EK3_SRC1_POSXY", "EK3_SRC2_POSXY", "EK3_SRC3_POSXY"]
        .iter()
        .filter_map(|n| p(n))
        .any(|v| (v - POSXY_EXTERNAL_NAV).abs() < 0.5)
}

/// A multirotor in `place` lands where it is on a low battery, rather than
/// returning along its flown path: it may navigate without GNSS
/// ([`gnss_free`]), unless the place is confined and it positions from
/// external nav. There SmartRTL retraces the path the route check cleared
/// (ArduCopter records it from the EKF's local position, which external nav
/// provides, and lands in place when it has none), while a landing in place
/// comes down beside the walls and furniture the confined margin let it fly
/// past. Elsewhere without GNSS the vehicle keeps Land. The pilot's reserve
/// rule, the setup wizard and this item read the same rule.
pub fn lands_on_low(place: PlaceClass, p: impl Fn(&str) -> Option<f32>) -> bool {
    gnss_free(&p) && !(place == PlaceClass::Confined && external_nav(&p))
}

/// The battery actions the ADOS profile needs: Land for both on a
/// multirotor that lands on a low battery ([`lands_on_low`]),
/// SmartRTL-or-Land low and Land critical on any other multirotor, the
/// class's `fc_profile` rows otherwise. `crt` is `None` where the profile
/// sets no critical action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recommended {
    pub low: Option<f32>,
    pub crt: Option<f32>,
}

pub fn recommended(class: VehicleClass, lands: bool) -> Recommended {
    if class == VehicleClass::Multirotor {
        return Recommended {
            low: Some(if lands { ACT_LAND } else { ACT_SMART_RTL_LAND }),
            crt: Some(ACT_LAND),
        };
    }
    let row = |name: &str| {
        fc_profile(Firmware::Ardupilot, class)
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value)
    };
    Recommended {
        low: row(ACTIONS[0]),
        crt: row(ACTIONS[1]),
    }
}

/// The recommended value for an `fc_profile` row: [`recommended`] for the
/// battery actions, the row's own value otherwise.
pub fn profile_value(name: &str, value: f32, class: VehicleClass, lands: bool) -> f32 {
    let r = recommended(class, lands);
    match name {
        n if n == ACTIONS[0] => r.low.unwrap_or(value),
        n if n == ACTIONS[1] => r.crt.unwrap_or(value),
        _ => value,
    }
}

fn item(ok: bool, severity: Severity, msg: impl Into<String>) -> Readiness {
    Readiness {
        id: ID.to_owned(),
        ok,
        severity,
        msg: msg.into(),
    }
}

/// The item for an ArduPilot flight controller flying in `place`.
pub fn item_for(fc: &FcState, mode: ControlMode, place: PlaceClass) -> Readiness {
    let p = |n: &str| fc.params.get(n).copied();
    let hazard = if mode == ControlMode::Ados {
        Severity::Block
    } else {
        Severity::Warn
    };
    let Some(monitor) = p("BATT_MONITOR") else {
        return item(false, Severity::Warn, text("unread"));
    };
    if monitor == 0.0 {
        return item(false, hazard, text("no_monitor"));
    }
    let (Some(low), Some(crt)) = (p("BATT_FS_LOW_ACT"), p("BATT_FS_CRT_ACT")) else {
        return item(false, Severity::Warn, text("unread"));
    };
    let class = fc.vehicle_class();
    let copter = class == VehicleClass::Multirotor;
    // The `_confined` texts name Land for a copter that may fly without
    // GNSS; one that returns on a low battery reads the GNSS texts.
    let lands = copter && lands_on_low(place, p);
    let key = |k: &str| {
        if lands {
            text(&format!("{k}_confined"))
        } else {
            text(k)
        }
    };
    if low == ACT_NONE && crt == ACT_NONE {
        return item(
            false,
            hazard,
            if copter {
                key("none")
            } else {
                text("low_none_plane")
            },
        );
    }
    let climbs = copter && (ACT_CLIMBS.contains(&low) || ACT_CLIMBS.contains(&crt));
    if climbs && lands {
        return item(false, hazard, text("confined_rtl"));
    }
    let rec = recommended(class, lands);
    let differs = |have: f32, want: Option<f32>| want.is_some_and(|w| (have - w).abs() >= 0.5);
    if differs(low, rec.low) {
        let msg = match (low == ACT_NONE, copter) {
            (true, true) => key("low_none"),
            (true, false) => text("low_none_plane"),
            (false, true) => key("differs"),
            (false, false) => text("differs_low"),
        };
        return item(false, hazard, msg);
    }
    if differs(crt, rec.crt) {
        let msg = if crt == ACT_NONE {
            text("critical_none")
        } else {
            key("differs")
        };
        return item(false, hazard, msg);
    }
    let set = |volt: &str, mah: &str| p(volt).unwrap_or(0.0) > 0.0 || p(mah).unwrap_or(0.0) > 0.0;
    if !set("BATT_LOW_VOLT", "BATT_LOW_MAH") || !set("BATT_CRT_VOLT", "BATT_CRT_MAH") {
        return item(false, Severity::Warn, text("levels_unset"));
    }
    if p("BATT_CAPACITY").is_some_and(|c| c <= CAPACITY_MIN_MAH) {
        return item(false, Severity::Warn, text("capacity_unset"));
    }
    item(true, hazard, text("ok"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_parameter_is_a_real_ardupilot_name() {
        let real = include_str!("fc_sensor_profile/fixtures/arducopter-x500-ados-params.txt");
        for n in super::PARAMS {
            assert!(
                real.lines().any(|l| l.trim() == *n),
                "{n} is not an ArduCopter parameter"
            );
        }
    }

    #[test]
    fn every_text_is_in_the_spec() {
        for k in [
            "ok",
            "none",
            "none_confined",
            "low_none",
            "low_none_confined",
            "critical_none",
            "confined_rtl",
            "differs",
            "differs_confined",
            "differs_low",
            "low_none_plane",
            "levels_unset",
            "capacity_unset",
            "no_monitor",
            "unread",
            "fix",
        ] {
            assert!(!super::text(k).is_empty());
        }
    }

    #[test]
    fn gnss_free_reads_the_gps_type_and_every_source_set() {
        use super::gnss_free;
        let p = |kv: &'static [(&'static str, f32)]| {
            move |n: &str| kv.iter().find(|(k, _)| *k == n).map(|(_, v)| *v)
        };
        assert!(!gnss_free(p(&[])));
        assert!(!gnss_free(p(&[
            ("GPS1_TYPE", 1.0),
            ("EK3_SRC1_POSXY", 3.0)
        ])));
        assert!(gnss_free(p(&[("GPS1_TYPE", 0.0)])));
        assert!(gnss_free(p(&[("GPS_TYPE", 0.0)])));
        // Optical flow alone on the boot set.
        assert!(gnss_free(p(&[("EK3_SRC1_POSXY", 0.0)])));
        // GPS on set 1, external nav (6) or a beacon (4) on a later set: the
        // FC may switch to it in flight.
        assert!(gnss_free(p(&[
            ("EK3_SRC1_POSXY", 3.0),
            ("EK3_SRC2_POSXY", 6.0)
        ])));
        assert!(gnss_free(p(&[
            ("EK3_SRC1_POSXY", 3.0),
            ("EK3_SRC3_POSXY", 4.0)
        ])));
        // An unused later set (0) is not a source the FC switches to.
        assert!(!gnss_free(p(&[
            ("EK3_SRC1_POSXY", 3.0),
            ("EK3_SRC2_POSXY", 0.0)
        ])));
    }
}
