//! Bring the radio onto a fleet address written to config, never in flight.
//!
//! The radio reads `video.wfb.fleet_id` / `fleet_slot` when it comes up and
//! never again while it runs, so a slot the ground station's reconciler
//! delivers (`PUT /api/config`) changed only the file: the drone kept
//! transmitting on its old slot while the ground listened on the new one, and
//! nothing converged until a reboot. That is the path an auto-paired drone
//! takes off its provisional first slot, so it has to take effect.
//!
//! Taking effect means restarting the radio unit, which drops the video,
//! telemetry and command link for a few seconds. So the config write only
//! records that a restart is owed (a sidecar under the run directory), and the
//! applier here performs it when the vehicle is not treated as armed (the same
//! judgement the armed interlock uses: a missing or stale vehicle state counts
//! as armed). While armed the restart waits and the sidecar says so, which the
//! full status surfaces as `radioFleetApply`.
//!
//! The sidecar is the pending record as well as the status: it survives this
//! process restarting, and a reboot needs no record because the radio then
//! comes up on the file anyway.

use std::future::Future;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use ados_protocol::armed::vehicle_treated_as_armed;
use serde_json::{json, Value};

use crate::state::AppState;

/// The sidecar file name under the run directory.
pub const SIDECAR_FILE: &str = "radio-fleet-apply.json";

/// Version of the sidecar body.
pub const SIDECAR_VERSION: u32 = 1;

/// How often an owed restart is re-judged against the armed state. Short, so
/// the link moves within seconds of landing; the steady state reads one small
/// file.
pub const APPLY_INTERVAL: Duration = Duration::from_secs(2);

/// Where an owed radio restart stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Recorded by the config write, not yet judged.
    Pending,
    /// Waiting for the vehicle to disarm.
    DeferredArmed,
    /// The restart was issued; the radio is on the written address.
    Applied,
    /// `systemctl` refused or timed out; retried on the next tick.
    RestartFailed,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::DeferredArmed => "deferred_armed",
            Self::Applied => "applied",
            Self::RestartFailed => "restart_failed",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        [
            Self::Pending,
            Self::DeferredArmed,
            Self::Applied,
            Self::RestartFailed,
        ]
        .into_iter()
        .find(|o| o.label() == label)
    }

    /// Whether a restart is still owed.
    fn owed(self) -> bool {
        self != Self::Applied
    }
}

/// What one tick does with the record on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing is owed.
    Idle,
    /// A restart is owed but the vehicle is (treated as) armed.
    Defer,
    /// A restart is owed and the vehicle is disarmed.
    Restart,
}

/// Pure over its inputs so both paths are testable without a radio.
pub fn decide(on_disk: Option<Outcome>, armed: bool) -> Action {
    match on_disk {
        Some(o) if o.owed() => {
            if armed {
                Action::Defer
            } else {
                Action::Restart
            }
        }
        _ => Action::Idle,
    }
}

/// The radio unit that reads the fleet address on this profile.
pub fn radio_unit(ground_station: bool) -> &'static str {
    if ground_station {
        ados_supervisor::bind::ADOS_WFB_GS_UNIT
    } else {
        ados_supervisor::bind::ADOS_WFB_DRONE_UNIT
    }
}

/// Serialises every read-modify-write of the sidecar in this process: the
/// config write records a new address while the applier may be finishing the
/// previous one, and the later address must never be marked applied by the
/// earlier restart.
static SIDECAR_LOCK: Mutex<()> = Mutex::new(());

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read(path: &Path) -> Option<Value> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object)
}

fn write(path: &Path, body: &Value) {
    let bytes = match serde_json::to_vec(body) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "radio_fleet_apply_sidecar_encode_failed");
            return;
        }
    };
    if let Err(e) = ados_protocol::sidecar::write_durable(path, &bytes) {
        tracing::warn!(path = %path.display(), error = %e, "radio_fleet_apply_sidecar_write_failed");
    }
}

fn outcome_of(body: &Value) -> Option<Outcome> {
    body.get("outcome")
        .and_then(Value::as_str)
        .and_then(Outcome::from_label)
}

/// The address a record asks for, the identity a later record is compared by.
fn address(body: &Value) -> (Option<u64>, Option<u64>, Option<u64>) {
    (
        body.get("fleet_id").and_then(Value::as_u64),
        body.get("fleet_slot").and_then(Value::as_u64),
        body.get("since_ms").and_then(Value::as_u64),
    )
}

/// Record that `unit` must restart onto `fleet_id` / `fleet_slot`. Called by
/// the config write once the new address is on disk.
pub fn mark_pending(run_dir: &Path, fleet_id: u16, fleet_slot: u8, unit: &str) {
    let _held = SIDECAR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let now = now_ms();
    write(
        &run_dir.join(SIDECAR_FILE),
        &json!({
            "version": SIDECAR_VERSION,
            "outcome": Outcome::Pending.label(),
            "fleet_id": fleet_id,
            "fleet_slot": fleet_slot,
            "unit": unit,
            "since_ms": now,
            "at_ms": now,
        }),
    );
}

/// The record as status reads it, null when nothing was ever owed.
pub fn read_status(run_dir: &Path) -> Value {
    read(&run_dir.join(SIDECAR_FILE)).unwrap_or(Value::Null)
}

/// One tick: judge the owed restart against `armed`, perform it through
/// `restart` (which issues the unit restart and reports success), and record
/// the result. Returns the record written, `None` when nothing is owed.
pub async fn apply_tick<F, Fut>(run_dir: &Path, armed: bool, restart: F) -> Option<Value>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = bool>,
{
    let path = run_dir.join(SIDECAR_FILE);
    let seen = {
        let _held = SIDECAR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        read(&path)?
    };
    let outcome = match decide(outcome_of(&seen), armed) {
        Action::Idle => return None,
        Action::Defer => Outcome::DeferredArmed,
        Action::Restart => {
            let unit = seen
                .get("unit")
                .and_then(Value::as_str)
                .unwrap_or(ados_supervisor::bind::ADOS_WFB_DRONE_UNIT)
                .to_string();
            if restart(unit).await {
                Outcome::Applied
            } else {
                Outcome::RestartFailed
            }
        }
    };
    let _held = SIDECAR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let current = read(&path)?;
    if address(&current) != address(&seen) {
        // A newer address was written while this one was being applied: it
        // stays owed, and the next tick restarts the radio onto it.
        return None;
    }
    if outcome_of(&current) == Some(outcome) {
        // Restating an unchanged deferral would rewrite the file every tick.
        return Some(current);
    }
    let mut next = current;
    next["outcome"] = json!(outcome.label());
    next["at_ms"] = json!(now_ms());
    match outcome {
        Outcome::DeferredArmed => tracing::info!(
            fleet_id = ?next.get("fleet_id"),
            fleet_slot = ?next.get("fleet_slot"),
            "radio_fleet_restart_deferred_while_armed"
        ),
        Outcome::Applied => tracing::info!(
            fleet_id = ?next.get("fleet_id"),
            fleet_slot = ?next.get("fleet_slot"),
            "radio_fleet_restart_applied"
        ),
        Outcome::RestartFailed => tracing::warn!("radio_fleet_restart_failed; retrying"),
        Outcome::Pending => {}
    }
    write(&path, &next);
    Some(next)
}

/// Run the applier for the life of the process. Vehicle profiles only (drone
/// and ground station): those are the nodes whose radio carries a flying
/// aircraft's link.
pub fn spawn_applier(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(APPLY_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let armed = vehicle_treated_as_armed(state.state.snapshot_at());
            apply_tick(&state.run_dir, armed, |unit| async move {
                crate::probe::status_only(
                    "systemctl",
                    &["--no-block", "try-restart", &unit],
                    crate::probe::PROBE_TIMEOUT,
                )
                .await
            })
            .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn counting() -> (Arc<AtomicU32>, impl Fn(String) -> std::future::Ready<bool>) {
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        (calls, move |unit: String| {
            assert_eq!(unit, "ados-wfb.service");
            c.fetch_add(1, Ordering::SeqCst);
            std::future::ready(true)
        })
    }

    #[test]
    fn an_owed_restart_waits_for_disarm_and_nothing_owed_is_idle() {
        for owed in [
            Outcome::Pending,
            Outcome::DeferredArmed,
            Outcome::RestartFailed,
        ] {
            assert_eq!(decide(Some(owed), true), Action::Defer, "{owed:?}");
            assert_eq!(decide(Some(owed), false), Action::Restart, "{owed:?}");
        }
        for armed in [false, true] {
            assert_eq!(decide(Some(Outcome::Applied), armed), Action::Idle);
            assert_eq!(decide(None, armed), Action::Idle);
        }
    }

    #[tokio::test]
    async fn an_armed_vehicle_keeps_its_radio_and_the_status_says_why() {
        let dir = tempfile::tempdir().unwrap();
        mark_pending(dir.path(), 1, 3, "ados-wfb.service");
        let (calls, restart) = counting();

        let body = apply_tick(dir.path(), true, &restart).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0, "no restart while armed");
        assert_eq!(body["outcome"], "deferred_armed");
        assert_eq!(read_status(dir.path())["outcome"], "deferred_armed");
        assert_eq!(read_status(dir.path())["fleet_slot"], 3);
        // Still owed on the next armed tick, still no restart.
        apply_tick(dir.path(), true, &restart).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_disarmed_vehicle_restarts_its_radio_once() {
        let dir = tempfile::tempdir().unwrap();
        mark_pending(dir.path(), 1, 3, "ados-wfb.service");
        let (calls, restart) = counting();

        // Deferred while armed, then applied on the first disarmed tick.
        apply_tick(dir.path(), true, &restart).await;
        let body = apply_tick(dir.path(), false, &restart).await.unwrap();
        assert_eq!(body["outcome"], "applied");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Nothing owed afterwards, armed or not.
        assert!(apply_tick(dir.path(), false, &restart).await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_restart_stays_owed() {
        let dir = tempfile::tempdir().unwrap();
        mark_pending(dir.path(), 1, 3, "ados-wfb.service");
        let body = apply_tick(dir.path(), false, |_| std::future::ready(false))
            .await
            .unwrap();
        assert_eq!(body["outcome"], "restart_failed");
        let (calls, restart) = counting();
        apply_tick(dir.path(), false, &restart).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "retried");
    }

    #[tokio::test]
    async fn an_address_written_during_a_restart_is_not_marked_applied() {
        let dir = tempfile::tempdir().unwrap();
        mark_pending(dir.path(), 1, 3, "ados-wfb.service");
        let run_dir = dir.path().to_path_buf();
        // The config write lands slot 4 while slot 3's restart is in flight.
        let body = apply_tick(dir.path(), false, |_| {
            std::thread::sleep(Duration::from_millis(2));
            mark_pending(&run_dir, 1, 4, "ados-wfb.service");
            std::future::ready(true)
        })
        .await;
        assert!(body.is_none());
        let on_disk = read_status(dir.path());
        assert_eq!(on_disk["fleet_slot"], 4);
        assert_eq!(
            on_disk["outcome"], "pending",
            "the later address stays owed"
        );
    }

    #[test]
    fn each_profile_restarts_its_own_radio_unit() {
        assert_eq!(radio_unit(false), "ados-wfb.service");
        assert_eq!(radio_unit(true), "ados-wfb-rx.service");
    }
}
