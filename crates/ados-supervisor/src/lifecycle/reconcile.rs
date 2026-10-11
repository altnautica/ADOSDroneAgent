//! Service start/stop/restart and the per-unit reconcile pass.

use super::*;

impl Supervisor {
    /// Start a unit, honoring profile/role gates. A crash-looping unit is
    /// started like any other: the label never stretches the retry cadence.
    /// Returns true only when the unit reached `active`.
    ///
    /// A gated-off row goes to `Stopped` (reason `gated_off`), so a Failed or
    /// CrashLooping row whose gate closed (a role switch) leaves the parked
    /// retry set instead of being re-tried forever. A start that systemd
    /// skipped on an unmet unit condition (`ConditionPathExists` and kin exit
    /// the start verb 0) goes to `Idle` (reason `condition_unmet`) with no
    /// failure recorded; the config-change wake path tries it again.
    pub async fn start_service(&mut self, name: &str) -> bool {
        let Some(i) = self.index_of(name) else {
            tracing::warn!(service = name, "unknown service");
            return false;
        };

        if !gate_allows(&self.services[i], &self.config) {
            if self.services[i].state != ServiceState::Stopped {
                tracing::info!(service = name, "service gated off for this profile/role");
                self.work_proof.forget(name);
                self.set_state(i, ServiceState::Stopped, "gated_off");
            }
            return false;
        }

        self.set_state(i, ServiceState::Starting, "start_requested");
        // Clear any prior failed / start-limit-hit state so `start` is not a
        // no-op on a unit that crash-looped past the start-limit burst.
        self.pm.reset_failed(name).await;

        if self.pm.start(name).await {
            if self.pm.condition_result(name).await == Some(false) {
                self.set_state(i, ServiceState::Idle, "condition_unmet");
                tracing::info!(
                    service = name,
                    "service start skipped: unit condition unmet"
                );
                return false;
            }
            self.set_state(i, ServiceState::Running, "start_ok");
            tracing::info!(service = name, "service started");
            true
        } else {
            let _ = self.record_failure_and_emit(i, Instant::now(), "start_failed");
            tracing::error!(service = name, "service start failed");
            false
        }
    }

    /// Stop a unit and reset its runtime state.
    pub async fn stop_service(&mut self, name: &str) -> bool {
        let Some(i) = self.index_of(name) else {
            return false;
        };
        let ok = self.pm.stop(name).await;
        // The replacement process starts its work counter at zero, so the
        // dead one's total must not survive as a baseline to compare against.
        self.work_proof.forget(name);
        self.set_state(i, ServiceState::Stopped, "stopped");
        tracing::info!(service = name, "service stopped");
        ok
    }

    /// Stop then start, with the same brief settle the Python path uses.
    pub async fn restart_service(&mut self, name: &str) -> bool {
        self.stop_service(name).await;
        sleep(Duration::from_millis(500)).await;
        self.start_service(name).await
    }

    /// Block until none of `names` is confirmed `active`, polling at 100ms up to
    /// `timeout`. A unit whose state cannot be read counts as still up (not
    /// confirmed stopped). Returns even if some remain up (logged), so a wedged
    /// unit cannot stall the rest of shutdown.
    pub(super) async fn wait_for_stop(&self, names: &[&str], timeout: Duration) {
        if names.is_empty() {
            return;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let mut still_up = Vec::new();
            for n in names {
                if self.pm.is_active(n).await != Some(false) {
                    still_up.push(*n);
                }
            }
            if still_up.is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                tracing::warn!(services = ?still_up, "stop wait timed out");
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Full startup: apply GS role, start core, detect + start hardware.
    pub async fn start(&mut self) {
        tracing::info!("supervisor starting");

        // On a ground station, apply the configured mesh role so the sentinel,
        // systemd masks, and role-gate checks all agree before the hardware
        // pass tries to start role-gated units. No-op on a drone. Gated on the
        // RESOLVED profile, so a config that says `auto` (profile.conf decides)
        // or the hyphen spelling applies its role exactly like the literal one.
        if self.config.is_ground_station() {
            let role = self.config.configured_gs_role.clone();
            // The sentinel the gate re-reads (`mesh_role_path`, which honours
            // the `ADOS_MESH_ROLE` override), so the writer and the reader can
            // never name two different files.
            let role_path = self.config.mesh_role_path.clone();
            crate::role::apply_role_on_boot(self.pm.as_ref(), &role, &role_path).await;
        }

        // Start core units. They are independent of one another (hardware and
        // on-demand depend on core, not the reverse).
        let core: Vec<&'static str> = self
            .services
            .iter()
            .filter(|s| s.category == Category::Core)
            .map(|s| s.name)
            .collect();
        for name in core {
            self.start_service(name).await;
        }

        self.detect_and_start_hardware().await;

        // A unit this pass leaves off may still carry a `failed` result from a
        // previous process (a stop that landed mid-start). Nothing starts it
        // again to clear that, so the status surface would report a failure
        // for a unit that is meant to be off. The rows this pass started or
        // tried are not `Stopped`, and `reset-failed` leaves a running unit be.
        let left_off: Vec<&'static str> = self
            .services
            .iter()
            .filter(|s| s.state == ServiceState::Stopped)
            .map(|s| s.name)
            .collect();
        for name in left_off {
            self.pm.reset_failed(name).await;
        }
        tracing::info!(services = self.services.len(), "supervisor ready");
    }

    /// Graceful shutdown in dependency-aware tiers: HTTP frontend first (so no
    /// new requests land on dying hardware services), then hardware, on-demand,
    /// and finally the rest of core. Poll `is-active` between tiers.
    ///
    /// Tears down only the rows this process brought up. An adopted row is left
    /// to the service manager: every catalog unit is `PartOf=` the supervisor,
    /// so systemd already propagates the supervisor's stop to it, and on
    /// `systemctl restart ados-supervisor` it propagates a restart that brings
    /// the unit straight back. A `systemctl stop` from here lands after that
    /// restart and leaves the unit down for good, because the replacement
    /// process starts only its own rows and the adoption sweep only promotes a
    /// unit that is already active. That is how a supervisor restart used to take
    /// `ados-control`, the operator's `:8080`, down with every other
    /// installer-started unit. Under launchd the installer loads and unloads
    /// each LaunchAgent itself, and a `bootout` from here unloads the job with
    /// nothing to load it again.
    ///
    /// Nothing is stopped at all while systemd itself stops or restarts the
    /// supervisor: that same transaction has already stopped every `PartOf=`
    /// unit and, on a restart, started it again before SIGTERM reaches this
    /// process. A `systemctl stop` from here would hit the fresh instance, so
    /// every unit would start twice and one caught mid-`ExecStartPre` would be
    /// left `failed`.
    pub async fn stop(&mut self) {
        tracing::info!("supervisor stopping");
        if self.service_manager_cycling_self().await {
            tracing::info!("systemd is cycling the supervisor's units; leaving them to it");
            return;
        }

        // Tier 0: the API frontend stops accepting requests first.
        if self
            .index_of("ados-api")
            .is_some_and(|i| stops_on_shutdown(&self.services[i]))
        {
            self.stop_service("ados-api").await;
            self.wait_for_stop(&["ados-api"], Duration::from_secs(5))
                .await;
        }

        for category in [Category::Hardware, Category::OnDemand, Category::Core] {
            let tier: Vec<&'static str> = self
                .services
                .iter()
                .filter(|s| s.name != "ados-api" && s.category == category && stops_on_shutdown(s))
                .map(|s| s.name)
                .collect();
            for name in &tier {
                self.stop_service(name).await;
            }
            self.wait_for_stop(&tier, Duration::from_secs(5)).await;
        }
        tracing::info!("supervisor stopped");
    }

    /// The service half of a monitor pass: detect deaths and stalls, auto-
    /// restart, adopt units this process did not start, then retry every
    /// parked service whose cooldown has elapsed. Restarts are held while
    /// systemd stops or restarts the supervisor itself.
    ///
    /// Split out from [`monitor_pass`](Self::monitor_pass) so it is drivable
    /// without the network/hardware reconcilers, and stamps monitor progress
    /// per unit: a pass that walks 30-odd `systemctl` calls is advancing, and
    /// must keep the watchdog fed, while a pass stuck on any one of them is not.
    pub async fn reconcile_services(&mut self) {
        // Snapshot the names + states we need so we can issue async restarts
        // without holding an immutable borrow across the await.
        let mut to_restart: Vec<&'static str> = Vec::new();
        let now = Instant::now();
        // Latched once a death is seen while systemd cycles the supervisor, so
        // the rest of the pass holds without probing again.
        let mut held = false;

        for spec in &self.services {
            if matches!(spec.state, ServiceState::Running | ServiceState::Starting) {
                // Checked below via is_active (needs await); collect names.
                to_restart.push(spec.name);
            }
        }
        let to_retry = self.parked_retries_due(now);

        // Liveness check + auto-restart for running services.
        for name in to_restart {
            let verdict = self.pm.is_active(name).await;
            self.progress.mark();
            let Some(i) = self.index_of(name) else {
                continue;
            };
            // No verdict is not a death. A probe that timed out or could not
            // be spawned (a busy service manager, fork failing under memory
            // pressure) says nothing about the unit, and reading it as dead
            // would restart every healthy unit on the node at once.
            let Some(active) = verdict else {
                tracing::debug!(service = name, "liveness probe gave no verdict");
                continue;
            };
            if !active && self.services[i].state == ServiceState::Running {
                if held || self.service_manager_cycling_self().await {
                    // This is the supervisor's own stop or restart reaching the
                    // unit, not a failure. The process is about to get SIGTERM.
                    held = true;
                    tracing::info!(
                        service = name,
                        "unit stopped while systemd cycles the supervisor; not restarting"
                    );
                    continue;
                }
                // A unit declared to exit 0 when the node has nothing for it
                // to run (`RestartPreventExitStatus=0`) and that did exactly
                // that is idle, not dead: no failure, no restart.
                if self.services[i].nothing_to_run_ok
                    && self.pm.exited_cleanly(name).await == Some(true)
                {
                    tracing::info!(service = name, "service exited cleanly: nothing to run");
                    self.work_proof.forget(name);
                    self.set_state(i, ServiceState::Idle, "nothing_to_run");
                    continue;
                }
                tracing::warn!(service = name, "service died");
                self.work_proof.forget(name);
                let _ = self.record_failure_and_emit(i, Instant::now(), "died");
                let blocked = self.restart_blocked_by_bind(name).await;
                if self.services[i].state != ServiceState::CrashLooping && !blocked {
                    tracing::info!(service = name, "auto-restart");
                    self.start_service(name).await;
                    self.progress.mark();
                }
                continue;
            }
            // The unit is alive. For the lanes whose silence is invisible in
            // `systemctl`, that is not the same as working: ask the delta
            // counter too. Process liveness is never proof of work.
            if active
                && self.services[i].state == ServiceState::Running
                && crate::work_proof::requires_work_proof(name)
            {
                self.enforce_work_proof(name).await;
                self.progress.mark();
            }
        }

        // Adopt the rows the installer started and this process never has, so
        // they get death detection too. Bounded to its own cadence: a row only
        // needs adopting once.
        if self
            .last_adoption_sweep
            .is_none_or(|t| now.duration_since(t) >= ADOPTION_SWEEP_INTERVAL)
        {
            self.last_adoption_sweep = Some(now);
            self.adopt_active_units().await;
        }

        // Parked-service retry (bounded by the cooldown) and the one fresh start
        // an idle row gets per config change. Held, like the auto-restart above,
        // while systemd cycles the supervisor.
        let to_wake = if held {
            Vec::new()
        } else {
            self.idle_rows_due_on_config_change()
        };
        if (to_retry.is_empty() && to_wake.is_empty())
            || held
            || self.service_manager_cycling_self().await
        {
            return;
        }
        for name in to_retry {
            if self.restart_blocked_by_bind(name).await {
                continue;
            }
            if let Some(i) = self.index_of(name) {
                self.services[i].last_retry_at = Some(Instant::now());
            }
            tracing::info!(service = name, "parked retry");
            self.start_service(name).await;
            self.progress.mark();
        }
        for name in to_wake {
            if self.restart_blocked_by_bind(name).await {
                continue;
            }
            tracing::info!(
                service = name,
                "agent config changed; starting idle service"
            );
            self.start_service(name).await;
            self.progress.mark();
        }
    }
}
