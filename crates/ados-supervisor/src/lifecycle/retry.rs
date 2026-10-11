//! Parked-retry and config-change wake selection, work-proof enforcement
//! and the self-cycling guard.

use super::*;

impl Supervisor {
    /// Names of every parked service whose retry is due at `now`.
    ///
    /// Deliberately NOT filtered by category. `Category::OnDemand` used to be
    /// excluded, which let `ados-control` — the lean headless node's only HTTP
    /// surface — stay down permanently after five failures in a minute,
    /// clearable only over SSH. A unit the operator never enabled sits in
    /// `Stopped`, never `Failed`/`CrashLooping`, so retrying every parked
    /// service cannot spuriously start one: `gate_allows` plus the unit's own
    /// `ConditionPathExists` marker remain the gate. The cadence is the fixed
    /// [`PARKED_RETRY_COOLDOWN`] whatever the failure history.
    pub(super) fn parked_retries_due(&self, now: Instant) -> Vec<&'static str> {
        self.services
            .iter()
            .filter(|spec| {
                matches!(
                    spec.state,
                    ServiceState::Failed | ServiceState::CrashLooping
                ) && spec
                    .last_retry_at
                    .map(|t| now.duration_since(t) >= PARKED_RETRY_COOLDOWN)
                    .unwrap_or(true)
            })
            .map(|spec| spec.name)
            .collect()
    }

    /// Names of every idle row to start again because the agent config changed
    /// since the last look, consuming the change.
    ///
    /// A row is idle because its unit exited 0: this node had nothing for it to
    /// run under the config it read (a drone with no fleet slot yet, a feature
    /// switched off). A config change can be exactly what gives it work, and
    /// nothing else would ever start it again, so every idle row gets one fresh
    /// start per change. A unit that still has nothing to run exits 0 again and
    /// returns to idle; there is no retry cadence to loop on.
    pub(super) fn idle_rows_due_on_config_change(&mut self) -> Vec<&'static str> {
        let stamp = config_mtime(&self.config.config_yaml);
        if stamp == self.config_stamp {
            return Vec::new();
        }
        self.config_stamp = stamp;
        self.services
            .iter()
            .filter(|spec| spec.state == ServiceState::Idle)
            .map(|spec| spec.name)
            .collect()
    }

    /// Judge one active, work-proven unit on its work-counter delta and
    /// restart it when the counter has been flat across the whole stall
    /// window. Returns whether it was judged stalled.
    ///
    /// `restart`, not `start`: the process is alive, so there is something to
    /// tear down. The baseline is dropped either way — the replacement process
    /// starts its counters at zero.
    pub(super) async fn enforce_work_proof(&mut self, name: &'static str) -> bool {
        use crate::work_proof::WorkVerdict;
        let counter = self.pm.work_counter(name).await;
        let verdict = self
            .work_proof
            .observe(name, counter, tokio::time::Instant::now());
        let WorkVerdict::Stalled { flat_for } = verdict else {
            return false;
        };
        tracing::warn!(
            service = name,
            flat_for_s = flat_for.as_secs(),
            "service is active but its work counter has not moved; treating as dead"
        );
        let Some(i) = self.index_of(name) else {
            return false;
        };
        let _ = self.record_failure_and_emit(i, Instant::now(), "stalled");
        self.work_proof.forget(name);
        if self.services[i].state != ServiceState::CrashLooping
            && !self.restart_blocked_by_bind(name).await
        {
            self.restart_service(name).await;
            self.work_proof.forget(name);
        }
        true
    }

    /// Whether the service manager is stopping or restarting this supervisor
    /// right now: a job is pending on its own unit. Every catalog unit is
    /// `PartOf=` the supervisor, so systemd stops or restarts all of them in the
    /// same transaction, and a unit that goes inactive in that window was
    /// stopped on purpose. Restarting it undoes `systemctl stop ados-supervisor`
    /// (the unit outlives the supervisor), and a start queued behind the
    /// supervisor's own job (ados-logd is ordered after it) blocks the monitor
    /// pass, so SIGTERM goes unhandled until systemd's stop timeout kills the
    /// process. A start job still pending right after READY holds one pass at
    /// most. `None` (launchd, a failed probe) reads as not cycling, so the
    /// monitor keeps restarting.
    pub(super) async fn service_manager_cycling_self(&self) -> bool {
        self.pm.job_pending(SUPERVISOR_UNIT).await == Some(true)
    }
}
