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

/// Every readiness item for the current state.
pub fn items(
    fc: &FcState,
    mode: ControlMode,
    controller_live: bool,
    now_ms: u64,
) -> Vec<Readiness> {
    let mut out = Vec::with_capacity(6);
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
    }
    out.push(if fc.fresh_local(now_ms).is_some() {
        item(
            "position",
            true,
            Severity::Block,
            "Position estimate available",
        )
    } else {
        item("position", false, Severity::Block, "No position estimate")
    });
    out.push(
        if fc.rangefinder_agl(now_ms).is_some() || fc.landed() && fc.rangefinder.is_some() {
            item(
                "agl",
                true,
                Severity::Warn,
                "Downward rangefinder available",
            )
        } else {
            item(
                "agl",
                false,
                Severity::Warn,
                "No downward rangefinder: height reference is EKF altitude",
            )
        },
    );
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
    for row in fc_profile(fw, class) {
        match check_row(row, |n| fc.params.get(n).copied()) {
            RowCheck::Ok => {}
            RowCheck::Absent => unread |= fc.params.is_empty(),
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
