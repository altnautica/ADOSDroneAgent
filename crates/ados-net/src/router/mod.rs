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
/// [`failover::apply_default_route`]; tests inject a recording no-op.
pub trait RouteApplier: Send + Sync {
    fn apply(&self, iface: &str, gateway: Option<&str>) -> bool;
}

/// Production route applier: `ip route replace default ...`.
#[derive(Debug, Default, Clone, Copy)]
pub struct IpRouteApplier;

impl RouteApplier for IpRouteApplier {
    fn apply(&self, iface: &str, gateway: Option<&str>) -> bool {
        failover::apply_default_route(iface, gateway)
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
}

/// Priority-based uplink failover with hysteresis and health probing.
pub struct UplinkRouter {
    managers: HashMap<String, Arc<dyn UplinkManager>>,
    priority: Vec<String>,
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
            Arc::new(IpRouteApplier),
            ActiveFlagWriter::new(),
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
            priority,
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

    pub fn get_priority(&self) -> Vec<String> {
        self.priority.clone()
    }

    /// Validate, store, and atomically persist a new priority list.
    pub fn set_priority(&mut self, priority_list: Vec<String>) -> Result<(), String> {
        failover::validate_priority(&priority_list)?;
        self.priority = priority_list;
        failover::save_priority(&self.priority_config_path, &self.priority);
        info!(priority = ?self.priority, "uplink.priority_updated");
        Ok(())
    }

    fn manager_for(&self, name: &str) -> Option<&Arc<dyn UplinkManager>> {
        // wwan0/wlan0_client/eth0 → their managers; usb0 has no manager (the
        // FSM checks the carrier directly, see `uplink_up`).
        self.managers.get(name)
    }

    async fn is_usb_tether_up(&self) -> bool {
        match std::fs::read_to_string(paths::USB0_CARRIER) {
            Ok(text) => text.trim() == "1",
            Err(_) => false,
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

    async fn viable_uplinks(&self) -> Vec<String> {
        let mut viable = Vec::new();
        for name in &self.priority {
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
        self.active_flag.lock().await.sync(active, reachable);
        self.bus.publish(UplinkEvent {
            kind: UplinkEventKind::HealthChanged,
            active_uplink: active.map(|s| s.to_string()),
            available,
            internet_reachable: reachable,
            data_cap_state: None,
            timestamp_ms: self.now_ms(),
        });
    }

    /// Switch the active uplink, re-program the default route, reconcile the
    /// active-uplink flag, and publish an `uplink_changed` event. Ports
    /// `_switch_to`. Caller holds the state lock.
    async fn switch_to(
        &self,
        st: &mut RouterState,
        uplink: Option<String>,
        available: Vec<String>,
    ) {
        let previous = st.active_uplink.clone();
        st.active_uplink = uplink.clone();
        st.fail_streak = 0;
        st.success_streak = 0;
        st.last_switch_at = Some(Instant::now());

        if let Some(ref name) = uplink {
            let iface = self
                .uplink_iface(name)
                .await
                .unwrap_or_else(|| name.clone());
            let gateway = self.uplink_gateway(name).await;
            self.route.apply(&iface, gateway.as_deref());
        }

        // Presence of the flag is the mesh gateway-election signal: write it on
        // a switch to a real uplink, unlink it on a switch to None.
        self.active_flag
            .lock()
            .await
            .sync(uplink.as_deref(), st.internet_reachable);

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
            internet_reachable: st.internet_reachable,
            data_cap_state: None,
            timestamp_ms: self.now_ms(),
        });
    }

    /// One control-loop iteration. Ports `_tick`.
    pub async fn tick(&self) {
        let mut st = self.state.lock().await;
        let available = self.viable_uplinks().await;

        // No viable uplink at all. Clear state if we had one.
        if available.is_empty() {
            if st.active_uplink.is_some() {
                self.switch_to(&mut st, None, Vec::new()).await;
            }
            if st.internet_reachable {
                st.internet_reachable = false;
                self.publish_health_change(None, Vec::new(), false).await;
            } else {
                // Even with no prior reachability, ensure the flag is gone when
                // there is no uplink, so mesh election never sees a stale flag.
                self.active_flag.lock().await.sync(None, false);
            }
            return;
        }

        // First-time pick: highest-priority viable uplink.
        if st.active_uplink.is_none() {
            let first = available[0].clone();
            self.switch_to(&mut st, Some(first), available.clone())
                .await;
        }

        // Probe the current uplink.
        let current = st.active_uplink.clone().unwrap_or_default();
        let iface = self.uplink_iface(&current).await;
        let ok = self.prober.probe(iface.as_deref()).await;

        let cooldown_ok = match st.last_switch_at {
            Some(t) => t.elapsed().as_secs_f64() >= failover::SWITCH_COOLDOWN_SECONDS,
            None => true,
        };

        if ok {
            self.handle_probe_success(&mut st, available, cooldown_ok)
                .await;
        } else {
            self.handle_probe_failure(&mut st, available, cooldown_ok)
                .await;
        }
    }

    /// Ports `_handle_probe_success`. Caller holds the state lock.
    async fn handle_probe_success(
        &self,
        st: &mut RouterState,
        available: Vec<String>,
        cooldown_ok: bool,
    ) {
        st.fail_streak = 0;
        if !st.internet_reachable {
            st.internet_reachable = true;
            let active = st.active_uplink.clone();
            info!(uplink = ?active, "uplink.health_recovered");
            self.publish_health_change(active.as_deref(), available.clone(), true)
                .await;
        }

        let active = match (cooldown_ok, st.active_uplink.clone()) {
            (true, Some(a)) => a,
            _ => return,
        };

        let higher = failover::select_higher_priority(&self.priority, &available, Some(&active));
        if higher.is_empty() {
            st.success_streak = 0;
            return;
        }

        st.success_streak += 1;
        if st.success_streak < failover::SUCCESS_UP_THRESHOLD {
            return;
        }

        // Probe the higher-priority uplink before switching up so we do not
        // drop off a working link for a dead one.
        let candidate = higher[0].clone();
        let cand_iface = self
            .uplink_iface(&candidate)
            .await
            .unwrap_or_else(|| candidate.clone());
        if self.prober.probe(Some(&cand_iface)).await {
            self.switch_to(st, Some(candidate), available).await;
        }
    }

    /// Ports `_handle_probe_failure`. Caller holds the state lock.
    async fn handle_probe_failure(
        &self,
        st: &mut RouterState,
        available: Vec<String>,
        cooldown_ok: bool,
    ) {
        st.success_streak = 0;
        st.fail_streak += 1;
        if st.fail_streak < failover::FAIL_DOWN_THRESHOLD {
            return;
        }
        if !cooldown_ok {
            return;
        }

        let next_uplink = failover::select_failover_target(
            &self.priority,
            &available,
            st.active_uplink.as_deref(),
        );
        let next_uplink = match next_uplink {
            Some(n) => n,
            None => {
                // Only the current (failing) uplink is available.
                if st.internet_reachable {
                    st.internet_reachable = false;
                    let active = st.active_uplink.clone();
                    self.publish_health_change(active.as_deref(), available, false)
                        .await;
                }
                return;
            }
        };

        warn!(
            from_uplink = ?st.active_uplink,
            to_uplink = next_uplink,
            fail_streak = st.fail_streak,
            "uplink.failover"
        );
        st.internet_reachable = false;
        self.switch_to(st, Some(next_uplink), available).await;
    }

    /// A snapshot of the live router state. Key names match the Python
    /// `get_state` dict (data-cap usage lands with the modem chunk).
    pub async fn get_state(&self) -> serde_json::Value {
        let st = self.state.lock().await;
        serde_json::json!({
            "active_uplink": st.active_uplink,
            "internet_reachable": st.internet_reachable,
            "priority": self.priority,
            "fail_streak": st.fail_streak,
            "success_streak": st.success_streak,
            "last_switch_monotonic": st.last_switch_at.map(|t| t.elapsed().as_secs_f64()),
            "data_usage": serde_json::Value::Null,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// A manager whose `is_up` is fixed at construction.
    struct UpManager {
        iface: String,
        up: bool,
    }
    #[async_trait]
    impl UplinkManager for UpManager {
        async fn is_up(&self) -> bool {
            self.up
        }
        fn get_iface(&self) -> String {
            self.iface.clone()
        }
        async fn get_gateway(&self) -> Option<String> {
            None
        }
    }

    /// Prober that returns scripted verdicts; counts calls.
    struct ScriptedProber {
        verdict: bool,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl Prober for ScriptedProber {
        async fn probe(&self, _iface: Option<&str>) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.verdict
        }
    }

    /// Route applier that records the last (iface, gateway) it was asked for.
    #[derive(Default)]
    struct RecordingRoute {
        calls: std::sync::Mutex<Vec<(String, Option<String>)>>,
    }
    impl RouteApplier for RecordingRoute {
        fn apply(&self, iface: &str, gateway: Option<&str>) -> bool {
            self.calls
                .lock()
                .unwrap()
                .push((iface.to_string(), gateway.map(|g| g.to_string())));
            true
        }
    }

    fn managers(specs: &[(&str, bool)]) -> HashMap<String, Arc<dyn UplinkManager>> {
        specs
            .iter()
            .map(|(name, up)| {
                let m: Arc<dyn UplinkManager> = Arc::new(UpManager {
                    iface: name.to_string(),
                    up: *up,
                });
                (name.to_string(), m)
            })
            .collect()
    }

    fn router(
        specs: &[(&str, bool)],
        verdict: bool,
        flag_path: std::path::PathBuf,
    ) -> (UplinkRouter, Arc<ScriptedProber>) {
        let prober = Arc::new(ScriptedProber {
            verdict,
            calls: AtomicUsize::new(0),
        });
        let r = UplinkRouter::with_seams(
            managers(specs),
            Some(s(&["eth0", "wlan0_client", "wwan0", "usb0"])),
            Some(flag_path.with_extension("cfg.json")),
            Arc::clone(&prober) as Arc<dyn Prober>,
            Arc::new(RecordingRoute::default()),
            ActiveFlagWriter::with_path(flag_path),
        );
        (r, prober)
    }

    #[tokio::test]
    async fn first_pick_selects_highest_priority_and_writes_flag() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        // wlan0_client up, eth0 down → highest-priority viable is wlan0_client.
        let (r, _p) = router(
            &[("eth0", false), ("wlan0_client", true)],
            true,
            flag.clone(),
        );
        r.tick().await;
        let st = r.get_state().await;
        assert_eq!(st["active_uplink"], "wlan0_client");
        assert_eq!(st["internet_reachable"], true);
        // Headline fix: the flag exists now that there is an active uplink.
        assert!(flag.is_file());
    }

    #[tokio::test]
    async fn no_viable_uplink_unlinks_flag() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        let (r, _p) = router(&[("eth0", false)], true, flag.clone());
        r.tick().await;
        let st = r.get_state().await;
        assert!(st["active_uplink"].is_null());
        assert!(!flag.is_file());
    }

    /// Drop `last_switch_at` far into the past so the cooldown gate is open.
    async fn arm_cooldown(r: &UplinkRouter) {
        r.state.lock().await.last_switch_at =
            Some(Instant::now() - std::time::Duration::from_secs(60));
    }

    #[tokio::test]
    async fn three_consecutive_failures_after_cooldown_fail_over() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        // eth0 + wlan0_client both up; probe always fails.
        let (mut r, _p) = router(
            &[("eth0", true), ("wlan0_client", true)],
            false,
            flag.clone(),
        );
        // First tick picks eth0 AND probes it (probe fails) → fail_streak 1.
        // The pick set last_switch_at = now, so the cooldown gate is closed and
        // no failover happens this tick regardless of the streak (Python `_tick`
        // behaves the same: it probes the freshly-picked uplink in the same tick).
        r.tick().await;
        assert_eq!(r.get_state().await["active_uplink"], "eth0");
        assert_eq!(r.get_state().await["fail_streak"], 1);
        // Open the cooldown gate for the remaining failure ticks.
        arm_cooldown(&r).await;
        r.tick().await; // fail_streak 2 (still < FAIL_DOWN_THRESHOLD, no switch).
        assert_eq!(r.get_state().await["active_uplink"], "eth0");
        assert_eq!(r.get_state().await["fail_streak"], 2);
        arm_cooldown(&r).await;
        // fail_streak 3 + cooldown open → failover down to wlan0_client.
        r.tick().await;
        assert_eq!(r.get_state().await["active_uplink"], "wlan0_client");
        // The flag is still present: a lower uplink is now active.
        assert!(flag.is_file());
        // set_priority is the persisted-config writer; exercise it once here.
        assert!(r.set_priority(s(&["eth0", "wlan0_client"])).is_ok());
    }

    #[tokio::test]
    async fn fewer_than_three_failures_do_not_fail_over() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        let (r, _p) = router(
            &[("eth0", true), ("wlan0_client", true)],
            false,
            flag.clone(),
        );
        // Picking tick → fail_streak 1. One more failure tick (cooldown open)
        // → fail_streak 2, still below the 3-fail threshold, so no failover.
        r.tick().await;
        arm_cooldown(&r).await;
        r.tick().await;
        assert_eq!(r.get_state().await["fail_streak"], 2);
        assert_eq!(r.get_state().await["active_uplink"], "eth0");
    }

    #[tokio::test]
    async fn climbs_back_to_higher_priority_after_success_streak() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        // Both up; probe always succeeds. Force the active uplink to the lower
        // wlan0_client so a higher-priority eth0 is available to climb to.
        let (r, _p) = router(&[("eth0", true), ("wlan0_client", true)], true, flag);
        {
            let mut st = r.state.lock().await;
            st.active_uplink = Some("wlan0_client".to_string());
            st.internet_reachable = true;
        }
        // SUCCESS_UP_THRESHOLD (=3) consecutive successes on the lower uplink,
        // each with the cooldown gate open and the higher candidate probing OK,
        // climb us back to eth0. The first two ticks build the success streak.
        arm_cooldown(&r).await;
        r.tick().await;
        assert_eq!(r.get_state().await["active_uplink"], "wlan0_client");
        arm_cooldown(&r).await;
        r.tick().await;
        assert_eq!(r.get_state().await["active_uplink"], "wlan0_client");
        // Third success → probe the candidate (eth0) → switch up.
        arm_cooldown(&r).await;
        r.tick().await;
        assert_eq!(r.get_state().await["active_uplink"], "eth0");
    }

    #[tokio::test]
    async fn get_state_keys_match_python() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        let (r, _p) = router(&[("eth0", true)], true, flag);
        let st = r.get_state().await;
        for k in [
            "active_uplink",
            "internet_reachable",
            "priority",
            "fail_streak",
            "success_streak",
            "last_switch_monotonic",
            "data_usage",
        ] {
            assert!(st.get(k).is_some(), "missing key {k}");
        }
    }
}
