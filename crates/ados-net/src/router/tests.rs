//! Tests for the uplink router.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
#[async_trait]
impl RouteApplier for RecordingRoute {
    async fn apply(&self, iface: &str, gateway: Option<&str>) -> bool {
        self.calls
            .lock()
            .unwrap()
            .push((iface.to_string(), gateway.map(|g| g.to_string())));
        true
    }
    async fn installed_iface(&self) -> Option<String> {
        self.calls.lock().unwrap().last().map(|(i, _)| i.clone())
    }
}

/// Route applier that reads the router's own state while the route is being
/// applied.
///
/// This is the regression guard for the substantive fix: `commit_switch`
/// runs the route apply with the state mutex released, so a concurrent
/// reader completes. Back when `_switch_to` took `st: &mut RouterState` the
/// guard was still held here, and every state reader — `active_iface`,
/// `get_state`, the data-cap consumer — queued behind an `ip route replace`
/// spawn. The read is bounded so a regression fails as an assertion rather
/// than as a hung test.
#[derive(Default)]
struct ReentrantRoute {
    router: std::sync::OnceLock<std::sync::Weak<UplinkRouter>>,
    state_readable: AtomicBool,
}
#[async_trait]
impl RouteApplier for ReentrantRoute {
    async fn apply(&self, _iface: &str, _gateway: Option<&str>) -> bool {
        let Some(router) = self.router.get().and_then(|w| w.upgrade()) else {
            return true;
        };
        // Bounded so a regression is a failed assertion, not a hung test.
        let bound = std::time::Duration::from_secs(2);
        let read = tokio::time::timeout(bound, router.get_state()).await;
        if read.is_ok() {
            self.state_readable.store(true, Ordering::SeqCst);
        }
        true
    }
    async fn installed_iface(&self) -> Option<String> {
        None
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
    r.state.lock().await.last_switch_at = Some(Instant::now() - std::time::Duration::from_secs(60));
}

#[tokio::test]
async fn three_consecutive_failures_after_cooldown_fail_over() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("uplink-active");
    // eth0 + wlan0_client both up; probe always fails.
    let (r, _p) = router(
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
    assert!(r.set_priority(s(&["eth0", "wlan0_client"])).await.is_ok());
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
        "route_applied",
        "data_usage",
    ] {
        assert!(st.get(k).is_some(), "missing key {k}");
    }
}

/// Build a router with an explicit priority-config path so a test can rewrite
/// the file and prove the per-tick reload adopts it.
fn router_with_cfg(
    specs: &[(&str, bool)],
    verdict: bool,
    cfg_path: std::path::PathBuf,
    flag_path: std::path::PathBuf,
) -> UplinkRouter {
    let prober = Arc::new(ScriptedProber {
        verdict,
        calls: AtomicUsize::new(0),
    });
    UplinkRouter::with_seams(
        managers(specs),
        // Load the order from the cfg file (None forces the loader).
        None,
        Some(cfg_path),
        prober as Arc<dyn Prober>,
        Arc::new(RecordingRoute::default()),
        ActiveFlagWriter::with_path(flag_path),
    )
}

#[tokio::test]
async fn reload_priority_adopts_a_rewritten_file() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("ground-station-uplink.json");
    // Seed the file with a non-default order so the initial load picks it up.
    failover::save_priority(&cfg, &s(&["wlan0_client", "eth0"]));
    let r = router_with_cfg(
        &[("eth0", true), ("wlan0_client", true)],
        true,
        cfg.clone(),
        dir.path().join("uplink-active"),
    );
    assert_eq!(r.get_priority(), s(&["wlan0_client", "eth0"]));
    // An unchanged file → reload is a no-op.
    assert!(!r.reload_priority().await);
    // Operator rewrites the file (the REST PUT does this) → reload adopts it.
    failover::save_priority(&cfg, &s(&["eth0", "wlan0_client", "wwan0"]));
    assert!(r.reload_priority().await);
    assert_eq!(r.get_priority(), s(&["eth0", "wlan0_client", "wwan0"]));
    // A second reload with no further change → no-op.
    assert!(!r.reload_priority().await);
}

#[tokio::test]
async fn tick_picks_up_a_priority_change_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("ground-station-uplink.json");
    // Order puts wlan0_client first; both ifaces are up but the probe FAILS,
    // so a failover walks the live priority order.
    failover::save_priority(&cfg, &s(&["wlan0_client", "eth0"]));
    let r = router_with_cfg(
        &[("eth0", true), ("wlan0_client", true)],
        false,
        cfg.clone(),
        dir.path().join("uplink-active"),
    );
    // First tick picks the highest-priority viable uplink = wlan0_client and
    // probes it (fails → fail_streak 1, no switch this tick).
    r.tick().await;
    assert_eq!(r.get_state().await["active_uplink"], "wlan0_client");

    // Operator rewrites the file to prefer eth0; the daemon never restarts.
    // The reloaded order means the next failover target below the failing
    // wlan0_client is eth0. Drive the fail streak past threshold + cooldown.
    failover::save_priority(&cfg, &s(&["eth0", "wlan0_client"]));
    arm_cooldown(&r).await;
    r.tick().await; // reloads the order, fail_streak 2.
                    // The live order now leads with eth0 (proves the reload reached state).
    assert_eq!(
        r.get_state().await["priority"],
        serde_json::json!(["eth0", "wlan0_client"])
    );
    arm_cooldown(&r).await;
    r.tick().await; // fail_streak 3 + cooldown → fail over to eth0.
    assert_eq!(r.get_state().await["active_uplink"], "eth0");
}

#[tokio::test]
async fn the_state_mutex_is_not_held_across_the_route_apply() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("uplink-active");
    let route = Arc::new(ReentrantRoute::default());
    let r = Arc::new(UplinkRouter::with_seams(
        managers(&[("eth0", true)]),
        Some(s(&["eth0"])),
        Some(flag.with_extension("cfg.json")),
        Arc::new(ScriptedProber {
            verdict: true,
            calls: AtomicUsize::new(0),
        }),
        Arc::clone(&route) as Arc<dyn RouteApplier>,
        ActiveFlagWriter::with_path(flag),
    ));
    route.router.set(Arc::downgrade(&r)).unwrap();

    // The first tick has no active uplink, so it picks eth0 — the route-apply
    // path — and the applier reads the state from inside the apply.
    r.tick().await;
    assert_eq!(r.get_state().await["active_uplink"], "eth0");
    assert!(
        route.state_readable.load(Ordering::SeqCst),
        "a router state read must complete while the default route is being applied"
    );
}

/// A manager whose `is_up` can flip mid-test.
struct FlipManager {
    iface: String,
    up: Arc<AtomicBool>,
}
#[async_trait]
impl UplinkManager for FlipManager {
    async fn is_up(&self) -> bool {
        self.up.load(Ordering::SeqCst)
    }
    fn get_iface(&self) -> String {
        self.iface.clone()
    }
    async fn get_gateway(&self) -> Option<String> {
        None
    }
}

/// Route applier whose applies fail until `fail_left` runs out; reports the
/// last successfully applied iface as installed.
#[derive(Default)]
struct FlakyRoute {
    fail_left: AtomicUsize,
    applies: AtomicUsize,
    installed: std::sync::Mutex<Option<String>>,
}
#[async_trait]
impl RouteApplier for FlakyRoute {
    async fn apply(&self, iface: &str, _gateway: Option<&str>) -> bool {
        self.applies.fetch_add(1, Ordering::SeqCst);
        if self.fail_left.load(Ordering::SeqCst) > 0 {
            self.fail_left.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        *self.installed.lock().unwrap() = Some(iface.to_string());
        true
    }
    async fn installed_iface(&self) -> Option<String> {
        self.installed.lock().unwrap().clone()
    }
}

fn flaky_router(
    managers: HashMap<String, Arc<dyn UplinkManager>>,
    route: Arc<FlakyRoute>,
    dir: &std::path::Path,
) -> UplinkRouter {
    UplinkRouter::with_seams(
        managers,
        Some(s(&["eth0", "wlan0_client", "wwan0", "usb0"])),
        Some(dir.join("uplink.cfg.json")),
        Arc::new(ScriptedProber {
            verdict: true,
            calls: AtomicUsize::new(0),
        }),
        route as Arc<dyn RouteApplier>,
        ActiveFlagWriter::with_path(dir.join("uplink-active")),
    )
}

#[tokio::test]
async fn failed_route_apply_retries_each_tick() {
    let dir = tempfile::tempdir().unwrap();
    let route = Arc::new(FlakyRoute::default());
    route.fail_left.store(2, Ordering::SeqCst);
    let r = flaky_router(managers(&[("eth0", true)]), Arc::clone(&route), dir.path());

    r.tick().await; // first pick: apply fails
    assert_eq!(r.get_state().await["route_applied"], false);
    assert_eq!(route.applies.load(Ordering::SeqCst), 1);
    r.tick().await; // retry fails again
    assert_eq!(route.applies.load(Ordering::SeqCst), 2);
    assert_eq!(r.get_state().await["route_applied"], false);
    r.tick().await; // retry lands
    assert_eq!(route.applies.load(Ordering::SeqCst), 3);
    assert_eq!(r.get_state().await["route_applied"], true);
    r.tick().await; // installed route names the iface: no re-apply
    assert_eq!(route.applies.load(Ordering::SeqCst), 3);

    // Someone else rewrote the default: the next tick puts it back.
    *route.installed.lock().unwrap() = Some("wlan9".to_string());
    r.tick().await;
    assert_eq!(route.applies.load(Ordering::SeqCst), 4);
    assert_eq!(route.installed.lock().unwrap().as_deref(), Some("eth0"));
}

#[tokio::test]
async fn vanished_active_uplink_switches_in_one_tick() {
    let dir = tempfile::tempdir().unwrap();
    let eth_up = Arc::new(AtomicBool::new(true));
    let mut m: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
    m.insert(
        "eth0".to_string(),
        Arc::new(FlipManager {
            iface: "eth0".to_string(),
            up: Arc::clone(&eth_up),
        }),
    );
    m.insert(
        "wlan0_client".to_string(),
        Arc::new(FlipManager {
            iface: "wlan0".to_string(),
            up: Arc::new(AtomicBool::new(true)),
        }),
    );
    let route = Arc::new(FlakyRoute::default());
    let r = flaky_router(m, Arc::clone(&route), dir.path());

    r.tick().await;
    assert_eq!(r.get_state().await["active_uplink"], "eth0");

    // Unplugged right after a switch: the cooldown is still closed and no
    // failure streak exists, yet the switch happens on this very tick.
    eth_up.store(false, Ordering::SeqCst);
    r.tick().await;
    assert_eq!(r.get_state().await["active_uplink"], "wlan0_client");
    assert_eq!(route.installed.lock().unwrap().as_deref(), Some("wlan0"));
}
