//! Readiness items shown in the pre-flight checklist and the chip.

use ados_protocol::pilot::{ControlMode, PlaceClass, RcReceiver, Readiness, Severity};

use crate::fc::FcState;
use crate::fc_profile::{
    avoid_conflict, check_row, Firmware, ProfileParam, RowCheck, AVOID_ENABLE, AVOID_MARGIN,
};

fn item(id: &str, ok: bool, severity: Severity, msg: impl Into<String>) -> Readiness {
    Readiness {
        id: id.to_owned(),
        ok,
        severity,
        msg: msg.into(),
    }
}

/// An EKF_STATUS_REPORT older than this no longer vouches for the estimate.
const EKF_STATUS_STALE_MS: u64 = 3000;
/// Horizontal position test ratio that warns: half-way to rejection.
const EKF_POS_VARIANCE_WARN: f32 = 0.5;
/// Horizontal position test ratio that blocks: ArduPilot's default EKF
/// failsafe threshold (`FS_EKF_THRESH` 0.8).
const EKF_POS_VARIANCE_BLOCK: f32 = 0.8;

/// `position`: a fresh local position, and while the FC reports its EKF
/// status, an absolute horizontal position the EKF trusts. A local position
/// keeps streaming after the EKF has lost its absolute fix, so it alone
/// cannot say the estimate is good.
fn position_item(fc: &FcState, now_ms: u64) -> Readiness {
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
        return warn("Position degraded (EKF): status not reporting");
    }
    let v = ekf.v;
    let enabled = |n: &str| fc.params.get(n).is_some_and(|p| *p != 0.0);
    let relative_enabled = enabled("FLOW_TYPE") || enabled("VISO_TYPE");
    if !v.has_position(fc.armed, relative_enabled) {
        block("Position degraded (EKF): no absolute or relative horizontal position")
    } else if v.horiz_variance >= EKF_POS_VARIANCE_BLOCK {
        block("Position degraded (EKF): position variance too high")
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
        Some(Firmware::Px4) => out.push(item(
            "firmware",
            mode != ControlMode::Ados,
            Severity::Block,
            "ADOS Pilot flies ArduPilot; use FC control on PX4",
        )),
        None => out.push(item(
            "firmware",
            false,
            Severity::Block,
            "Flight controller firmware not identified",
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
    out.push(position_item(fc, now_ms));
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
                    missing.push(format!("{} absent ({why})", row.name))
                }
            }
            RowCheck::Mismatch { name, have, want } => {
                missing.push(format!("{name} {have} (want {want})"))
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
        missing.extend(avoid_conflict(margin_m, lookup));
    }
    let severity = if mode == ControlMode::Ados {
        Severity::Block
    } else {
        Severity::Warn
    };
    if !missing.is_empty() {
        item(
            "fc_profile",
            false,
            severity,
            format!("FC parameters differ: {}", missing.join(", ")),
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
    for row in &rows {
        match check_row(row, place, |n| fc.params.get(n).copied()) {
            RowCheck::Ok => {}
            RowCheck::Absent if being_read(fc, row) => unread = true,
            RowCheck::Absent => absent.push(row.name),
            RowCheck::Mismatch { name, have, want } if row.reboot => missing.push(format!(
                "{name} {have} (want {want}, used after an FC reboot)"
            )),
            RowCheck::Mismatch { name, have, want } => {
                missing.push(format!("{name} {have} (want {want})"))
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
    Some(item("fc_backstop", ok, Severity::Warn, msg))
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
mod tests;
