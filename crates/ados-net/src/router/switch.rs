//! Uplink switching, route application, and the failover control tick.

use super::*;

impl UplinkRouter {
    pub(super) fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Publish a `health_changed` event and reconcile the active-uplink flag to
    /// the new reachability. The flag's presence drives mesh gateway election,
    /// so a reachability transition is a flag-sync point.
    pub(super) async fn publish_health_change(
        &self,
        active: Option<&str>,
        available: Vec<String>,
        reachable: bool,
    ) {
        self.active_flag
            .lock()
            .await
            .sync_async(active, reachable)
            .await;
        self.bus.publish(UplinkEvent {
            kind: UplinkEventKind::HealthChanged,
            active_uplink: active.map(|s| s.to_string()),
            available,
            internet_reachable: reachable,
            data_cap_state: None,
            timestamp_ms: self.now_ms(),
        });
    }

    /// Apply a switch to the in-memory state and hand back the effects it
    /// implies.
    ///
    /// Synchronous by construction: this is the whole of the state critical
    /// section, and the route-apply / flag-write / event-publish that follow are
    /// deliberately *not* part of it — see [`commit_switch`](Self::commit_switch).
    pub(super) fn record_switch(
        st: &mut RouterState,
        uplink: Option<String>,
        available: Vec<String>,
    ) -> SwitchEffect {
        let previous = st.active_uplink.clone();
        st.active_uplink = uplink.clone();
        st.fail_streak = 0;
        st.success_streak = 0;
        st.last_switch_at = Some(Instant::now());
        SwitchEffect {
            previous,
            uplink,
            available,
            reachable: st.internet_reachable,
        }
    }

    /// Re-program the default route, reconcile the active-uplink flag, and
    /// publish an `uplink_changed` event. The I/O half of `_switch_to`.
    ///
    /// Runs with the router state mutex **released**. It used to run with the
    /// guard held, because `_switch_to` took `st: &mut RouterState`: the manager
    /// gateway lookup, the `ip route replace` spawn and the flag write all held
    /// the one lock every other router operation needs — `active_iface`,
    /// `get_state`, `set_data_cap_state`, the next tick. On the failover path
    /// that meant the lock was held longest at the worst possible moment, when
    /// the link had already dropped. Everything this needs from the state is
    /// carried in the [`SwitchEffect`] instead, so nothing here touches it.
    pub(super) async fn commit_switch(&self, effect: SwitchEffect) -> bool {
        let SwitchEffect {
            previous,
            uplink,
            available,
            reachable,
        } = effect;

        let route_ok = match &uplink {
            Some(name) => self.apply_route(name).await,
            None => true,
        };

        // Presence of the flag is the mesh gateway-election signal: write it on
        // a switch to a real uplink, unlink it on a switch to None.
        self.active_flag
            .lock()
            .await
            .sync_async(uplink.as_deref(), reachable)
            .await;

        info!(
            previous = ?previous,
            current = ?uplink,
            available = ?available,
            "uplink.switched"
        );
        self.bus.publish(UplinkEvent {
            kind: UplinkEventKind::UplinkChanged,
            active_uplink: uplink,
            available,
            internet_reachable: reachable,
            data_cap_state: None,
            timestamp_ms: self.now_ms(),
        });
        route_ok
    }

    /// Program the default route for uplink `name` and record whether it took.
    pub(super) async fn apply_route(&self, name: &str) -> bool {
        let iface = self
            .uplink_iface(name)
            .await
            .unwrap_or_else(|| name.to_string());
        let gateway = self.uplink_gateway(name).await;
        let ok = self.route.apply(&iface, gateway.as_deref()).await;
        self.state.lock().await.route_applied = ok;
        ok
    }

    /// Re-apply the active uplink's route when the last apply failed or the
    /// installed default no longer names its interface (a link flap, or
    /// another tool rewriting the table).
    pub(super) async fn verify_route(&self, name: &str) {
        let applied = self.state.lock().await.route_applied;
        let want = self
            .uplink_iface(name)
            .await
            .unwrap_or_else(|| name.to_string());
        if applied && self.route.installed_iface().await.as_deref() == Some(want.as_str()) {
            return;
        }
        if self.apply_route(name).await {
            info!(uplink = %name, iface = %want, "uplink.route_reapplied");
        } else {
            warn!(uplink = %name, iface = %want, "uplink.route_reapply_failed_retrying");
        }
    }

    /// Switch the active uplink and run the side effects it implies. Ports
    /// `_switch_to`, split across the lock boundary: record under the guard,
    /// commit without it. Callers must NOT hold the state lock.
    pub(super) async fn switch_to(&self, uplink: Option<String>, available: Vec<String>) -> bool {
        let effect = {
            let mut st = self.state.lock().await;
            Self::record_switch(&mut st, uplink, available)
        };
        self.commit_switch(effect).await
    }

    /// One control-loop iteration. Ports `_tick`.
    ///
    /// The Python original held one lock for the whole tick, and so did this
    /// until the route apply became a bounded async spawn. It now takes the state
    /// mutex only for the synchronous decision points and releases it across
    /// every `.await`: the manager reads, the cloud probe, the route apply, the
    /// flag write. `tick` has a single driver (the daemon's health interval), so
    /// what that interleaving admits is only the short state readers — which are
    /// exactly what a wedged `ip route replace` used to block. Effect ordering
    /// inside the tick is unchanged: a switch is still committed before the
    /// uplink it selected is probed.
    pub async fn tick(&self) {
        // Pick up an operator priority change (`PUT /network/priority` rewrote
        // the file) before evaluating viability, so a new failover order takes
        // effect this very tick. Then snapshot the order once so this whole
        // iteration sees a consistent list even if the file is rewritten again
        // mid-tick.
        self.reload_priority().await;
        let priority = self.priority_snapshot();

        // Viability reads the managers only — it never touches router state — so
        // it runs before the lock is taken. It used to run under it, which meant
        // every manager `is_up` (a carrier read, or an `ip`/`nmcli` shell-out)
        // held the state mutex.
        let available = self.viable_uplinks(&priority).await;

        // No viable uplink at all. Clear state if we had one.
        if available.is_empty() {
            // One critical section decides both effects; both then execute with
            // the guard released, in the original order. The switch is recorded
            // *before* `internet_reachable` is cleared, so its `uplink_changed`
            // event still carries the pre-clear reachability the Python emitted.
            let (switch, was_reachable) = {
                let mut st = self.state.lock().await;
                let switch = if st.active_uplink.is_some() {
                    Some(Self::record_switch(&mut st, None, Vec::new()))
                } else {
                    None
                };
                let was_reachable = st.internet_reachable;
                st.internet_reachable = false;
                (switch, was_reachable)
            };
            if let Some(effect) = switch {
                self.commit_switch(effect).await;
            }
            if was_reachable {
                self.publish_health_change(None, Vec::new(), false).await;
            } else {
                // Even with no prior reachability, ensure the flag is gone when
                // there is no uplink, so mesh election never sees a stale flag.
                self.active_flag.lock().await.sync_async(None, false).await;
            }
            return;
        }

        // First-time pick: highest-priority viable uplink.
        let first_pick = {
            let mut st = self.state.lock().await;
            if st.active_uplink.is_none() {
                Some(Self::record_switch(
                    &mut st,
                    Some(available[0].clone()),
                    available.clone(),
                ))
            } else {
                None
            }
        };
        let picked = first_pick.is_some();
        if let Some(effect) = first_pick {
            self.commit_switch(effect).await;
        }

        // The active uplink vanished from the viable set (unplugged, carrier
        // lost): move to the best viable one now. No probe streak or cooldown
        // applies, because there is nothing left to probe on the old link.
        let active = self.state.lock().await.active_uplink.clone();
        if let Some(name) = active {
            if !available.contains(&name) {
                warn!(from_uplink = %name, to_uplink = %available[0], "uplink.active_vanished");
                self.switch_to(Some(available[0].clone()), available.clone())
                    .await;
            } else if !picked {
                self.verify_route(&name).await;
            }
        }

        // Probe the current uplink. The name is cloned out and the guard dropped
        // before the iface lookup and the probe, both of which are I/O.
        let current = {
            let st = self.state.lock().await;
            st.active_uplink.clone().unwrap_or_default()
        };
        let iface = self.uplink_iface(&current).await;
        let ok = self.prober.probe(iface.as_deref()).await;

        let last_switch_at = self.state.lock().await.last_switch_at;
        let cooldown_ok = match last_switch_at {
            Some(t) => t.elapsed().as_secs_f64() >= failover::SWITCH_COOLDOWN_SECONDS,
            None => true,
        };

        if ok {
            self.handle_probe_success(&priority, available, cooldown_ok)
                .await;
        } else {
            self.handle_probe_failure(&priority, available, cooldown_ok)
                .await;
        }
    }

    /// Ports `_handle_probe_success`.
    ///
    /// Takes no state guard: it acquires the mutex for two short synchronous
    /// critical sections and releases it across the reachability publish, the
    /// candidate probe and the switch — all I/O.
    pub(super) async fn handle_probe_success(
        &self,
        priority: &[String],
        available: Vec<String>,
        cooldown_ok: bool,
    ) {
        let (recovered, active) = {
            let mut st = self.state.lock().await;
            st.fail_streak = 0;
            let recovered = !st.internet_reachable;
            if recovered {
                st.internet_reachable = true;
            }
            (recovered, st.active_uplink.clone())
        };
        if recovered {
            info!(uplink = ?active, "uplink.health_recovered");
            self.publish_health_change(active.as_deref(), available.clone(), true)
                .await;
        }

        let active = match (cooldown_ok, active) {
            (true, Some(a)) => a,
            _ => return,
        };

        // `select_higher_priority` is pure, so the streak bookkeeping and the
        // selection that gates it share one critical section with no await in it.
        let candidate = {
            let mut st = self.state.lock().await;
            let higher = failover::select_higher_priority(priority, &available, Some(&active));
            if higher.is_empty() {
                st.success_streak = 0;
                return;
            }
            st.success_streak += 1;
            if st.success_streak < failover::SUCCESS_UP_THRESHOLD {
                return;
            }
            higher[0].clone()
        };

        // Probe the higher-priority uplink before switching up so we do not
        // drop off a working link for a dead one.
        let cand_iface = self
            .uplink_iface(&candidate)
            .await
            .unwrap_or_else(|| candidate.clone());
        if self.prober.probe(Some(&cand_iface)).await {
            self.switch_to(Some(candidate), available).await;
        }
    }

    /// Ports `_handle_probe_failure`.
    ///
    /// The whole decision — streak bookkeeping, the pure failover selection, the
    /// reachability clear — is one synchronous critical section, and the effect it
    /// produces is awaited with the guard released. This is the uplink-failover
    /// path, so it is the one that most needs the lock back before `ip` runs.
    pub(super) async fn handle_probe_failure(
        &self,
        priority: &[String],
        available: Vec<String>,
        cooldown_ok: bool,
    ) {
        let outcome = {
            let mut st = self.state.lock().await;
            st.success_streak = 0;
            st.fail_streak += 1;
            if st.fail_streak < failover::FAIL_DOWN_THRESHOLD {
                return;
            }
            if !cooldown_ok {
                return;
            }

            let next_uplink =
                failover::select_failover_target(priority, &available, st.active_uplink.as_deref());
            match next_uplink {
                None => {
                    // Only the current (failing) uplink is available.
                    if !st.internet_reachable {
                        return;
                    }
                    st.internet_reachable = false;
                    FailoverOutcome::LostReachability(st.active_uplink.clone())
                }
                Some(next_uplink) => {
                    warn!(
                        from_uplink = ?st.active_uplink,
                        to_uplink = next_uplink,
                        fail_streak = st.fail_streak,
                        "uplink.failover"
                    );
                    // Cleared before the switch is recorded, so the
                    // `uplink_changed` event carries `internet_reachable: false`
                    // exactly as it did before.
                    st.internet_reachable = false;
                    FailoverOutcome::Switch(Self::record_switch(
                        &mut st,
                        Some(next_uplink),
                        available.clone(),
                    ))
                }
            }
        };

        match outcome {
            FailoverOutcome::LostReachability(active) => {
                self.publish_health_change(active.as_deref(), available, false)
                    .await;
            }
            FailoverOutcome::Switch(effect) => {
                self.commit_switch(effect).await;
            }
        }
    }
}
