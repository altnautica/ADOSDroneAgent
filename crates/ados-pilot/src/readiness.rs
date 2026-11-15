//! Readiness items shown in the pre-flight checklist and the chip.

use ados_protocol::pilot::{ControlMode, Readiness, Severity};

use crate::fc::FcState;
use crate::fc_profile::{check_row, fc_profile, Firmware, RowCheck};

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

/// Every readiness item for the current state.
pub fn items(
    fc: &FcState,
    mode: ControlMode,
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
        out.push(profile_item(fc, fw, mode));
        if fw == Firmware::Ardupilot {
            out.push(crate::batt_failsafe::item_for(fc, mode));
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

fn profile_item(fc: &FcState, fw: Firmware, mode: ControlMode) -> Readiness {
    let class = fc.vehicle_class();
    let mut missing = Vec::new();
    let mut unread = false;
    // The battery actions are the `battery_failsafe` item's: their right
    // value depends on whether the vehicle navigates by GNSS.
    let rows = fc_profile(fw, class)
        .iter()
        .filter(|r| !crate::batt_failsafe::ACTIONS.contains(&r.name));
    for row in rows {
        match check_row(row, |n| fc.params.get(n).copied()) {
            RowCheck::Ok => {}
            RowCheck::Absent => match row.absent_blocks {
                Some(why) if !fc.params.is_empty() => {
                    missing.push(format!("{} absent ({why})", row.name))
                }
                _ => unread |= fc.params.is_empty(),
            },
            RowCheck::Mismatch { name, have, want } => {
                missing.push(format!("{name} {have} (want {want})"))
            }
        }
    }
    let severity = if mode == ControlMode::Ados {
        Severity::Block
    } else {
        Severity::Warn
    };
    if unread {
        item(
            "fc_profile",
            false,
            severity,
            "Reading flight controller failsafe parameters",
        )
    } else if missing.is_empty() {
        item(
            "fc_profile",
            true,
            severity,
            "FC failsafe parameters match the ADOS profile",
        )
    } else {
        item(
            "fc_profile",
            false,
            severity,
            format!("FC parameters differ: {}", missing.join(", ")),
        )
    }
}

/// The first blocking item that is not ok.
pub fn first_block(items: &[Readiness]) -> Option<&Readiness> {
    items
        .iter()
        .find(|r| !r.ok && r.severity == Severity::Block)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::pilot::VehicleClass;

    /// An ArduCopter whose parameters match the whole profile.
    fn copter() -> FcState {
        let mut fc = FcState {
            firmware: Some(Firmware::Ardupilot),
            ..FcState::default()
        };
        for row in fc_profile(Firmware::Ardupilot, VehicleClass::Multirotor) {
            fc.params.insert(row.name.into(), row.value);
        }
        fc
    }

    fn profile(fc: &FcState) -> Readiness {
        profile_item(fc, Firmware::Ardupilot, ControlMode::Ados)
    }

    #[test]
    fn each_avoidance_and_return_row_blocks_when_it_differs() {
        assert!(profile(&copter()).ok);
        for (name, want, wrong) in [
            ("OA_TYPE", 0.0, 1.0),
            ("AVOID_ENABLE", 0.0, 7.0),
            ("FS_GCS_ENABLE", 4.0, 1.0),
            ("FS_THR_ENABLE", 5.0, 1.0),
            ("SRTL_POINTS", 500.0, 300.0),
            ("SRTL_ACCURACY", 2.0, 1.0),
        ] {
            let mut fc = copter();
            assert_eq!(fc.params.get(name), Some(&want), "{name} in the profile");
            fc.params.insert(name.into(), wrong);
            let r = profile(&fc);
            assert!(!r.ok, "{name} {wrong} passes");
            assert_eq!(r.severity, Severity::Block, "{name}");
            assert!(
                r.msg.contains(&format!("{name} {wrong} (want {want})")),
                "{name}: {}",
                r.msg
            );
        }
    }

    #[test]
    fn absent_smartrtl_blocks_while_another_absent_row_is_skipped() {
        let mut fc = copter();
        fc.params.remove("GUID_TIMEOUT");
        fc.params.remove("OA_TYPE");
        assert!(profile(&fc).ok, "{}", profile(&fc).msg);

        let mut fc = copter();
        fc.params.remove("SRTL_POINTS");
        let r = profile(&fc);
        assert!(!r.ok);
        assert_eq!(r.severity, Severity::Block);
        assert!(r.msg.contains("SRTL_POINTS absent"), "{}", r.msg);
    }
}
