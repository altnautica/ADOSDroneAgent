//! Give this aircraft a MAVLink identity of its own.
//!
//! ## What this closes
//!
//! Two flight controllers on the shipped default system id are ONE vehicle to a
//! ground station, and a command addressed to that id is accepted by both — an
//! operator commanding one aircraft is commanding two. The ground station can
//! now SEE that collision, and the rule for what each aircraft should carry is
//! settled in `ados_groundlink::fleet_identity_policy`. This is the part that
//! acts on it.
//!
//! ## Why it ships switched off
//!
//! Acting means writing `SYSID_THISMAV` to the autopilot, and on the firmware
//! we target that does not take effect until the autopilot reboots. So the
//! whole feature is one config flag away from doing nothing, and the flag is
//! off. That is not caution for its own sake: nobody has yet watched this run
//! against a real airframe, and an autopilot reboot is not a thing to discover
//! the edges of in the field.
//!
//! What ships enabled is the OBSERVATION — the decision is evaluated and
//! reported on every tick whether or not the write is allowed, so an operator
//! can see what would happen before letting it happen.
//!
//! ## Why it never reboots the autopilot itself
//!
//! It writes the parameter and says a reboot is required. It does not command
//! one. A parameter write is reversible by another write; a reboot is a
//! discontinuity in a vehicle's control, and choosing its moment belongs to
//! whoever can see the aircraft.

use std::time::Duration;

use ados_groundlink::fleet_identity_policy::{decide_identity, IdentityDecision};
use serde_json::Value;

/// How often the aircraft's identity is reconciled.
///
/// Slow on purpose. A slot changes at pair time and an identity follows it, so
/// this catches a boot or a re-pair rather than anything faster, and the
/// steady state must cost nothing on a companion that has flying to do.
pub const IDENTITY_RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// The autopilot parameter holding a vehicle's own MAVLink system id.
const SYSID_PARAM: &str = "SYSID_THISMAV";

/// Config key gating the write. Absent or false means observe only.
const ENABLE_KEY: &[&str] = &["mavlink", "adopt_slot_system_id"];

/// Config key holding this node's issued fleet slot.
const SLOT_KEY: &[&str] = &["video", "wfb", "fleet_slot"];

/// What the reconciler concluded on one tick, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityOutcome {
    /// Nothing to do.
    Settled,
    /// A change is wanted and the write is allowed; the parameter was written
    /// and the autopilot needs a reboot for it to take effect.
    Written { target: u8 },
    /// A change is wanted but the write is switched off. Reported so the gap
    /// between what is wanted and what is permitted is visible rather than
    /// silent.
    WouldWrite { target: u8 },
    /// A change is wanted but the aircraft is armed.
    DeferredArmed { target: u8 },
    /// Nothing could be decided: no slot, an unusable one, or the autopilot has
    /// not reported its current id.
    Undecided(&'static str),
    /// The write was attempted and the autopilot did not take it.
    WriteFailed { target: u8 },
}

/// Pluck a nested value out of a config document.
fn nested<'a>(cfg: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(cfg, |node, key| node.get(*key))
}

/// Whether the operator has switched the write on.
pub fn write_enabled(cfg: &Value) -> bool {
    nested(cfg, ENABLE_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// This node's issued fleet slot, if it has one.
///
/// Zero reads as absent rather than as slot zero: it is the unset value the
/// config ships with, and slot zero is not issuable.
pub fn configured_slot(cfg: &Value) -> Option<u8> {
    let raw = nested(cfg, SLOT_KEY).and_then(Value::as_u64)?;
    match u8::try_from(raw).ok()? {
        0 => None,
        s => Some(s),
    }
}

/// Decide what this tick should do, without doing any of it.
///
/// Pure over its inputs so every branch — including the ones that need an armed
/// aircraft or a missing parameter — is reachable in a test rather than only on
/// a bench.
pub fn decide_tick(cfg: &Value, current_sysid: Option<u8>, armed: bool) -> IdentityOutcome {
    let Some(current) = current_sysid else {
        // The autopilot has not told us what it currently is. Writing an
        // identity without knowing the present one would mean rebooting an
        // autopilot to change something that might already be right.
        return IdentityOutcome::Undecided("the autopilot has not reported its system id");
    };
    match decide_identity(configured_slot(cfg), current, armed) {
        IdentityDecision::AlreadyCorrect => IdentityOutcome::Settled,
        IdentityDecision::NoSlot => IdentityOutcome::Undecided("no fleet slot has been issued"),
        IdentityDecision::UnusableSlot { .. } => {
            IdentityOutcome::Undecided("the issued slot cannot be a system id")
        }
        IdentityDecision::DeferredArmed { target } => IdentityOutcome::DeferredArmed { target },
        IdentityDecision::Adopt { target } => {
            if write_enabled(cfg) {
                IdentityOutcome::Written { target }
            } else {
                IdentityOutcome::WouldWrite { target }
            }
        }
    }
}

/// Report one tick's outcome.
///
/// A wanted-but-not-permitted change is logged at info rather than swallowed,
/// because the whole point of running the decision while the write is off is
/// that somebody can see what it would do.
pub fn report(outcome: &IdentityOutcome) {
    match outcome {
        IdentityOutcome::Settled => {}
        IdentityOutcome::Written { target } => tracing::warn!(
            target_system_id = target,
            param = SYSID_PARAM,
            "fc_identity_written_reboot_required"
        ),
        IdentityOutcome::WouldWrite { target } => tracing::info!(
            target_system_id = target,
            "fc_identity_change_wanted_but_writes_are_disabled"
        ),
        IdentityOutcome::DeferredArmed { target } => tracing::info!(
            target_system_id = target,
            "fc_identity_change_deferred_while_armed"
        ),
        IdentityOutcome::Undecided(why) => {
            tracing::debug!(reason = %why, "fc_identity_undecided")
        }
        IdentityOutcome::WriteFailed { target } => tracing::error!(
            target_system_id = target,
            "fc_identity_write_refused_by_autopilot"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(slot: u64, enabled: bool) -> Value {
        json!({
            "video": { "wfb": { "fleet_slot": slot } },
            "mavlink": { "adopt_slot_system_id": enabled },
        })
    }

    #[test]
    fn writes_are_off_unless_switched_on() {
        // The default has to be off: acting means rebooting an autopilot, and
        // nobody has watched this run against a real airframe yet.
        assert!(!write_enabled(&json!({})));
        assert!(!write_enabled(&json!({"mavlink": {}})));
        assert!(!write_enabled(&cfg(2, false)));
        assert!(write_enabled(&cfg(2, true)));
    }

    #[test]
    fn an_unset_slot_reads_as_absent_not_as_slot_zero() {
        // Zero is what the config ships with, and zero is not issuable.
        assert_eq!(configured_slot(&json!({})), None);
        assert_eq!(configured_slot(&cfg(0, true)), None);
        assert_eq!(configured_slot(&cfg(3, true)), Some(3));
    }

    #[test]
    fn a_change_is_reported_even_when_the_write_is_off() {
        // The reason the decision runs while switched off: an operator can see
        // what it would do before letting it.
        assert_eq!(
            decide_tick(&cfg(2, false), Some(1), false),
            IdentityOutcome::WouldWrite { target: 2 }
        );
    }

    #[test]
    fn a_change_is_performed_once_the_write_is_on() {
        assert_eq!(
            decide_tick(&cfg(2, true), Some(1), false),
            IdentityOutcome::Written { target: 2 }
        );
    }

    #[test]
    fn an_armed_aircraft_is_deferred_even_with_writes_enabled() {
        // The safety rule outranks the operator's switch: the change needs a
        // reboot, and the motors are live.
        assert_eq!(
            decide_tick(&cfg(2, true), Some(1), true),
            IdentityOutcome::DeferredArmed { target: 2 }
        );
    }

    #[test]
    fn an_aircraft_already_correct_is_silent_in_every_configuration() {
        for enabled in [false, true] {
            for armed in [false, true] {
                assert_eq!(
                    decide_tick(&cfg(2, enabled), Some(2), armed),
                    IdentityOutcome::Settled,
                    "enabled={enabled} armed={armed}"
                );
            }
        }
    }

    #[test]
    fn an_unknown_current_identity_decides_nothing() {
        // Writing without knowing the present value would mean rebooting an
        // autopilot to change something that may already be right.
        assert!(matches!(
            decide_tick(&cfg(2, true), None, false),
            IdentityOutcome::Undecided(_)
        ));
    }

    #[test]
    fn an_aircraft_with_no_slot_decides_nothing_even_with_writes_on() {
        assert!(matches!(
            decide_tick(&cfg(0, true), Some(1), false),
            IdentityOutcome::Undecided(_)
        ));
    }

    #[test]
    fn reporting_every_outcome_is_infallible() {
        // The reporter runs on a loop that must not be the thing that dies.
        for o in [
            IdentityOutcome::Settled,
            IdentityOutcome::Written { target: 2 },
            IdentityOutcome::WouldWrite { target: 2 },
            IdentityOutcome::DeferredArmed { target: 2 },
            IdentityOutcome::Undecided("x"),
            IdentityOutcome::WriteFailed { target: 2 },
        ] {
            report(&o);
        }
    }
}
