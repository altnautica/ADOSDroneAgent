//! `battery_failsafe` readiness item: whether the flight controller acts on a
//! low or critical battery, and acts safely where the vehicle flies. ArduPilot's
//! default for `BATT_FS_LOW_ACT` and `BATT_FS_CRT_ACT` is 0, which only warns:
//! the battery runs flat in flight. Worded by the spec's
//! `readiness.battery_failsafe.*` strings (`cockpit-spec/setup-strings.json`).
//!
//! - no battery monitor (`BATT_MONITOR` 0), or the parameters not read yet:
//!   unknown, shown as "—" (warn);
//! - the actions differ from [`recommended`]: graded like the `fc_profile`
//!   item, block under ADOS control and warn otherwise. This item owns the
//!   ADOS profile's `BATT_FS_*_ACT` rows; `fc_profile` leaves them out, so
//!   the two never disagree. A multirotor that navigates by GNSS needs low =
//!   RTL and critical = Land; one that navigates without GNSS (GPS disabled,
//!   or the active EKF source set's horizontal position is not GPS: external
//!   nav, optical flow alone) needs Land for both, since RTL climbs to the
//!   RTL altitude, into a roof indoors. Fixed-wing and VTOL need the profile's
//!   low action;
//! - a level with neither a voltage nor a capacity threshold: warn;
//! - a multirotor RTL battery action whose RTL altitude is above the pilot's
//!   configured altitude limit (`pilot.max_alt_agl_m`): warn.
//!
//! The setup wizard's `ados_profile` step writes [`recommended`] actions.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use ados_protocol::pilot::{ControlMode, Readiness, Severity, VehicleClass};
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
    "GPS1_TYPE",
    "RTL_ALT_M",
    "EK3_SRC1_POSXY",
    "EK3_SRC2_POSXY",
    "EK3_SRC3_POSXY",
];

/// The same parameters' names before ArduCopter 4.7: `GPS_TYPE`, and
/// `RTL_ALT` in centimetres.
pub const LEGACY_PARAMS: &[&str] = &["GPS_TYPE", "RTL_ALT"];

/// ArduCopter battery actions.
const ACT_NONE: f32 = 0.0;
const ACT_LAND: f32 = 1.0;
/// Actions that may fly RTL and so climb to the RTL altitude: RTL, SmartRTL
/// or RTL, Auto DO_LAND_START or RTL.
const ACT_CLIMBS: [f32; 3] = [2.0, 3.0, 6.0];
/// `EK3_SRCn_POSXY` value for GPS.
const POSXY_GPS: f32 = 3.0;

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
    let full = format!("readiness.battery_failsafe.{key}");
    STRINGS
        .get(&full)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("setup-strings.json lacks {full}"))
}

/// What the item reads beyond the FC's parameters.
#[derive(Debug, Clone, Copy)]
pub struct BattCtx {
    /// The active EKF source set (1–3) as the pilot knows it.
    pub nav_set: u8,
    /// The highest the vehicle may fly, metres above home, when configured.
    pub ceiling_m: Option<f32>,
}

/// The vehicle navigates without GNSS: GPS disabled (`GPS1_TYPE`/`GPS_TYPE`
/// 0), or the EKF source set `nav_set` takes horizontal position from
/// something other than GPS (external nav, optical flow alone, none).
pub fn gnss_free(p: impl Fn(&str) -> Option<f32>, nav_set: u8) -> bool {
    if p("GPS1_TYPE").or_else(|| p("GPS_TYPE")) == Some(0.0) {
        return true;
    }
    let posxy = match nav_set {
        1 => p("EK3_SRC1_POSXY"),
        2 => p("EK3_SRC2_POSXY"),
        3 => p("EK3_SRC3_POSXY"),
        _ => None,
    };
    posxy.is_some_and(|v| (v - POSXY_GPS).abs() >= 0.5)
}

/// The battery actions the ADOS profile needs: the class's `fc_profile`
/// rows, with Land for both on a multirotor that navigates without GNSS.
/// `crt` is `None` where the profile sets no critical action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recommended {
    pub low: Option<f32>,
    pub crt: Option<f32>,
}

pub fn recommended(class: VehicleClass, gnss_free: bool) -> Recommended {
    if class == VehicleClass::Multirotor && gnss_free {
        return Recommended {
            low: Some(ACT_LAND),
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
pub fn profile_value(name: &str, value: f32, class: VehicleClass, gnss_free: bool) -> f32 {
    let r = recommended(class, gnss_free);
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

/// The item for an ArduPilot flight controller.
pub fn item_for(fc: &FcState, mode: ControlMode, ctx: &BattCtx) -> Readiness {
    let p = |n: &str| fc.params.get(n).copied();
    let Some(monitor) = p("BATT_MONITOR") else {
        return item(false, Severity::Warn, text("unread"));
    };
    if monitor == 0.0 {
        return item(false, Severity::Warn, text("no_monitor"));
    }
    let (Some(low), Some(crt)) = (p("BATT_FS_LOW_ACT"), p("BATT_FS_CRT_ACT")) else {
        return item(false, Severity::Warn, text("unread"));
    };
    let hazard = if mode == ControlMode::Ados {
        Severity::Block
    } else {
        Severity::Warn
    };
    let class = fc.vehicle_class();
    let copter = class == VehicleClass::Multirotor;
    let confined = copter && gnss_free(p, ctx.nav_set);
    let key = |k: &str| {
        if confined {
            text(&format!("{k}_confined"))
        } else {
            text(k)
        }
    };
    if low == ACT_NONE && crt == ACT_NONE {
        return item(false, hazard, key("none"));
    }
    let climbs = copter && (ACT_CLIMBS.contains(&low) || ACT_CLIMBS.contains(&crt));
    if climbs && confined {
        return item(false, hazard, text("confined_rtl"));
    }
    let rec = recommended(class, confined);
    let differs = |have: f32, want: Option<f32>| want.is_some_and(|w| (have - w).abs() >= 0.5);
    if differs(low, rec.low) {
        let msg = match (low == ACT_NONE, copter) {
            (true, _) => key("low_none"),
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
    let rtl_m = p("RTL_ALT_M").or_else(|| p("RTL_ALT").map(|cm| cm / 100.0));
    if let (true, Some(rtl_m), Some(ceiling_m)) = (climbs, rtl_m, ctx.ceiling_m) {
        if rtl_m > ceiling_m {
            let msg = text("rtl_above_ceiling")
                .replace("{rtl_m}", &format!("{rtl_m:.0}"))
                .replace("{ceiling_m}", &format!("{ceiling_m:.0}"));
            return item(false, Severity::Warn, msg);
        }
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
            "rtl_above_ceiling",
            "differs",
            "differs_confined",
            "differs_low",
            "levels_unset",
            "no_monitor",
            "unread",
            "fix",
        ] {
            assert!(!super::text(k).is_empty());
        }
    }

    #[test]
    fn gnss_free_reads_the_gps_type_and_the_active_source_set() {
        use super::gnss_free;
        let p = |kv: &'static [(&'static str, f32)]| {
            move |n: &str| kv.iter().find(|(k, _)| *k == n).map(|(_, v)| *v)
        };
        assert!(!gnss_free(p(&[]), 1));
        assert!(!gnss_free(
            p(&[("GPS1_TYPE", 1.0), ("EK3_SRC1_POSXY", 3.0)]),
            1
        ));
        assert!(gnss_free(p(&[("GPS1_TYPE", 0.0)]), 1));
        assert!(gnss_free(p(&[("GPS_TYPE", 0.0)]), 1));
        // External nav (6) on set 2, GPS on set 1.
        let kv = &[("EK3_SRC1_POSXY", 3.0), ("EK3_SRC2_POSXY", 6.0)];
        assert!(!gnss_free(p(kv), 1));
        assert!(gnss_free(p(kv), 2));
    }
}
