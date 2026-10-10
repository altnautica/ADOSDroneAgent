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
//! Acting means writing the system-id parameter (`SYSID_THISMAV`, or
//! `MAV_SYSID` on ArduPilot 4.7+) to the autopilot, and on the firmware
//! we target that does not take effect until the autopilot reboots. So the
//! whole feature is one config flag away from doing nothing, and the flag is
//! off. That is not caution for its own sake: nobody has yet watched this run
//! against a real airframe, and an autopilot reboot is not a thing to discover
//! the edges of in the field.
//!
//! A reconciler ([`spawn_reconciler`]) evaluates the decision every
//! [`IDENTITY_RECONCILE_INTERVAL`] whether or not the write is allowed, and
//! records the outcome in `/run/ados/fc-identity.json`, so an operator can see
//! what would happen before letting it happen. It writes only when the flag is
//! on and the vehicle is not treated as armed, through the same PARAM_SET path
//! the parameter route uses. It is the only writer of the identity parameter:
//! the REST parameter route refuses it.
//!
//! ## Why it never reboots the autopilot itself
//!
//! It writes the parameter and says a reboot is required. It does not command
//! one. A parameter write is reversible by another write; a reboot is a
//! discontinuity in a vehicle's control, and choosing its moment belongs to
//! whoever can see the aircraft.

use std::path::Path;
use std::time::Duration;

use ados_groundlink::fleet_identity_policy::{decide_identity, IdentityDecision};
use ados_protocol::armed::vehicle_treated_as_armed;
use serde_json::{json, Value};

use crate::state::AppState;

/// Version of the `/run/ados/fc-identity.json` sidecar body.
pub const SIDECAR_VERSION: u32 = 1;

/// The sidecar file name under the run directory.
const SIDECAR_FILE: &str = "fc-identity.json";

/// How often the aircraft's identity is reconciled.
///
/// Slow on purpose. A slot changes at pair time and an identity follows it, so
/// this catches a boot or a re-pair rather than anything faster, and the
/// steady state must cost nothing on a companion that has flying to do.
pub const IDENTITY_RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

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
            param = ?crate::fc_target::SYSID_PARAMS,
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

/// Start the identity reconciler on the current runtime.
///
/// Ticks every [`IDENTITY_RECONCILE_INTERVAL`]; each tick decides, writes when
/// permitted, and records the outcome in `<run_dir>/fc-identity.json`, the same
/// run directory full status reads it back from.
pub fn spawn_reconciler(state: AppState) {
    tokio::spawn(async move {
        let path = state.run_dir.join(SIDECAR_FILE);
        let mut interval = tokio::time::interval(IDENTITY_RECONCILE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            reconcile_tick(&state, &path).await;
        }
    });
}

/// The node config as a JSON value; an absent or unreadable file reads empty,
/// which leaves the write switched off.
fn read_config(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_norway::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null)
}

/// The system id the autopilot currently carries: the id latched from its
/// HEARTBEAT on a fresh snapshot, else the cached system-id parameter.
fn current_system_id(state: &AppState, snapshot: Option<&Value>) -> Option<u8> {
    snapshot
        .and_then(|s| s.get("fc_system_id"))
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .filter(|&v| v != 0)
        .or_else(|| {
            crate::fc_target::cached_sysid_param(&state.params_path)
                .1
                .filter(|v| v.fract() == 0.0 && (1.0..=255.0).contains(v))
                .map(|v| v as u8)
        })
}

/// The system id the decision wants the autopilot to carry, when it has one.
fn wanted_system_id(outcome: &IdentityOutcome, current: Option<u8>) -> Option<u8> {
    match outcome {
        IdentityOutcome::Settled => current,
        IdentityOutcome::Written { target }
        | IdentityOutcome::WouldWrite { target }
        | IdentityOutcome::DeferredArmed { target }
        | IdentityOutcome::WriteFailed { target } => Some(*target),
        IdentityOutcome::Undecided(_) => None,
    }
}

/// Run one reconcile tick and record its outcome in the sidecar at `path`.
/// Returns the recorded body.
async fn reconcile_tick(state: &AppState, path: &Path) -> Value {
    let cfg = read_config(&state.pairing_paths.config);
    let enabled = write_enabled(&cfg);
    let snapshot_at = state.state.snapshot_at();
    let armed = vehicle_treated_as_armed(snapshot_at.clone());
    let current = current_system_id(state, snapshot_at.as_ref().map(|(_, v)| v));

    let decided = decide_tick(&cfg, current, armed);
    let wanted = wanted_system_id(&decided, current);
    let (outcome, label) = if !enabled {
        (decided, "disabled")
    } else if armed {
        (decided, "deferred_armed")
    } else {
        match decided {
            IdentityOutcome::Settled => (IdentityOutcome::Settled, "in_sync"),
            IdentityOutcome::Written { target } => {
                // Write whichever spelling this firmware's cache holds.
                let (param, _) = crate::fc_target::cached_sysid_param(&state.params_path);
                match crate::routes::params_write::send_param_set(state, param, f64::from(target))
                    .await
                {
                    Ok((true, _)) => (IdentityOutcome::Written { target }, "written"),
                    Ok((false, _)) | Err(_) => (IdentityOutcome::WriteFailed { target }, "error"),
                }
            }
            other => (other, "error"),
        }
    };
    report(&outcome);

    let body = json!({
        "version": SIDECAR_VERSION,
        "enabled": enabled,
        "outcome": label,
        "wanted_system_id": wanted,
        "current_system_id": current,
        "at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    match serde_json::to_vec(&body) {
        Ok(bytes) => {
            if let Err(e) = ados_protocol::sidecar::write_durable(path, &bytes) {
                tracing::warn!(path = %path.display(), error = %e, "fc_identity_sidecar_write_failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "fc_identity_sidecar_encode_failed"),
    }
    body
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

    /// An `AppState` under `dir` whose config carries slot 2 and the write
    /// flag `enabled`; the MAVLink socket is absent, so any PARAM_SET attempt
    /// would surface as an `error` outcome.
    fn state_with_config(dir: &std::path::Path, enabled: bool) -> AppState {
        use crate::auth::PairingState;
        use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
        use crate::state::PairingPaths;
        use std::sync::Arc;
        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            format!(
                "video:\n  wfb:\n    fleet_slot: 2\nmavlink:\n  adopt_slot_system_id: {enabled}\n"
            ),
        )
        .unwrap();
        let pairing_paths = PairingPaths {
            config,
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
            profile_conf: dir.join("profile.conf"),
            mesh_role: dir.join("mesh-role"),
            relay_secret: dir.join("relay-peer-secret"),
        };
        AppState::new(
            Arc::new(PairingState::with_path(dir.join("pairing.json"))),
            StateIpcClient::disconnected(),
            MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
            LogdQueryClient::new(dir.join("absent-logd.sock")),
            dir.join("board.json"),
            pairing_paths,
            Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.join("dashboard-pin.json"),
            )),
            Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.join("mcp-token.json"),
            )),
        )
        .with_params_path(dir.join("params.json"))
    }

    fn read_sidecar(path: &std::path::Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn reconciler_defers_while_armed_and_writes_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_config(dir.path(), true);
        state.state.set_snapshot_for_test(json!({
            "armed": true,
            "fc_connected": true,
            "fc_system_id": 1,
            "fc_component_id": 1,
        }));
        let path = dir.path().join("fc-identity.json");
        let body = reconcile_tick(&state, &path).await;
        assert_eq!(
            body["outcome"], "deferred_armed",
            "no PARAM_SET while armed"
        );
        let on_disk = read_sidecar(&path);
        assert_eq!(on_disk["version"], SIDECAR_VERSION);
        assert_eq!(on_disk["enabled"], true);
        assert_eq!(on_disk["outcome"], "deferred_armed");
        assert_eq!(on_disk["wanted_system_id"], 2);
        assert_eq!(on_disk["current_system_id"], 1);
        assert!(on_disk["at_ms"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn disabled_flag_reports_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_config(dir.path(), false);
        state.state.set_snapshot_for_test(json!({
            "armed": false,
            "fc_system_id": 1,
            "fc_component_id": 1,
        }));
        let path = dir.path().join("fc-identity.json");
        reconcile_tick(&state, &path).await;
        let on_disk = read_sidecar(&path);
        assert_eq!(on_disk["enabled"], false);
        assert_eq!(on_disk["outcome"], "disabled");
        assert_eq!(on_disk["wanted_system_id"], 2);
    }

    #[tokio::test]
    async fn an_unknown_identity_reports_null_rather_than_a_guess() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_config(dir.path(), false);
        let path = dir.path().join("fc-identity.json");
        reconcile_tick(&state, &path).await;
        let on_disk = read_sidecar(&path);
        assert_eq!(on_disk["current_system_id"], Value::Null);
        assert_eq!(on_disk["wanted_system_id"], Value::Null);
    }

    #[tokio::test]
    async fn cached_system_id_is_read_under_either_firmware_spelling() {
        for name in crate::fc_target::SYSID_PARAMS {
            let dir = tempfile::tempdir().unwrap();
            let state = state_with_config(dir.path(), false);
            let cache = json!({ name: { "value": 1 } }).to_string();
            std::fs::write(dir.path().join("params.json"), cache).unwrap();
            let path = dir.path().join("fc-identity.json");
            reconcile_tick(&state, &path).await;
            assert_eq!(read_sidecar(&path)["current_system_id"], 1, "{name}");
        }
    }
}
