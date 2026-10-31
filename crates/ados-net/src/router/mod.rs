//! Uplink router orchestrator.
//!
//! The ground station can reach the cloud over several uplinks: wired Ethernet
//! (`eth0`), WiFi client (`wlan0_client` when the onboard radio is in STA mode,
//! mutually exclusive with AP mode), cellular (`wwan0`), and USB tether
//! (`usb0` when a laptop shares its connection over the USB gadget). This
//! orchestrator picks exactly one as the default route based on a configured
//! priority list, and fails over automatically when the active uplink stops
//! reaching the cloud relay.
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
mod switch;

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
