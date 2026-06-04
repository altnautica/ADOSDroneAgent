//! Onboard-WiFi heartbeat reach-back (the last-resort management fallback).
//!
//! The rigs run management over wired Ethernet by default. When that wire is
//! unplugged or its port dies the box can vanish from the GCS entirely, because
//! the onboard WiFi is not used as a fallback. This reconciler watches the
//! wired primary and, when it is physically down for a sustained window while
//! an onboard WiFi has a usable path, declares a **heartbeat-only** reach-back:
//! it does not route packets or carry the data plane — it surfaces the degraded
//! mode (`mgmtLinkMode = wifi_heartbeat`) on the heartbeat so the GCS keeps
//! seeing the box and shows the operator that video and full telemetry are
//! unavailable, while the existing status-push / command-receive plumbing keeps
//! working over whatever interface holds the route.
//!
//! It composes with the management-link guardian: the guardian reconciles the
//! link while it physically exists; this reach-back is the last resort when the
//! wired link is gone. It has hysteresis on both edges (a transient blip must
//! not trigger a failover; the primary must be confirmed healthy before the
//! demotion is undone). It depends on the stable-MAC pin so a no-efuse onboard
//! adapter stays addressable across the failover.
//!
//! Default-ON, configurable under `network.mgmt_failover`. The pure decision
//! logic and config parsing are unit-tested on every host; the OS edges are
//! Linux-only and the tick is an inert no-op on a non-Linux dev host.

use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;

#[cfg(target_os = "linux")]
use crate::mgmt_link_guardian::detection::{self, Transport};

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

/// The event kind recorded on a reach-back transition. Bland and reader-facing.
pub const FAILOVER_KIND: &str = "link.mgmt_failover";

/// The `/run/ados` sidecar the Python heartbeat reads to surface the mode.
#[cfg(target_os = "linux")]
const SIDECAR_PATH: &str = "/run/ados/mgmt-failover.json";

/// Default sustained-down window before failing over to the WiFi heartbeat.
/// Longer than a transient cable blip so a momentary drop never demotes.
const DEFAULT_DOWN_DEBOUNCE_S: u64 = 20;
/// Default sustained-healthy window before restoring the primary, so a flapping
/// wired link does not bounce the mode.
const DEFAULT_RECOVER_DEBOUNCE_S: u64 = 15;
/// Default reconcile cadence (roughly one monitor pass).
const DEFAULT_TICK_INTERVAL_S: u64 = 5;

/// Which management link the box is currently reachable over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MgmtLinkMode {
    /// The wired primary is up (or the box has no wired primary): normal.
    Primary,
    /// The wired primary is gone; the onboard WiFi is the heartbeat reach-back.
    WifiHeartbeat,
    /// The wired primary is gone and there is no usable WiFi reach-back.
    NoReachback,
}

impl MgmtLinkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MgmtLinkMode::Primary => "primary",
            MgmtLinkMode::WifiHeartbeat => "wifi_heartbeat",
            MgmtLinkMode::NoReachback => "none",
        }
    }
}

/// What the hysteresis machine decided this step. Pure so the two-edge debounce
/// is unit-tested without a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverTransition {
    /// No mode change this step.
    None,
    /// Moved into a non-primary reach-back mode (or changed reach-back mode).
    Entered,
    /// Restored the wired primary.
    Exited,
}

/// Configuration, read from `network.mgmt_failover`. Default-ON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MgmtFailoverConfig {
    pub enabled: bool,
    pub down_debounce: Duration,
    pub recover_debounce: Duration,
    pub tick_interval: Duration,
}

impl Default for MgmtFailoverConfig {
    fn default() -> Self {
        MgmtFailoverConfig {
            enabled: true,
            down_debounce: Duration::from_secs(DEFAULT_DOWN_DEBOUNCE_S),
            recover_debounce: Duration::from_secs(DEFAULT_RECOVER_DEBOUNCE_S),
            tick_interval: Duration::from_secs(DEFAULT_TICK_INTERVAL_S),
        }
    }
}

/// Parse `network.mgmt_failover` out of a config body. Absent / malformed →
/// enabled defaults so the reach-back is on out of the box.
pub fn read_config_from(text: &str) -> MgmtFailoverConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        mgmt_failover: Option<Failover>,
    }
    #[derive(serde::Deserialize)]
    struct Failover {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        down_debounce_s: Option<u64>,
        #[serde(default)]
        recover_debounce_s: Option<u64>,
        #[serde(default)]
        tick_interval_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.mgmt_failover {
            Some(f) => MgmtFailoverConfig {
                enabled: f.enabled,
                down_debounce: Duration::from_secs(
                    f.down_debounce_s.unwrap_or(DEFAULT_DOWN_DEBOUNCE_S),
                ),
                recover_debounce: Duration::from_secs(
                    f.recover_debounce_s.unwrap_or(DEFAULT_RECOVER_DEBOUNCE_S),
                ),
                tick_interval: Duration::from_secs(
                    f.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1),
                ),
            },
            None => MgmtFailoverConfig::default(),
        },
        Err(_) => MgmtFailoverConfig::default(),
    }
}

#[cfg(target_os = "linux")]
fn read_config() -> MgmtFailoverConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => MgmtFailoverConfig::default(),
    }
}

/// The reach-back reconciler. Holds the current mode + the two-edge debounce
/// instants. Read only on the Linux tick (and in tests).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct MgmtFailover {
    mode: MgmtLinkMode,
    down_since: Option<Instant>,
    healthy_since: Option<Instant>,
    last_tick: Option<Instant>,
    failover_iface: Option<String>,
    events: EventEmitter,
}

impl MgmtFailover {
    pub fn new(events: EventEmitter) -> Self {
        MgmtFailover {
            mode: MgmtLinkMode::Primary,
            down_since: None,
            healthy_since: None,
            last_tick: None,
            failover_iface: None,
            events,
        }
    }

    pub fn mode(&self) -> MgmtLinkMode {
        self.mode
    }

    #[cfg(target_os = "linux")]
    fn due(&self, interval: Duration, now: Instant) -> bool {
        match self.last_tick {
            None => true,
            Some(last) => now.duration_since(last) >= interval,
        }
    }

    /// Pure two-edge hysteresis. Folds this step's `primary_down` /
    /// `wifi_available` into the mode, debouncing both edges so neither flaps.
    /// Returns the transition for the event emitter. Pure so the debounce is
    /// tested without a clock.
    #[cfg(any(target_os = "linux", test))]
    fn step(
        &mut self,
        primary_down: bool,
        wifi_available: bool,
        down_debounce: Duration,
        recover_debounce: Duration,
        now: Instant,
    ) -> FailoverTransition {
        if !primary_down {
            // Primary healthy. Restore it only after a sustained-healthy window.
            self.down_since = None;
            if self.mode == MgmtLinkMode::Primary {
                self.healthy_since = None;
                return FailoverTransition::None;
            }
            let healthy_since = *self.healthy_since.get_or_insert(now);
            if now.duration_since(healthy_since) >= recover_debounce {
                self.healthy_since = None;
                self.mode = MgmtLinkMode::Primary;
                return FailoverTransition::Exited;
            }
            return FailoverTransition::None;
        }

        // Primary down. Fail over only after a sustained-down window.
        self.healthy_since = None;
        let down_since = *self.down_since.get_or_insert(now);
        if now.duration_since(down_since) < down_debounce {
            return FailoverTransition::None;
        }
        let target = if wifi_available {
            MgmtLinkMode::WifiHeartbeat
        } else {
            MgmtLinkMode::NoReachback
        };
        if self.mode != target {
            self.mode = target;
            return FailoverTransition::Entered;
        }
        FailoverTransition::None
    }

    /// One reach-back tick: throttle, read the wired primary's carrier, and (on
    /// a sustained-down primary) check the onboard WiFi for a usable path, then
    /// fold both through the hysteresis and mirror the mode to the sidecar.
    #[cfg(target_os = "linux")]
    pub async fn tick(&mut self) {
        let cfg = read_config();
        if !cfg.enabled {
            return;
        }
        let now = Instant::now();
        if !self.due(cfg.tick_interval, now) {
            return;
        }
        self.last_tick = Some(now);

        let candidates = detection::collect_candidates().await;
        // Wired primaries: real, non-injection, non-virtual Ethernet interfaces.
        let wired: Vec<&detection::IfaceCandidate> = candidates
            .iter()
            .filter(|c| c.transport == Transport::Ethernet && !c.is_injection && !c.is_virtual)
            .collect();

        // No wired primary → management is over WiFi normally; there is no
        // reach-back concept. Stay Primary and surface it.
        if wired.is_empty() {
            self.set_mode_primary_if_needed(now, cfg);
            self.write_sidecar();
            return;
        }

        // Primary is up when any wired interface has carrier.
        let mut primary_up = false;
        for c in &wired {
            if iface_carrier(&c.name).await {
                primary_up = true;
                break;
            }
        }
        let primary_down = !primary_up;

        // Only probe the WiFi fallback when the primary is down (keeps the
        // healthy path cheap). A usable reach-back has carrier + a routable
        // lease so the heartbeat can egress.
        let mut wifi_available = false;
        let mut wifi_iface: Option<String> = None;
        if primary_down {
            for c in candidates
                .iter()
                .filter(|c| c.transport == Transport::Wifi && !c.is_injection && !c.is_virtual)
            {
                let s = detection::collect_signals(&c.name).await;
                if s.carrier && s.has_lease {
                    wifi_available = true;
                    wifi_iface = Some(c.name.clone());
                    break;
                }
            }
        }

        let transition = self.step(
            primary_down,
            wifi_available,
            cfg.down_debounce,
            cfg.recover_debounce,
            now,
        );
        self.failover_iface = if self.mode == MgmtLinkMode::WifiHeartbeat {
            wifi_iface
        } else {
            None
        };

        match transition {
            FailoverTransition::None => {}
            FailoverTransition::Entered => {
                let reason = if self.mode == MgmtLinkMode::WifiHeartbeat {
                    "primary_carrier_down"
                } else {
                    "no_wifi_reachback"
                };
                tracing::warn!(
                    mode = self.mode.as_str(),
                    iface = ?self.failover_iface,
                    "mgmt_failover_entered"
                );
                self.events.emit(
                    FAILOVER_KIND,
                    ados_protocol::logd::Level::Warn,
                    failover_detail("entered", self.mode, self.failover_iface.as_deref(), reason),
                );
            }
            FailoverTransition::Exited => {
                tracing::info!("mgmt_failover_restored_primary");
                self.events.emit(
                    FAILOVER_KIND,
                    ados_protocol::logd::Level::Info,
                    failover_detail("exited", self.mode, None, "primary_restored"),
                );
            }
        }
        self.write_sidecar();
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn tick(&mut self) {}

    /// Restore the Primary mode (used on a box with no wired primary). Emits an
    /// exit event if we were previously in a reach-back mode.
    #[cfg(target_os = "linux")]
    fn set_mode_primary_if_needed(&mut self, _now: Instant, _cfg: MgmtFailoverConfig) {
        if self.mode != MgmtLinkMode::Primary {
            self.mode = MgmtLinkMode::Primary;
            self.down_since = None;
            self.healthy_since = None;
            self.failover_iface = None;
            self.events.emit(
                FAILOVER_KIND,
                ados_protocol::logd::Level::Info,
                failover_detail("exited", self.mode, None, "no_wired_primary"),
            );
        }
    }

    #[cfg(target_os = "linux")]
    fn write_sidecar(&self) {
        #[derive(serde::Serialize)]
        struct Snap<'a> {
            mgmt_link_mode: &'a str,
            mgmt_failover_iface: Option<&'a str>,
            mgmt_failover_reason: Option<&'a str>,
            updated_at_unix: u64,
        }
        let reason = match self.mode {
            MgmtLinkMode::Primary => None,
            MgmtLinkMode::WifiHeartbeat => Some("primary_carrier_down"),
            MgmtLinkMode::NoReachback => Some("no_wifi_reachback"),
        };
        let snap = Snap {
            mgmt_link_mode: self.mode.as_str(),
            mgmt_failover_iface: self.failover_iface.as_deref(),
            mgmt_failover_reason: reason,
            updated_at_unix: now_unix(),
        };
        if let Err(e) = write_json_atomic(std::path::Path::new(SIDECAR_PATH), &snap, 0o644) {
            tracing::debug!(error = %e, "mgmt_failover sidecar write failed");
        }
    }
}

/// Build the `link.mgmt_failover` detail map. Bland fields. Pure.
#[cfg(any(target_os = "linux", test))]
fn failover_detail(
    state: &str,
    mode: MgmtLinkMode,
    iface: Option<&str>,
    reason: &str,
) -> ados_protocol::logd::Fields {
    use ados_protocol::logd::{Fields, Value as MpVal};
    let mut d = Fields::new();
    d.insert("state".to_string(), MpVal::from(state));
    d.insert("mode".to_string(), MpVal::from(mode.as_str()));
    if let Some(i) = iface {
        d.insert("iface".to_string(), MpVal::from(i));
    }
    d.insert("reason".to_string(), MpVal::from(reason));
    d
}

/// True when an interface's carrier sysfs file reads up.
#[cfg(target_os = "linux")]
async fn iface_carrier(iface: &str) -> bool {
    tokio::fs::read_to_string(format!("/sys/class/net/{}/carrier", iface))
        .await
        .map(|s| detection::parse_carrier(&s))
        .unwrap_or(false)
}

/// Seconds since the Unix epoch, or 0.
#[cfg(target_os = "linux")]
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Atomically write `value` as JSON to `path` (tmp → fsync → rename).
#[cfg(target_os = "linux")]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> MgmtFailover {
        MgmtFailover::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ))
    }

    #[test]
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(
            cfg.down_debounce,
            Duration::from_secs(DEFAULT_DOWN_DEBOUNCE_S)
        );
        assert_eq!(
            cfg.recover_debounce,
            Duration::from_secs(DEFAULT_RECOVER_DEBOUNCE_S)
        );
    }

    #[test]
    fn explicit_disable_and_tunables() {
        let cfg = read_config_from(
            "network:\n  mgmt_failover:\n    enabled: false\n    down_debounce_s: 30\n    recover_debounce_s: 10\n",
        );
        assert!(!cfg.enabled);
        assert_eq!(cfg.down_debounce, Duration::from_secs(30));
        assert_eq!(cfg.recover_debounce, Duration::from_secs(10));
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        assert!(read_config_from(": : : not yaml").enabled);
    }

    #[tokio::test]
    async fn down_debounce_before_failover() {
        let mut m = machine();
        let t0 = Instant::now();
        let down = Duration::from_secs(20);
        let rec = Duration::from_secs(15);
        // Primary just went down: still debouncing, no failover yet.
        assert_eq!(m.step(true, true, down, rec, t0), FailoverTransition::None);
        assert_eq!(m.mode(), MgmtLinkMode::Primary);
        // Still inside the window.
        assert_eq!(
            m.step(true, true, down, rec, t0 + Duration::from_secs(10)),
            FailoverTransition::None
        );
        // Sustained past the window → fail over to the WiFi heartbeat.
        assert_eq!(
            m.step(true, true, down, rec, t0 + Duration::from_secs(21)),
            FailoverTransition::Entered
        );
        assert_eq!(m.mode(), MgmtLinkMode::WifiHeartbeat);
    }

    #[tokio::test]
    async fn no_wifi_yields_no_reachback() {
        let mut m = machine();
        let t0 = Instant::now();
        let down = Duration::from_secs(20);
        let rec = Duration::from_secs(15);
        m.step(true, false, down, rec, t0);
        assert_eq!(
            m.step(true, false, down, rec, t0 + Duration::from_secs(21)),
            FailoverTransition::Entered
        );
        assert_eq!(m.mode(), MgmtLinkMode::NoReachback);
    }

    #[tokio::test]
    async fn recover_debounce_before_restore_no_flap() {
        let mut m = machine();
        let t0 = Instant::now();
        let down = Duration::from_secs(20);
        let rec = Duration::from_secs(15);
        // Fail over first.
        m.step(true, true, down, rec, t0);
        m.step(true, true, down, rec, t0 + Duration::from_secs(21));
        assert_eq!(m.mode(), MgmtLinkMode::WifiHeartbeat);
        // Primary comes back: must hold healthy for the recover window before
        // restoring (no flap on a brief carrier blip).
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(m.step(false, true, down, rec, t1), FailoverTransition::None);
        assert_eq!(m.mode(), MgmtLinkMode::WifiHeartbeat);
        // A re-drop inside the recover window resets the healthy timer.
        assert_eq!(
            m.step(true, true, down, rec, t1 + Duration::from_secs(5)),
            FailoverTransition::None
        );
        // Sustained healthy past the recover window → restore.
        let t2 = t1 + Duration::from_secs(10);
        m.step(false, true, down, rec, t2);
        assert_eq!(
            m.step(false, true, down, rec, t2 + Duration::from_secs(16)),
            FailoverTransition::Exited
        );
        assert_eq!(m.mode(), MgmtLinkMode::Primary);
    }

    #[test]
    fn mode_strings_are_bland() {
        assert_eq!(MgmtLinkMode::Primary.as_str(), "primary");
        assert_eq!(MgmtLinkMode::WifiHeartbeat.as_str(), "wifi_heartbeat");
        assert_eq!(MgmtLinkMode::NoReachback.as_str(), "none");
    }

    #[test]
    fn failover_detail_is_bland() {
        let d = failover_detail(
            "entered",
            MgmtLinkMode::WifiHeartbeat,
            Some("wlan0"),
            "primary_carrier_down",
        );
        assert_eq!(d.get("state").and_then(|v| v.as_str()), Some("entered"));
        assert_eq!(
            d.get("mode").and_then(|v| v.as_str()),
            Some("wifi_heartbeat")
        );
        assert_eq!(d.get("iface").and_then(|v| v.as_str()), Some("wlan0"));
    }
}
