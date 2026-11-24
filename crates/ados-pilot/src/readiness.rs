//! Readiness items shown in the pre-flight checklist and the chip.

use ados_protocol::pilot::{
    ControlMode, ParamDiff, PlaceClass, RcReceiver, Readiness, ReadinessFix, Severity,
};

use crate::fc::FcState;
use crate::fc_profile::{
    avoid_conflict, check_row, Firmware, ProfileParam, RowCheck, AVOID_ENABLE, AVOID_MARGIN,
};

fn item(id: &str, ok: bool, severity: Severity, msg: impl Into<String>) -> Readiness {
    item_with(id, ok, severity, msg, Vec::new(), None)
}

/// An item with its parameter rows and fix.
pub(crate) fn item_with(
    id: &str,
    ok: bool,
    severity: Severity,
    msg: impl Into<String>,
    params: Vec<ParamDiff>,
    fix: Option<ReadinessFix>,
) -> Readiness {
    Readiness {
        id: id.to_owned(),
        ok,
        severity,
        msg: msg.into(),
        params,
        fix,
    }
}

/// A parameter row; the FC's `f32` reads back as it prints (`0.05`, not
/// `0.0500000007`).
pub(crate) fn diff(name: &str, have: Option<f32>, want: impl Into<String>) -> ParamDiff {
    ParamDiff {
        name: name.to_owned(),
        have: have.map(|v| v.to_string().parse().unwrap_or(f64::from(v))),
        want: want.into(),
    }
}

/// Open Aircraft setup at firmware detection.
fn detect_fix() -> ReadinessFix {
    ReadinessFix::SetupStep {
        step: "detect".to_owned(),
    }
}

/// Horizontal position test ratio that warns: half-way to rejection.
const EKF_POS_VARIANCE_WARN: f32 = 0.5;
/// How long, disarmed, the EKF's horizontal position must have held good
/// without a break before launch. ArduPilot's EKF3 can report a usable
/// position and lose all aiding within a second (on the bench: GPS origin
/// set, then every aiding source dropped 0.8 s later, and the launch armed in
/// between went to an EKF failsafe Land). Three seconds spans that transient
/// with margin, takes several consecutive reports at the FC's 2-5 Hz
/// EKF_STATUS_REPORT rate, and equals the staleness window, so one report
/// alone never clears the gate.
pub const POSITION_SETTLE_MS: u64 = 3000;
/// `EK3_SRCn_POSXY` value for GPS.
const POSXY_GPS: f32 = 3.0;
const POSXY: [&str; 3] = ["EK3_SRC1_POSXY", "EK3_SRC2_POSXY", "EK3_SRC3_POSXY"];
const VELXY: [&str; 3] = ["EK3_SRC1_VELXY", "EK3_SRC2_VELXY", "EK3_SRC3_VELXY"];

/// Whether the active EKF source set takes its horizontal position from GPS
/// (`EK3_SRCn_POSXY` 3). `nav_set` is the set Navigation reported; with
/// none reported the FC's set is unknown (an RC switch may have chosen any),
/// so set 1 is assumed only while no other set names a non-GPS source. An
/// unread `EK3_SRC1_POSXY` reads as ArduPilot's default, GPS; an unread
/// set 2 or 3 as its default, unused.
fn gps_source(fc: &FcState, nav_set: Option<u8>) -> bool {
    let p = |names: &[&str; 3], i: usize| fc.params.get(names[i]).copied();
    let is_gps = |v: f32| (v - POSXY_GPS).abs() < 0.5;
    let set_gps = |i: usize| p(&POSXY, i).map_or(i == 0, is_gps);
    let other_source = |i: usize| {
        [p(&POSXY, i), p(&VELXY, i)]
            .into_iter()
            .flatten()
            .any(|v| v != 0.0 && !is_gps(v))
    };
    match nav_set {
        Some(n @ 1..=3) => set_gps(usize::from(n) - 1),
        Some(_) => false,
        None => set_gps(0) && !(1..3).any(other_source),
    }
}

/// `position`: a fresh local position, and while the FC reports its EKF
/// status, a horizontal position the EKF trusts. A local position keeps
/// streaming after the EKF has lost its absolute fix, so it alone cannot say
/// the estimate is good. Disarmed (the launch gate), a GPS source set needs
/// an absolute position (a relative one from flow is only a prediction from
/// the start point), the position must have held good for
/// [`POSITION_SETTLE_MS`], and a status gone quiet blocks; armed,
/// ArduPilot's own test applies and a quiet status only warns.
fn position_item(fc: &FcState, nav_set: Option<u8>, now_ms: u64) -> Readiness {
    use crate::fc::ekf::{relative_enabled, EKF_POS_VARIANCE_BLOCK, EKF_STATUS_STALE_MS};
    let block = |msg| item("position", false, Severity::Block, msg);
    let warn = |msg| item("position", false, Severity::Warn, msg);
    if fc.fresh_local(now_ms).is_none() {
        return block("No position estimate");
    }
    let Some(ekf) = fc.ekf else {
        return item(
            "position",
            true,
            Severity::Block,
            "Position estimate available",
        );
    };
    if now_ms.saturating_sub(ekf.at_ms) >= EKF_STATUS_STALE_MS {
        let msg = "Position degraded (EKF): status not reporting";
        return if fc.armed { warn(msg) } else { block(msg) };
    }
    let v = ekf.v;
    let relative = relative_enabled(&fc.params);
    let gps = !fc.armed && gps_source(fc, nav_set);
    let settled =
        |since: Option<u64>| since.is_some_and(|s| now_ms.saturating_sub(s) >= POSITION_SETTLE_MS);
    if gps && !v.has_position(false, false) {
        block("Position degraded (EKF): no absolute horizontal position on the GPS source")
    } else if !v.has_position(fc.armed, relative) {
        block("Position degraded (EKF): no absolute or relative horizontal position")
    } else if !v.horiz_variance.is_finite() {
        block("Position degraded (EKF): position variance not reported")
    } else if v.horiz_variance >= EKF_POS_VARIANCE_BLOCK {
        block("Position degraded (EKF): position variance too high")
    } else if !fc.armed
        && !settled(if gps {
            fc.ekf_settle.abs_since_ms
        } else {
            fc.ekf_settle.any_since_ms
        })
    {
        block("Position settling (EKF): waiting for a steady estimate")
    } else if v.horiz_variance >= EKF_POS_VARIANCE_WARN {
        warn("Position degraded (EKF): position variance high")
    } else {
        item(
            "position",
            true,
            Severity::Block,
            "Position estimate available",
        )
    }
}

/// Every readiness item for the current state, in a place of `place`, with
/// the operator's RC declaration `rc` and the pilot's obstacle margin
/// `margin_m` (the FC's proximity stop follows it).
#[allow(clippy::too_many_arguments)]
pub fn items(
    fc: &FcState,
    mode: ControlMode,
    place: PlaceClass,
    rc: RcReceiver,
    margin_m: f32,
    controller_live: bool,
    sensors: &crate::sensor_readiness::SensorCtx,
    now_ms: u64,
) -> Vec<Readiness> {
    let mut out = Vec::with_capacity(10);
    out.push(if fc.linked(now_ms) {
        item(
            "fc_link",
            true,
            Severity::Block,
            "Flight controller connected",
        )
    } else {
        item(
            "fc_link",
            false,
            Severity::Block,
            "No flight controller heartbeat",
        )
    });
    match fc.firmware {
        Some(Firmware::Ardupilot) => {
            out.push(item("firmware", true, Severity::Block, "ArduPilot"));
        }
        Some(Firmware::Px4) => {
            let ok = mode != ControlMode::Ados;
            out.push(item_with(
                "firmware",
                ok,
                Severity::Block,
                "ADOS Pilot flies ArduPilot; use FC control on PX4",
                Vec::new(),
                (!ok).then(detect_fix),
            ))
        }
        None => out.push(item_with(
            "firmware",
            false,
            Severity::Block,
            "Flight controller firmware not identified",
            Vec::new(),
            Some(detect_fix()),
        )),
    }
    if let Some(fw) = fc.firmware {
        out.push(profile_item(fc, fw, mode, place, rc, margin_m));
        out.extend(backstop_item(fc, fw, place, rc, margin_m));
        if fw == Firmware::Ardupilot {
            out.push(crate::batt_failsafe::item_for(fc, mode, place));
            let p = |n: &str| fc.params.get(n).copied();
            out.extend(crate::height_source::item(fc.vehicle_class(), place, p));
        }
    }
    out.push(position_item(fc, sensors.nav_set, now_ms));
    out.extend(crate::sensor_readiness::items(fc, mode, sensors, now_ms));
    if mode == ControlMode::Ados {
        out.push(if controller_live {
            item("controller", true, Severity::Warn, "Controller connected")
        } else {
            item(
                "controller",
                false,
                Severity::Warn,
                "No controller: skills, gotos and missions only",
            )
        });
    }
    out
}

fn profile_item(
    fc: &FcState,
    fw: Firmware,
    mode: ControlMode,
    place: PlaceClass,
    rc: RcReceiver,
    margin_m: f32,
) -> Readiness {
    let class = fc.vehicle_class();
    let mut missing = Vec::new();
    let mut params = Vec::new();
    let mut unread = false;
    // The battery actions are the `battery_failsafe` item's: their right
    // value depends on the GNSS sources and the place class. The RC
    // failsafe and the fence follow the RC declaration. The rows the pilot
    // sets itself before the flight that needs them are not checked.
    let rows: Vec<ProfileParam> = crate::no_rc::profile(fw, class, rc, margin_m)
        .filter(|r| !r.pilot_sets && !crate::batt_failsafe::ACTIONS.contains(&r.name))
        .collect();
    for row in rows.iter().filter(|r| !r.backstop) {
        match check_row(row, place, |n| fc.params.get(n).copied()) {
            RowCheck::Ok => {}
            // Absent only once every name of the row was asked for without
            // an answer: a missing row is otherwise still being read.
            RowCheck::Absent if being_read(fc, row) => unread = true,
            RowCheck::Absent => {
                if let Some(why) = row.absent_blocks {
                    missing.push(format!("{} absent ({why})", row.name));
                    params.push(diff(row.name, None, why));
                }
            }
            RowCheck::Mismatch { name, have, want } => {
                missing.push(format!("{name} {have} (want {want})"));
                params.push(diff(name, Some(have), want.to_string()));
            }
        }
    }
    // The proximity backstop rows are `fc_backstop`'s and never block by
    // themselves: an FC without them flies on the pilot's brake alone. An
    // FC avoider that would act before that brake does block.
    if rows.iter().any(|r| r.backstop) {
        let lookup = |n: &str| fc.params.get(n).copied();
        unread |= [AVOID_ENABLE, AVOID_MARGIN]
            .into_iter()
            .any(|n| lookup(n).is_none() && !fc.param_absent(n));
        if let Some(c) = avoid_conflict(margin_m, lookup) {
            missing.push(c.to_string());
            params.push(diff(c.name, Some(c.have), c.want));
        }
    }
    let severity = if mode == ControlMode::Ados {
        Severity::Block
    } else {
        Severity::Warn
    };
    if !missing.is_empty() {
        item_with(
            "fc_profile",
            false,
            severity,
            format!("FC parameters differ: {}", missing.join(", ")),
            params,
            Some(ReadinessFix::ApplyProfile),
        )
    } else if unread {
        item(
            "fc_profile",
            false,
            severity,
            "Reading flight controller failsafe parameters",
        )
    } else {
        item(
            "fc_profile",
            true,
            severity,
            "FC failsafe parameters match the ADOS profile",
        )
    }
}

/// A row no name of which has been taken as absent yet: still being read.
fn being_read(fc: &FcState, row: &ProfileParam) -> bool {
    !std::iter::once(row.name)
        .chain(row.alt_names.iter().copied())
        .all(|n| fc.param_absent(n))
}

/// `fc_backstop`: the FC's proximity stop under the pilot's brake (the
/// profile's backstop rows). A warning in every control mode, never a
/// block: the backstop is a fallback, and an FC set up before it joined the
/// profile flies on the pilot's brake alone until the setup wizard's ADOS
/// profile step writes it. A row the FC reads at boot only (`PRX1_TYPE`)
/// says so. `None` for a profile without backstop rows.
fn backstop_item(
    fc: &FcState,
    fw: Firmware,
    place: PlaceClass,
    rc: RcReceiver,
    margin_m: f32,
) -> Option<Readiness> {
    let rows: Vec<ProfileParam> = crate::no_rc::profile(fw, fc.vehicle_class(), rc, margin_m)
        .filter(|r| r.backstop)
        .collect();
    if rows.is_empty() {
        return None;
    }
    let (mut missing, mut absent, mut unread) = (Vec::new(), Vec::new(), false);
    let mut params = Vec::new();
    for row in &rows {
        match check_row(row, place, |n| fc.params.get(n).copied()) {
            RowCheck::Ok => {}
            RowCheck::Absent if being_read(fc, row) => unread = true,
            RowCheck::Absent => absent.push(row.name),
            RowCheck::Mismatch { name, have, want } => {
                missing.push(if row.reboot {
                    format!("{name} {have} (want {want}, used after an FC reboot)")
                } else {
                    format!("{name} {have} (want {want})")
                });
                params.push(diff(name, Some(have), want.to_string()));
            }
        }
    }
    let (ok, msg) = if !missing.is_empty() {
        (
            false,
            format!(
                "FC proximity backstop not set: {}. The pilot's brake flies alone until the ADOS profile is applied in setup",
                missing.join(", ")
            ),
        )
    } else if unread {
        (
            false,
            "Reading the FC proximity backstop parameters".to_owned(),
        )
    } else if !absent.is_empty() {
        (
            false,
            format!(
                "FC proximity backstop not in this firmware: {} absent",
                absent.join(", ")
            ),
        )
    } else if crate::fc_profile::fc_avoid_margin_m(margin_m).is_none() {
        (
            true,
            "FC proximity backstop off: its stop does not fit inside the pilot's margin".to_owned(),
        )
    } else {
        (true, "FC proximity backstop set".to_owned())
    };
    let fix = (!missing.is_empty()).then_some(ReadinessFix::ApplyProfile);
    Some(item_with(
        "fc_backstop",
        ok,
        Severity::Warn,
        msg,
        params,
        fix,
    ))
}

/// The first blocking item that is not ok.
pub fn first_block(items: &[Readiness]) -> Option<&Readiness> {
    items
        .iter()
        .find(|r| !r.ok && r.severity == Severity::Block)
}

#[cfg(test)]
mod backstop_tests;
#[cfg(test)]
mod position_tests;
#[cfg(test)]
mod tests;
