//! Uplink router orchestrator.
//!
//! The ground station can reach the cloud over several uplinks: wired Ethernet
//! (`eth0`), WiFi client (`wlan0_client` when the onboard radio is in STA mode,
//! mutually exclusive with AP mode), cellular (`wwan0`), and USB tether
//! (`usb0` when a laptop shares its connection over the USB gadget). This
//! orchestrator picks exactly one as the default route based on a configured
//! priority list, and fails over automatically when the active uplink stops
//! reaching the cloud relay. Ports `uplink/router.py`.
//!
//! The hardware managers (wifi-client / ethernet / hostapd / modem), the
//! firewall, the USB-gadget surface, and the data-cap tracker land in later
//! chunks. The control-loop FSM here is exercised against [`StubManager`] and
//! injectable [`Prober`] / [`RouteApplier`] seams so the failover logic is
//! unit-testable without a NIC.

pub mod active_flag;
pub mod events;
pub mod failover;
pub mod health;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::cmd::{CmdRunner, TokioCmdRunner};
use crate::paths;
use active_flag::ActiveFlagWriter;
use events::{UplinkEvent, UplinkEventBus, UplinkEventKind};

/// Structural type every uplink manager satisfies. The router only uses
/// `is_up`, `get_iface`, and `get_gateway`. Ports `UplinkManagerProto`.
#[async_trait]
pub trait UplinkManager: Send + Sync {
    async fn is_up(&self) -> bool;
    fn get_iface(&self) -> String;
    async fn get_gateway(&self) -> Option<String>;
}

/// Inert placeholder for an unwired uplink slot. `is_up` returns `false` so the
/// stubbed uplink never passes the viability filter; a real manager replaces it
/// through the router constructor. Ports `_StubManager`.
#[derive(Debug, Clone)]
pub struct StubManager {
    iface: String,
}

impl StubManager {
    pub fn new(iface: impl Into<String>) -> Self {
        Self {
            iface: iface.into(),
        }
    }
}

#[async_trait]
impl UplinkManager for StubManager {
    async fn is_up(&self) -> bool {
        false
    }
    fn get_iface(&self) -> String {
        self.iface.clone()
    }
    async fn get_gateway(&self) -> Option<String> {
        None
    }
}

/// The cloud-reachability probe seam. The production impl forwards to
/// [`health::probe_host`]; tests inject a scripted prober.
#[async_trait]
pub trait Prober: Send + Sync {
    async fn probe(&self, iface: Option<&str>) -> bool;
}

/// Production prober: bare TCP connect to the cloud relay, bound to the iface.
#[derive(Debug, Default, Clone, Copy)]
pub struct CloudProber;

#[async_trait]
impl Prober for CloudProber {
    async fn probe(&self, iface: Option<&str>) -> bool {
        health::probe_host(iface).await
    }
}

/// The kernel-routing seam. The production impl forwards to
/// [`failover::apply_default_route`] over the shared [`CmdRunner`]; tests inject
/// a recording no-op.
///
/// `apply` is `async` because `ip route replace` is a process spawn. A sync
/// trait method is what let a blocking `std::process::Command` sit on the
/// reactor two frames below [`tick`](UplinkRouter::tick) with nothing in the
/// type system objecting; an `async` method makes that unrepresentable.
#[async_trait]
pub trait RouteApplier: Send + Sync {
    async fn apply(&self, iface: &str, gateway: Option<&str>) -> bool;
    /// The interface the router-programmed default route points at now, or
    /// `None` when it is absent or could not be read.
    async fn installed_iface(&self) -> Option<String>;
}

/// Production route applier: `ip route replace default ...` over the crate's
/// async command runner, bounded by a timeout so a wedged `ip` cannot occupy a
/// worker for the life of the process.
///
/// The runner is held rather than constructed per call, mirroring how the
/// hardware managers hold theirs. The unit-testable seam is
/// [`failover::apply_default_route`], which takes the runner as a parameter, so
/// the argv is assertable against a scripted runner with no routing table.
#[derive(Clone)]
pub struct IpRouteApplier {
    runner: Arc<dyn CmdRunner>,
}

impl IpRouteApplier {
    /// Applier over the production `tokio::process` runner.
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioCmdRunner),
        }
    }
}

impl Default for IpRouteApplier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteApplier for IpRouteApplier {
    async fn apply(&self, iface: &str, gateway: Option<&str>) -> bool {
        failover::apply_default_route(self.runner.as_ref(), iface, gateway).await
    }
    async fn installed_iface(&self) -> Option<String> {
        failover::installed_default_iface(self.runner.as_ref()).await
    }
}

/// Live router state, guarded by a single async mutex so a manager-event tick
/// and the health-loop tick never interleave (matches the Python `self._lock`).
#[derive(Debug, Default)]
struct RouterState {
    active_uplink: Option<String>,
    internet_reachable: bool,
    fail_streak: u32,
    success_streak: u32,
    /// Monotonic instant of the last switch; `None` == never switched (treated
    /// as "cooldown elapsed", matching the Python `_last_switch_at = 0.0`).
    last_switch_at: Option<Instant>,
    /// Whether the default route for the active uplink was programmed. A
    /// failed apply is retried on every tick until it lands.
    route_applied: bool,
}

/// The externally-visible effects of a decided switch, cloned out of
/// [`RouterState`] under the lock so the I/O they imply can run with the lock
/// released. See [`UplinkRouter::commit_switch`].
struct SwitchEffect {
    previous: Option<String>,
    uplink: Option<String>,
    available: Vec<String>,
    reachable: bool,
}

/// What a failed probe decided. Computed inside the state critical section, then
/// executed outside it.
enum FailoverOutcome {
    /// The failing uplink is the only one available: reachability drops and the
    /// route stays where it is.
    LostReachability(Option<String>),
    /// A lower-priority uplink is viable: commit the switch to it.
    Switch(SwitchEffect),
}

/// Priority-based uplink failover with hysteresis and health probing.
pub struct UplinkRouter {
    managers: HashMap<String, Arc<dyn UplinkManager>>,
    /// The live failover order. Interior-mutable because the operator can edit
    /// the priority file (`PUT /network/priority`) while the daemon runs and the
    /// daemon owns the router behind an `Arc`; [`tick`](Self::tick) reloads it
    /// from disk each iteration so the change takes effect without a restart.
    /// A `std::sync::Mutex` (not the async one) because the critical sections
    /// are a quick clone / swap that never spans an `.await`.
    priority: std::sync::Mutex<Vec<String>>,
    priority_config_path: std::path::PathBuf,
    prober: Arc<dyn Prober>,
    route: Arc<dyn RouteApplier>,
    state: Mutex<RouterState>,
    bus: Arc<UplinkEventBus>,
    active_flag: Mutex<ActiveFlagWriter>,
}

impl UplinkRouter {
    /// Build a router with explicit managers, a priority list (or `None` to load
    /// from `priority_config_path`), and the production prober + route applier.
    pub fn new(
        managers: HashMap<String, Arc<dyn UplinkManager>>,
        priority: Option<Vec<String>>,
        priority_config_path: Option<std::path::PathBuf>,
    ) -> Self {
        Self::with_seams(
            managers,
            priority,
            priority_config_path,
            Arc::new(CloudProber),
            Arc::new(IpRouteApplier::new()),
            ActiveFlagWriter::new(),
        )
    }

    /// Like [`new`](Self::new) but attaches a durable-store emitter to the
    /// active-uplink flag writer, so each flag change also ships a
    /// `net.uplink_active` event to the logging store. The daemon wires this so a
    /// store-first reader sees the router's truth instead of a dead
    /// in-FastAPI-process singleton. The emitter is best-effort and never gates
    /// the disk write.
    pub fn new_with_emitter(
        managers: HashMap<String, Arc<dyn UplinkManager>>,
        priority: Option<Vec<String>>,
        priority_config_path: Option<std::path::PathBuf>,
        emitter: ados_protocol::logd::emitter::IngestEmitter,
    ) -> Self {
        Self::with_seams(
            managers,
            priority,
            priority_config_path,
            Arc::new(CloudProber),
            Arc::new(IpRouteApplier::new()),
            ActiveFlagWriter::new().with_emitter(emitter),
        )
    }

    /// Full constructor with injectable probe / route / active-flag seams (tests).
    #[allow(clippy::too_many_arguments)]
    pub fn with_seams(
        managers: HashMap<String, Arc<dyn UplinkManager>>,
        priority: Option<Vec<String>>,
        priority_config_path: Option<std::path::PathBuf>,
        prober: Arc<dyn Prober>,
        route: Arc<dyn RouteApplier>,
        active_flag: ActiveFlagWriter,
    ) -> Self {
        let cfg_path =
            priority_config_path.unwrap_or_else(|| paths::gs_uplink_json().to_path_buf());
        let priority = priority.unwrap_or_else(|| failover::load_priority(&cfg_path));
        Self {
            managers,
            priority: std::sync::Mutex::new(priority),
            priority_config_path: cfg_path,
            prober,
            route,
            state: Mutex::new(RouterState::default()),
            bus: Arc::new(UplinkEventBus::new()),
            active_flag: Mutex::new(active_flag),
        }
    }

    /// The event bus, for subscribers.
    pub fn bus(&self) -> Arc<UplinkEventBus> {
        Arc::clone(&self.bus)
    }

    /// A snapshot of the live failover order.
    pub fn get_priority(&self) -> Vec<String> {
        self.priority_snapshot()
    }

    /// Clone the current priority list out of its lock. The lock is held only
    /// for the clone, never across an `.await`.
    fn priority_snapshot(&self) -> Vec<String> {
        self.priority
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Validate, store, and atomically persist a new priority list. Takes
    /// `&self` (the list is interior-mutable) so the daemon, which owns the
    /// router behind an `Arc`, can also drive it.
    ///
    /// `async` because the persist is a blocking write that used to run on the
    /// reactor. The `std::sync::Mutex` critical section is the swap only; its
    /// guard is dropped before the write is awaited.
    pub async fn set_priority(&self, priority_list: Vec<String>) -> Result<(), String> {
        failover::validate_priority(&priority_list)?;
        {
            let mut p = self.priority.lock().unwrap_or_else(|e| e.into_inner());
            *p = priority_list;
        }
        let snapshot = self.priority_snapshot();
        failover::save_priority_async(&self.priority_config_path, &snapshot).await;
        info!(priority = ?snapshot, "uplink.priority_updated");
        Ok(())
    }

    /// Re-read the priority file from disk and adopt it when it differs from the
    /// live order. Returns `true` when the order changed. Called at the top of
    /// each [`tick`](Self::tick) so a `PUT /network/priority` that rewrote the
    /// file takes effect within one health cycle without a daemon restart, the
    /// same per-tick-reread contract the data-cap tracker uses for the cellular
    /// cap. Best-effort: a missing / malformed file resolves to the default,
    /// which is the loader's documented behavior, so a transient read error
    /// never wedges the failover order.
    ///
    /// The read is awaited first and the `std::sync::Mutex` taken only for the
    /// compare-and-swap that follows, so the guard never spans it.
    pub async fn reload_priority(&self) -> bool {
        let on_disk = failover::load_priority_async(&self.priority_config_path).await;
        let mut p = self.priority.lock().unwrap_or_else(|e| e.into_inner());
        if *p == on_disk {
            return false;
        }
        info!(from = ?*p, to = ?on_disk, "uplink.priority_reloaded");
        *p = on_disk;
        true
    }

    fn manager_for(&self, name: &str) -> Option<&Arc<dyn UplinkManager>> {
        // wwan0/wlan0_client/eth0 → their managers; usb0 has no manager (the
        // FSM checks the carrier directly, see `uplink_up`).
        self.managers.get(name)
    }

    async fn is_usb_tether_up(&self) -> bool {
        // Read the carrier flag on the blocking thread pool: a stalled /sys read
        // must never block the tokio reactor, especially since `tick` runs this
        // while holding the router state lock.
        let path = paths::USB0_CARRIER;
        let res = tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await;
        match res {
            Ok(Ok(text)) => text.trim() == "1",
            // A read error (iface absent) or a join error both mean "not up".
            _ => false,
        }
    }

    async fn uplink_up(&self, name: &str) -> bool {
        if name == "usb0" {
            return self.is_usb_tether_up().await;
        }
        match self.manager_for(name) {
            Some(mgr) => mgr.is_up().await,
            None => false,
        }
    }

    async fn uplink_iface(&self, name: &str) -> Option<String> {
        if name == "usb0" {
            return Some("usb0".to_string());
        }
        self.manager_for(name).map(|m| m.get_iface())
    }

    async fn uplink_gateway(&self, name: &str) -> Option<String> {
        if name == "usb0" {
            return None;
        }
        match self.manager_for(name) {
            Some(mgr) => mgr.get_gateway().await,
            None => None,
        }
    }

    async fn viable_uplinks(&self, priority: &[String]) -> Vec<String> {
        let mut viable = Vec::new();
        for name in priority {
            if self.uplink_up(name).await {
                viable.push(name.clone());
            }
        }
        viable
    }

    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Publish a `health_changed` event and reconcile the active-uplink flag to
    /// the new reachability. The flag's presence drives mesh gateway election,
    /// so a reachability transition is a flag-sync point.
    async fn publish_health_change(
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
    fn record_switch(
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
    async fn commit_switch(&self, effect: SwitchEffect) -> bool {
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
    async fn apply_route(&self, name: &str) -> bool {
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
    async fn verify_route(&self, name: &str) {
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
    async fn switch_to(&self, uplink: Option<String>, available: Vec<String>) -> bool {
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
    async fn handle_probe_success(
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
    async fn handle_probe_failure(
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

    /// The kernel iface name of the currently-active uplink, or `None` when no
    /// uplink is active. Used by the data-cap throttle consumer to scope the tc
    /// qdisc + MASQUERADE actions to the right interface.
    pub async fn active_iface(&self) -> Option<String> {
        let name = self.state.lock().await.active_uplink.clone()?;
        self.uplink_iface(&name).await
    }

    /// Record the cellular data-cap throttle level on the active-uplink flag so
    /// a reader of `/run/ados/uplink-active` learns the throttle level. Called
    /// by the data-cap throttle consumer on each threshold transition. Re-syncs
    /// the flag against the current active uplink + reachability so the change
    /// reaches disk immediately (when an uplink is active).
    pub async fn set_data_cap_state(&self, state: events::DataCapState) {
        let (active, reachable) = {
            let st = self.state.lock().await;
            (st.active_uplink.clone(), st.internet_reachable)
        };
        let mut flag = self.active_flag.lock().await;
        if flag.set_data_cap_state(state) {
            flag.sync_async(active.as_deref(), reachable).await;
        }
    }

    /// A snapshot of the live router state. Key names match the Python
    /// `get_state` dict (data-cap usage lands with the modem chunk).
    pub async fn get_state(&self) -> serde_json::Value {
        let priority = self.priority_snapshot();
        let st = self.state.lock().await;
        serde_json::json!({
            "active_uplink": st.active_uplink,
            "internet_reachable": st.internet_reachable,
            "priority": priority,
            "fail_streak": st.fail_streak,
            "success_streak": st.success_streak,
            "last_switch_monotonic": st.last_switch_at.map(|t| t.elapsed().as_secs_f64()),
            "route_applied": st.route_applied,
            "data_usage": serde_json::Value::Null,
        })
    }
}

#[cfg(test)]
mod tests;
