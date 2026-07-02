//! Management-link guardian (the supervisor's whole-link backstop).
//!
//! Watches the operator's management link — the interface carrying the default
//! route (wired Ethernet by default, or the onboard Wi-Fi), never the WFB
//! injection adapter — for a dead data path (no carrier, no routable lease, or
//! an unreachable gateway) and walks an escalating, idempotent software repair
//! ladder WITHOUT a reboot: re-assert the global regulatory domain → renew DHCP
//! → reconnect Wi-Fi → bounce the interface → restart the network backend. It
//! runs across both NetworkManager and systemd-networkd.
//!
//! It sits above the reactive Wi-Fi self-heal (which rebuilds a single onboard
//! Wi-Fi connection) as the stack-agnostic backstop for the whole link,
//! including wired Ethernet and networkd-managed boxes the per-connection heal
//! does not cover.
//!
//! Safety:
//! - It NEVER touches the WFB injection interface (a Realtek monitor-mode radio
//!   driver). The interface resolver excludes it; the picker can never return it.
//! - Link-dropping rungs are issued as an atomic local down→up that the
//!   supervisor (a local daemon, not reached over the management link) completes,
//!   so a bounce of the operator's own link self-restores. One rung runs per
//!   tick; the next tick re-checks health and escalates only if still broken, so
//!   a single tick stays well under the supervisor watchdog window.
//! - It runs the disruptive ladder only when a managed backend (NetworkManager
//!   or systemd-networkd) is active, so a host without a running management stack
//!   stays inert.
//!
//! Default-ON, configurable under `network.management_link_guardian`. The pure
//! decision logic, parsers, command matrix, and config parsing are unit-tested
//! on every host; the OS edges are Linux-only and the tick is an inert no-op on
//! a non-Linux dev host.

pub mod backends;
pub mod detection;
pub mod ladder;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;

use detection::HealthVerdict;
use ladder::RepairRung;

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

/// Default steady reconcile cadence. The health check is cheap; the expensive
/// ladder runs only on a sustained non-healthy verdict.
const DEFAULT_TICK_INTERVAL_S: u64 = 30;
/// Default duration after process start during which the health check runs at
/// the faster cadence (the boot window where a foreign regulatory domain is most
/// likely to break the onboard link). Measured against process uptime.
const DEFAULT_FAST_INITIAL_WINDOW_S: u64 = 60;
/// Default cadence during the fast-initial window. Also the cadence used while a
/// known break is actively being repaired, so the ladder climbs quickly.
const DEFAULT_FAST_INITIAL_TICK_INTERVAL_S: u64 = 5;
/// Default consecutive non-healthy ticks before the ladder runs (suppresses a
/// single transient failing sample).
const DEFAULT_FAIL_THRESHOLD: u32 = 2;
/// Default rolling-window cap on repair rungs before the guardian declares the
/// link exhausted and hands off to the reach-back layer.
const DEFAULT_REPAIRS_PER_WINDOW: u32 = 5;
/// Default rolling window for the repair cap.
const DEFAULT_REPAIR_WINDOW_S: u64 = 600;

/// The `/run/ados` sidecar the Python heartbeat reads to surface the link state.
#[cfg(target_os = "linux")]
const SIDECAR_PATH: &str = "/run/ados/mgmt-link.json";

/// Schema version of the `mgmt-link.json` sidecar. Bump on an incompatible
/// field-set change; a reader compares it best-effort via
/// `ados_protocol::sidecar::check_sidecar_version`. Kept in step with the
/// registry in `contracts.toml`. Gated to the platforms that build the writer
/// (Linux) or the version test.
#[cfg(any(target_os = "linux", test))]
const MGMT_LINK_SIDECAR_VERSION: u16 = 1;

/// Configuration, read from `network.management_link_guardian`. Default-ON so a
/// fresh board keeps its management link out of the box; an operator can disable
/// it cleanly if a bespoke network setup ever conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MgmtGuardianConfig {
    pub enabled: bool,
    pub tick_interval: Duration,
    pub fast_initial_window: Duration,
    pub fast_initial_tick: Duration,
    pub fail_threshold: u32,
    pub repairs_per_window: u32,
    pub repair_window: Duration,
}

impl Default for MgmtGuardianConfig {
    fn default() -> Self {
        MgmtGuardianConfig {
            enabled: true,
            tick_interval: Duration::from_secs(DEFAULT_TICK_INTERVAL_S),
            fast_initial_window: Duration::from_secs(DEFAULT_FAST_INITIAL_WINDOW_S),
            fast_initial_tick: Duration::from_secs(DEFAULT_FAST_INITIAL_TICK_INTERVAL_S),
            fail_threshold: DEFAULT_FAIL_THRESHOLD,
            repairs_per_window: DEFAULT_REPAIRS_PER_WINDOW,
            repair_window: Duration::from_secs(DEFAULT_REPAIR_WINDOW_S),
        }
    }
}

impl MgmtGuardianConfig {
    /// The cadence given the current process uptime: faster inside the
    /// fast-initial window (a zero window disables that), else the steady
    /// cadence. Pure so the schedule is unit-tested without a clock.
    pub fn effective_interval(&self, uptime: Duration) -> Duration {
        if !self.fast_initial_window.is_zero() && uptime < self.fast_initial_window {
            self.fast_initial_tick
        } else {
            self.tick_interval
        }
    }
}

/// Parse `network.management_link_guardian` out of a config body. An absent
/// section reads as the all-defaults (enabled) config; a malformed config also
/// falls back to enabled rather than silently disabling the protection.
pub fn read_config_from(text: &str) -> MgmtGuardianConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        management_link_guardian: Option<Guardian>,
    }
    #[derive(serde::Deserialize)]
    struct Guardian {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        tick_interval_s: Option<u64>,
        #[serde(default)]
        fast_initial_window_s: Option<u64>,
        #[serde(default)]
        fast_initial_tick_interval_s: Option<u64>,
        #[serde(default)]
        fail_threshold: Option<u32>,
        #[serde(default)]
        repairs_per_window: Option<u32>,
        #[serde(default)]
        repair_window_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.management_link_guardian {
            Some(g) => MgmtGuardianConfig {
                enabled: g.enabled,
                tick_interval: Duration::from_secs(
                    g.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1),
                ),
                fast_initial_window: Duration::from_secs(
                    g.fast_initial_window_s
                        .unwrap_or(DEFAULT_FAST_INITIAL_WINDOW_S),
                ),
                fast_initial_tick: Duration::from_secs(
                    g.fast_initial_tick_interval_s
                        .unwrap_or(DEFAULT_FAST_INITIAL_TICK_INTERVAL_S)
                        .max(1),
                ),
                fail_threshold: g.fail_threshold.unwrap_or(DEFAULT_FAIL_THRESHOLD).max(1),
                repairs_per_window: g
                    .repairs_per_window
                    .unwrap_or(DEFAULT_REPAIRS_PER_WINDOW)
                    .max(1),
                repair_window: Duration::from_secs(
                    g.repair_window_s.unwrap_or(DEFAULT_REPAIR_WINDOW_S).max(1),
                ),
            },
            None => MgmtGuardianConfig::default(),
        },
        Err(_) => MgmtGuardianConfig::default(),
    }
}

/// Read the guardian config from the canonical config path. Re-read each tick so
/// an edit takes effect without restarting the supervisor.
#[cfg(target_os = "linux")]
fn read_config() -> MgmtGuardianConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => MgmtGuardianConfig::default(),
    }
}

/// The management-link guardian. Holds single-interface episode state across
/// ticks. The fields are read only on the Linux tick (and in tests); on a
/// non-Linux dev host the tick is an inert no-op.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct MgmtLinkGuardian {
    last_tick: Option<Instant>,
    started_at: Instant,
    consecutive_unhealthy: u32,
    last_verdict: Option<HealthVerdict>,
    /// The interface last seen holding the default route, so a repair can still
    /// target it when the route is momentarily gone.
    last_managed_iface: Option<String>,
    /// The index of the next rung to climb for the current episode.
    rung_cursor: usize,
    repair_times: VecDeque<Instant>,
    last_rung: Option<RepairRung>,
    last_repair_at: Option<u64>,
    events: EventEmitter,
}

impl MgmtLinkGuardian {
    /// Build a guardian that records events through `events`.
    pub fn new(events: EventEmitter) -> Self {
        MgmtLinkGuardian {
            last_tick: None,
            started_at: Instant::now(),
            consecutive_unhealthy: 0,
            last_verdict: None,
            last_managed_iface: None,
            rung_cursor: 0,
            repair_times: VecDeque::new(),
            last_rung: None,
            last_repair_at: None,
            events,
        }
    }

    /// Whether the tick is due given the interval and the last tick time. Pure.
    #[cfg(any(target_os = "linux", test))]
    fn due(&self, interval: Duration, now: Instant) -> bool {
        match self.last_tick {
            None => true,
            Some(last) => now.duration_since(last) >= interval,
        }
    }

    /// Count repair rungs still inside the rolling window, pruning older ones.
    #[cfg(any(target_os = "linux", test))]
    fn repairs_in_window(&mut self, now: Instant, window: Duration) -> u32 {
        while let Some(&front) = self.repair_times.front() {
            if now.duration_since(front) > window {
                self.repair_times.pop_front();
            } else {
                break;
            }
        }
        self.repair_times.len() as u32
    }

    /// One guardian tick: throttle, resolve the management interface, check its
    /// health, mirror the sidecar, and (on a sustained break) climb exactly one
    /// repair rung.
    #[cfg(target_os = "linux")]
    pub async fn tick(&mut self) {
        let cfg = read_config();
        if !cfg.enabled {
            return;
        }
        let now = Instant::now();
        // Poll fast while a known break is being repaired; otherwise the
        // fast-initial-then-steady schedule.
        let interval = if matches!(
            self.last_verdict,
            Some(HealthVerdict::Degraded) | Some(HealthVerdict::Down)
        ) {
            cfg.fast_initial_tick
        } else {
            cfg.effective_interval(now.duration_since(self.started_at))
        };
        if !self.due(interval, now) {
            return;
        }
        self.last_tick = Some(now);

        // Detection needs `ip`; without it (a non-networked host) stay inert.
        if !ip_available().await {
            return;
        }

        let candidates = detection::collect_candidates().await;
        let default_iface = detection::default_route_iface().await;
        let Some(managed) = detection::pick_managed_iface(
            default_iface.as_deref(),
            self.last_managed_iface.as_deref(),
            &candidates,
        ) else {
            return; // no real management interface to watch
        };
        self.last_managed_iface = Some(managed.iface.clone());

        let signals = detection::collect_signals(&managed.iface).await;
        let verdict = detection::verdict_of(signals);

        // Detect the backend only when unhealthy (keeps healthy ticks cheap).
        let backend = if verdict != HealthVerdict::Healthy {
            Some(backends::detect_backend().await)
        } else {
            None
        };
        let backend_label = backend.map(|b| b.as_str()).unwrap_or("");

        // Emit a health event only on a state transition (not every tick).
        if self.last_verdict != Some(verdict) {
            self.events.emit(
                ladder::LINK_HEALTH_KIND,
                ladder::level_for(verdict),
                ladder::link_health_detail(&managed.iface, managed.transport, signals, verdict),
            );
        }

        let repairs_in_window = self.repairs_in_window(now, cfg.repair_window);
        let repairing = verdict != HealthVerdict::Healthy
            && self.consecutive_unhealthy.saturating_add(1) >= cfg.fail_threshold.max(1);
        self.write_sidecar(
            &managed,
            backend_label,
            signals,
            verdict,
            repairing,
            repairs_in_window,
        );

        // Healthy: clear the episode and stop.
        if verdict == HealthVerdict::Healthy {
            self.consecutive_unhealthy = 0;
            self.rung_cursor = 0;
            self.last_verdict = Some(verdict);
            return;
        }

        self.consecutive_unhealthy = self.consecutive_unhealthy.saturating_add(1);
        self.last_verdict = Some(verdict);

        // Suppress a single transient failing tick.
        if self.consecutive_unhealthy < cfg.fail_threshold.max(1) {
            return;
        }

        // Only repair when a managed backend is active (also keeps a host with
        // no running management stack — e.g. CI — inert).
        let Some(backend) = backend else {
            return;
        };
        if backend == backends::Backend::Fallback {
            return;
        }

        // Rolling repair cap → exhausted (hand off to the reach-back layer).
        if repairs_in_window >= cfg.repairs_per_window {
            self.emit_exhausted(&managed.iface, repairs_in_window);
            return;
        }

        let rungs = ladder::ladder_for(verdict, managed.transport);
        if self.rung_cursor >= rungs.len() {
            // Every rung was tried this pass. We only got here because the cap
            // check above PASSED — the rolling window has headroom — so re-arm
            // the ladder for another pass rather than staying exhausted forever.
            // The `repairs_per_window` cap (handled above) is the real bound; as
            // old repairs age out of the window, the guardian keeps re-attempting
            // at the capped rate instead of going silent until the next reboot.
            self.rung_cursor = 0;
        }

        // Run exactly ONE rung this tick. The next tick re-checks health and
        // escalates only if the link is still broken.
        let rung = rungs[self.rung_cursor];
        self.rung_cursor += 1;
        let dropping_on_control = ladder::rung_drops_link(rung)
            && ladder::is_live_control_path(&managed.iface, default_iface.as_deref());

        if rung == RepairRung::ReassertReg {
            // The shared channel-safety-gated global regulatory reconcile: never
            // caps the WFB radio, never touches an interface.
            crate::reg_reconciler::reconcile_global_domain(&self.events).await;
        } else {
            backends::run_rung(backend, rung, &managed.iface, managed.transport).await;
        }

        self.repair_times.push_back(now);
        self.last_rung = Some(rung);
        self.last_repair_at = Some(now_unix());
        self.events.emit(
            ladder::LINK_REPAIR_KIND,
            ados_protocol::logd::Level::Info,
            ladder::repair_attempt_detail(
                &managed.iface,
                backend.as_str(),
                rung,
                dropping_on_control,
            ),
        );
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn tick(&mut self) {}

    #[cfg(target_os = "linux")]
    fn emit_exhausted(&mut self, iface: &str, repairs_in_window: u32) {
        // Emit ONCE on entering the exhausted state, not on every 5 s tick for
        // the life of the outage. `last_rung == Exhausted` means the previous
        // tick already announced it and nothing has run since (a real repair
        // rung resets `last_rung`, which re-arms a fresh announcement).
        if self.last_rung == Some(RepairRung::Exhausted) {
            return;
        }
        self.events.emit(
            ladder::LINK_EXHAUSTED_KIND,
            ados_protocol::logd::Level::Warn,
            ladder::repair_exhausted_detail(iface, repairs_in_window),
        );
        self.last_rung = Some(RepairRung::Exhausted);
    }

    /// Mirror the current link state to `/run/ados/mgmt-link.json` for the
    /// heartbeat. Best-effort; a write error is logged and discarded.
    #[cfg(target_os = "linux")]
    fn write_sidecar(
        &self,
        managed: &detection::ManagedIface,
        backend: &str,
        signals: detection::LinkSignals,
        verdict: HealthVerdict,
        repairing: bool,
        repairs_in_window: u32,
    ) {
        let snap = MgmtLinkSidecar {
            version: MGMT_LINK_SIDECAR_VERSION,
            state: ladder::verdict_str(verdict).to_string(),
            iface: managed.iface.clone(),
            transport: managed.transport.as_str().to_string(),
            backend: backend.to_string(),
            carrier: signals.carrier,
            has_lease: signals.has_lease,
            gateway_reachable: signals.gateway_reachable,
            repairing,
            last_rung: self.last_rung.map(|r| r.as_str().to_string()),
            last_repair_at_unix: self.last_repair_at,
            repairs_in_window,
            updated_at_unix: now_unix(),
        };
        if let Err(e) = write_json_atomic(std::path::Path::new(SIDECAR_PATH), &snap, 0o644) {
            tracing::debug!(error = %e, "mgmt_link sidecar write failed");
        }
    }
}

/// The `/run/ados/mgmt-link.json` snapshot. snake_case on disk; the Python
/// heartbeat maps it to a camelCase `managementLink` object.
#[derive(serde::Serialize)]
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
struct MgmtLinkSidecar {
    /// Sidecar schema version (best-effort drift signal for readers).
    version: u16,
    state: String,
    iface: String,
    transport: String,
    backend: String,
    carrier: bool,
    has_lease: bool,
    gateway_reachable: bool,
    repairing: bool,
    last_rung: Option<String>,
    last_repair_at_unix: Option<u64>,
    repairs_in_window: u32,
    updated_at_unix: u64,
}

/// Seconds since the Unix epoch, or 0 if the clock is before it.
#[cfg(target_os = "linux")]
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Atomically write `value` as JSON to `path` with the given Unix `mode`
/// (serialize → tmp sibling → fsync → rename). Duplicated from the ground-link
/// sidecar helper to keep this crate dependency-minimal.
#[cfg(any(target_os = "linux", test))]
fn write_json_atomic<T: serde::Serialize>(
    path: &std::path::Path,
    value: &T,
    mode: u32,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Linux command helpers shared by the submodules (via `super::`).
// ---------------------------------------------------------------------------

/// True when the `ip` binary is on PATH.
#[cfg(target_os = "linux")]
async fn ip_available() -> bool {
    run_status("sh", &["-c", "command -v ip"]).await
}

/// Run a command, returning true on a zero exit. stdout/stderr discarded.
#[cfg(target_os = "linux")]
async fn run_status(cmd: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command and capture stdout, or `None` when it could not be run.
#[cfg(target_os = "linux")]
async fn run_output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mgmt_link_sidecar_version_matches_registry() {
        // The per-file const and the sidecar registry are the two sources of
        // truth for this sidecar's schema version; a drift is caught here.
        assert_eq!(
            MGMT_LINK_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("mgmt-link").unwrap()
        );
    }

    #[test]
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(
            cfg.tick_interval,
            Duration::from_secs(DEFAULT_TICK_INTERVAL_S)
        );
        assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
        assert_eq!(cfg.repairs_per_window, DEFAULT_REPAIRS_PER_WINDOW);
        assert_eq!(
            cfg.repair_window,
            Duration::from_secs(DEFAULT_REPAIR_WINDOW_S)
        );
    }

    #[test]
    fn explicit_disable_is_honored() {
        let cfg = read_config_from("network:\n  management_link_guardian:\n    enabled: false\n");
        assert!(!cfg.enabled);
    }

    #[test]
    fn explicit_tunables_parse_and_floor() {
        let body = "network:\n  management_link_guardian:\n    tick_interval_s: 15\n    fail_threshold: 3\n    repairs_per_window: 8\n    repair_window_s: 300\n";
        let cfg = read_config_from(body);
        assert_eq!(cfg.tick_interval, Duration::from_secs(15));
        assert_eq!(cfg.fail_threshold, 3);
        assert_eq!(cfg.repairs_per_window, 8);
        assert_eq!(cfg.repair_window, Duration::from_secs(300));
        // Zeros floor to 1 so the guardian can never spin or treat 0 as a cap.
        let zero = read_config_from(
            "network:\n  management_link_guardian:\n    tick_interval_s: 0\n    fail_threshold: 0\n    repairs_per_window: 0\n    repair_window_s: 0\n",
        );
        assert_eq!(zero.tick_interval, Duration::from_secs(1));
        assert_eq!(zero.fail_threshold, 1);
        assert_eq!(zero.repairs_per_window, 1);
        assert_eq!(zero.repair_window, Duration::from_secs(1));
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        let cfg = read_config_from(": : : not yaml");
        assert!(cfg.enabled);
        assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
    }

    #[test]
    fn fast_window_zero_disables_fast_path() {
        let cfg = read_config_from(
            "network:\n  management_link_guardian:\n    fast_initial_window_s: 0\n",
        );
        assert_eq!(cfg.fast_initial_window, Duration::ZERO);
        assert_eq!(cfg.effective_interval(Duration::ZERO), cfg.tick_interval);
    }

    #[test]
    fn effective_interval_fast_then_steady() {
        let cfg = MgmtGuardianConfig::default();
        assert_eq!(
            cfg.effective_interval(Duration::from_secs(0)),
            cfg.fast_initial_tick
        );
        assert_eq!(
            cfg.effective_interval(Duration::from_secs(59)),
            cfg.fast_initial_tick
        );
        assert_eq!(
            cfg.effective_interval(Duration::from_secs(60)),
            cfg.tick_interval
        );
    }

    fn guardian() -> MgmtLinkGuardian {
        MgmtLinkGuardian::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ))
    }

    #[tokio::test]
    async fn due_when_never_ticked_then_throttled() {
        let g = guardian();
        let now = Instant::now();
        assert!(g.due(Duration::from_secs(30), now));
        let mut g2 = guardian();
        g2.last_tick = Some(now);
        assert!(!g2.due(Duration::from_secs(30), now + Duration::from_secs(10)));
        assert!(g2.due(Duration::from_secs(30), now + Duration::from_secs(31)));
    }

    #[tokio::test]
    async fn repairs_in_window_prunes_old_entries() {
        let mut g = guardian();
        let t0 = Instant::now();
        let window = Duration::from_secs(600);
        g.repair_times.push_back(t0);
        g.repair_times.push_back(t0 + Duration::from_secs(100));
        // Both inside the window.
        assert_eq!(
            g.repairs_in_window(t0 + Duration::from_secs(200), window),
            2
        );
        // At t0+650 the first entry (age 650s) ages out past the 600s window but
        // the second (age 550s) is still inside it.
        assert_eq!(
            g.repairs_in_window(t0 + Duration::from_secs(650), window),
            1
        );
    }

    #[test]
    fn sidecar_round_trips_through_the_atomic_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mgmt-link.json");
        let snap = MgmtLinkSidecar {
            version: MGMT_LINK_SIDECAR_VERSION,
            state: "degraded".to_string(),
            iface: "eth0".to_string(),
            transport: "ethernet".to_string(),
            backend: "networkd".to_string(),
            carrier: true,
            has_lease: true,
            gateway_reachable: false,
            repairing: true,
            last_rung: Some("renew_dhcp".to_string()),
            last_repair_at_unix: Some(1_717_000_000),
            repairs_in_window: 2,
            updated_at_unix: 1_717_000_001,
        };
        write_json_atomic(&path, &snap, 0o644).unwrap();
        let reloaded: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(reloaded["state"], "degraded");
        assert_eq!(reloaded["gateway_reachable"], false);
        assert_eq!(reloaded["last_rung"], "renew_dhcp");
        assert_eq!(reloaded["repairs_in_window"], 2);
        // No leftover tmp sibling.
        assert!(!dir.path().join("mgmt-link.tmp").exists());
    }

    #[tokio::test]
    async fn tick_is_inert_on_dev_host() {
        // On a non-Linux dev host the tick is a no-op; on Linux CI it stays inert
        // (no `ip`, or no managed backend) and must never panic.
        let mut g = guardian();
        g.tick().await;
    }
}
