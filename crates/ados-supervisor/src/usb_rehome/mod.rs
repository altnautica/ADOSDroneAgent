//! USB-rehome self-heal (last-resort radio-adapter recovery).
//!
//! When a WFB adapter is on a slow USB port AND its RF is unverified (TX
//! advancing, zero confirmed reception) — both held across a confirm window —
//! the agent unbinds and rebinds the USB device for a clean re-enumeration that
//! can land it on a faster lane, then re-checks. It is the recovery action the
//! transmit-verification signals were missing.
//!
//! The decision (trigger debounce, bounded retry/cooldown, the fail-closed
//! never-touch-the-control-interface guard) lives here; the actual stop → rebind
//! → start sequence is driven by the supervisor, because only it owns the radio
//! unit lifecycle. `decide()` returns a `RehomePlan` when an attempt is
//! authorized; the supervisor stops the radio unit, calls `execute_rebind`,
//! starts it again, and the next `decide()` re-checks the fresh stats to confirm.
//!
//! Default-ON, configurable under `network.usb_rehome`. The pure logic and
//! config parsing are unit-tested on every host; the sysfs ops are Linux-only.

pub mod camera;
pub mod machine;
pub mod topo;

use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;

use machine::{RehomeMachine, RehomeTrigger};

#[cfg(target_os = "linux")]
use machine::{usb_rehome_detail, RehomeStep};
#[cfg(target_os = "linux")]
use topo::GuardVerdict;

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

/// Fixed wait between rehome attempts.
///
/// This replaces a three-attempt budget and an escalating `[10, 30, 60]` s
/// schedule. The budget latched an `exhausted` state that could not clear
/// without the attempts it stopped, so a genuinely wedged radio — the fault
/// this self-heal exists to repair — was abandoned after ninety seconds until
/// someone rebooted the vehicle. There is no budget now; recovery keeps going.
///
/// 60 s rather than the 2-5 s a socket reconnect would use, and deliberately
/// so: a rehome is a real USB unbind/rebind that stops the radio for over a
/// second and re-enumerates the bus. Retrying that every few seconds forever
/// would be its own fault. 60 s was the terminal value of the old schedule, so
/// a permanent fault now paces exactly as the old code's last rung did — it
/// simply never stops.
const DEFAULT_COOLDOWN_S: u64 = 60;
/// Default sustained-healthy window that closes the episode (anti-flap).
const DEFAULT_HEALTHY_RESET_S: u64 = 120;
/// Default reconcile cadence.
const DEFAULT_TICK_INTERVAL_S: u64 = 5;

/// Settle between the unbind and the bind so the device node fully drops.
#[cfg(target_os = "linux")]
const REHOME_SETTLE_UNBIND: Duration = Duration::from_millis(1500);
/// Bounded wait for the interface to re-enumerate after the bind.
#[cfg(target_os = "linux")]
const REHOME_REENUM_CEILING: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const REHOME_REENUM_STEP: Duration = Duration::from_millis(200);

#[cfg(target_os = "linux")]
const SIDECAR_PATH: &str = "/run/ados/usb-rehome.json";

/// Schema version of the `usb-rehome.json` sidecar. Bump on an incompatible
/// field-set change; a reader compares it best-effort via
/// `ados_protocol::sidecar::check_sidecar_version`. Kept in step with the
/// registry in `contracts.toml`. Gated to the platforms that build the writer
/// (Linux) or the version test.
#[cfg(any(target_os = "linux", test))]
const USB_REHOME_SIDECAR_VERSION: u16 = 1;

#[cfg(target_os = "linux")]
const WFB_STATS_PATH: &str = "/run/ados/wfb-stats.json";
/// Max age of `wfb-stats.json` before its signals are treated as stale. The
/// radio rewrites the sidecar on every ~1 Hz stats cycle, so a file older than
/// this means the writer (the radio service) is stopped or crashed — its last
/// `usb_degraded` / `rf_unverified` flags are frozen, and acting on them would
/// keep authorizing USB rebinds against a radio that is no longer running. A
/// stale sidecar reads as "no signals" (no rehome) rather than frozen-degraded.
#[cfg(target_os = "linux")]
const WFB_STATS_FRESH_CEILING: Duration = Duration::from_secs(30);
/// The USB core driver's bind/unbind sysfs attributes.
#[cfg(target_os = "linux")]
const USB_UNBIND_PATH: &str = "/sys/bus/usb/drivers/usb/unbind";
#[cfg(target_os = "linux")]
const USB_BIND_PATH: &str = "/sys/bus/usb/drivers/usb/bind";

/// Configuration, read from `network.usb_rehome`. Default-ON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbRehomeConfig {
    pub enabled: bool,
    /// Fixed wait between attempts. There is no attempt ceiling: see
    /// [`DEFAULT_COOLDOWN_S`].
    pub cooldown: Duration,
    pub healthy_reset: Duration,
    pub tick_interval: Duration,
}

impl Default for UsbRehomeConfig {
    fn default() -> Self {
        UsbRehomeConfig {
            enabled: true,
            cooldown: Duration::from_secs(DEFAULT_COOLDOWN_S),
            healthy_reset: Duration::from_secs(DEFAULT_HEALTHY_RESET_S),
            tick_interval: Duration::from_secs(DEFAULT_TICK_INTERVAL_S),
        }
    }
}

/// Parse `network.usb_rehome`. Absent / malformed → enabled defaults.
pub fn read_config_from(text: &str) -> UsbRehomeConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        usb_rehome: Option<Rehome>,
    }
    #[derive(serde::Deserialize)]
    struct Rehome {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        cooldown_s: Option<u64>,
        /// Legacy. Read so a node that tuned the old escalating schedule keeps
        /// the pacing it asked for; the fixed cooldown becomes the largest
        /// value in it, which was that schedule's steady state.
        #[serde(default)]
        cooldown_schedule_s: Option<Vec<u64>>,
        #[serde(default)]
        healthy_reset_s: Option<u64>,
        #[serde(default)]
        tick_interval_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.usb_rehome {
            Some(r) => {
                let cooldown_s = r
                    .cooldown_s
                    .or_else(|| {
                        r.cooldown_schedule_s
                            .as_deref()
                            .and_then(|v| v.iter().copied().max())
                    })
                    .unwrap_or(DEFAULT_COOLDOWN_S)
                    .max(1);
                UsbRehomeConfig {
                    enabled: r.enabled,
                    cooldown: Duration::from_secs(cooldown_s),
                    healthy_reset: Duration::from_secs(
                        r.healthy_reset_s.unwrap_or(DEFAULT_HEALTHY_RESET_S).max(1),
                    ),
                    tick_interval: Duration::from_secs(
                        r.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1),
                    ),
                }
            }
            None => UsbRehomeConfig::default(),
        },
        Err(_) => UsbRehomeConfig::default(),
    }
}

#[cfg(target_os = "linux")]
fn read_config() -> UsbRehomeConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => UsbRehomeConfig::default(),
    }
}

/// The plan the supervisor executes: stop `unit`, rebind `bind_id`, start `unit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RehomePlan {
    pub unit: &'static str,
    pub iface: String,
    pub bind_id: String,
    pub before_speed_mbps: Option<u32>,
    pub attempt: u32,
}

/// The signals read from the radio's `wfb-stats.json` sidecar.
#[cfg(target_os = "linux")]
struct WfbSignals {
    iface: String,
    profile: String,
    usb_degraded: bool,
    rf_unverified: bool,
    usb_speed_mbps: Option<u32>,
}

/// The USB-rehome reconciler. Owns the trigger + retry machine; the supervisor
/// drives it from the monitor pass.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct UsbRehome {
    trigger: RehomeTrigger,
    machine: RehomeMachine,
    last_tick: Option<Instant>,
    /// True once the guard has refused the current fault episode, so the
    /// supervisor stops re-resolving the topology every tick. Cleared when the
    /// adapter verifies healthy.
    guard_blocked: bool,
    last_result: &'static str,
    events: EventEmitter,
}

impl UsbRehome {
    pub fn new(events: EventEmitter) -> Self {
        UsbRehome {
            trigger: RehomeTrigger::new(),
            machine: RehomeMachine::new(),
            last_tick: None,
            guard_blocked: false,
            last_result: "idle",
            events,
        }
    }

    #[cfg(target_os = "linux")]
    fn due(&self, interval: Duration, now: Instant) -> bool {
        match self.last_tick {
            None => true,
            Some(last) => now.duration_since(last) >= interval,
        }
    }

    /// One reconcile decision. Reads the radio stats, runs the trigger + retry
    /// machine + the fail-closed guard, mirrors the state to the sidecar, and
    /// returns a `RehomePlan` only when an attempt is authorized.
    #[cfg(target_os = "linux")]
    pub async fn decide(&mut self) -> Option<RehomePlan> {
        let cfg = read_config();
        if !cfg.enabled {
            return None;
        }
        let now = Instant::now();
        if !self.due(cfg.tick_interval, now) {
            return None;
        }
        self.last_tick = Some(now);

        let Some(sig) = read_wfb_signals().await else {
            // No radio stats (radio not running / not a radio profile): nothing
            // to rehome.
            return None;
        };
        let unit = match sig.profile.as_str() {
            "drone" => "ados-wfb",
            "ground_station" => "ados-wfb-rx",
            _ => return None,
        };

        let cond = sig.usb_degraded && sig.rf_unverified;
        let verified_healthy = !sig.usb_degraded && !sig.rf_unverified;
        if verified_healthy {
            self.guard_blocked = false;
        }
        let armed = self.trigger.observe(cond, now);

        let step = self.machine.step(
            armed,
            verified_healthy,
            cfg.cooldown,
            cfg.healthy_reset,
            now,
        );

        let plan = match step {
            RehomeStep::Recovered => {
                self.guard_blocked = false;
                self.last_result = "success";
                self.events.emit(
                    machine::USB_REHOME_KIND,
                    ados_protocol::logd::Level::Info,
                    usb_rehome_detail(
                        "success",
                        &sig.iface,
                        "",
                        0,
                        cfg.cooldown.as_secs(),
                        None,
                        sig.usb_speed_mbps,
                        None,
                    ),
                );
                None
            }
            RehomeStep::Cooldown { .. } => {
                // Still failing, still trying. Announce once per episode at the
                // point the old code would have parked, so an operator sees a
                // vehicle that has been retrying for a while rather than
                // nothing at all — the previous `exhausted` warning was the
                // only loud signal here, and it was attached to giving up.
                if self.last_result != "retry" {
                    self.events.emit(
                        machine::USB_REHOME_KIND,
                        ados_protocol::logd::Level::Warn,
                        usb_rehome_detail(
                            "retry",
                            &sig.iface,
                            "",
                            self.machine.attempts(),
                            cfg.cooldown.as_secs(),
                            None,
                            sig.usb_speed_mbps,
                            None,
                        ),
                    );
                }
                self.last_result = "retry";
                None
            }
            RehomeStep::Idle => {
                if verified_healthy {
                    self.last_result = "idle";
                }
                None
            }
            RehomeStep::Attempt { index } => {
                if self.guard_blocked {
                    // Already refused for this fault: do not re-attempt or
                    // re-resolve the topology; refund the budget and hold.
                    self.machine.refund_attempt();
                    None
                } else {
                    self.authorize_attempt(unit, &sig, index, cfg.cooldown.as_secs())
                        .await
                }
            }
        };

        self.write_sidecar(self.machine.attempts(), cfg.cooldown.as_secs());
        plan
    }

    /// Resolve the topology + run the guard for an authorized attempt. Returns a
    /// plan when the guard allows, else refunds the budget and records the block.
    #[cfg(target_os = "linux")]
    async fn authorize_attempt(
        &mut self,
        unit: &'static str,
        sig: &WfbSignals,
        index: u32,
        cooldown_s: u64,
    ) -> Option<RehomePlan> {
        let Some(target) = topo::resolve_usb_topo(&sig.iface).await else {
            // The WFB interface is not USB-backed: nothing to rebind.
            self.machine.refund_attempt();
            self.guard_blocked = true;
            self.last_result = "guard_blocked";
            self.emit_guard_blocked(&sig.iface, "", "not_usb", cooldown_s);
            return None;
        };
        let default_iface = crate::mgmt_link_guardian::detection::default_route_iface().await;
        let control = topo::resolve_control_path(default_iface.as_deref()).await;
        let verdict = topo::guard_verdict(&target, &control);
        if verdict != GuardVerdict::Allow {
            self.machine.refund_attempt();
            self.guard_blocked = true;
            self.last_result = "guard_blocked";
            self.emit_guard_blocked(
                &sig.iface,
                &target.bind_id,
                verdict.reason().unwrap_or("blocked"),
                cooldown_s,
            );
            return None;
        }
        self.last_result = "rehoming";
        self.events.emit(
            machine::USB_REHOME_KIND,
            ados_protocol::logd::Level::Info,
            usb_rehome_detail(
                "rehoming",
                &sig.iface,
                &target.bind_id,
                index,
                cooldown_s,
                sig.usb_speed_mbps,
                None,
                None,
            ),
        );
        Some(RehomePlan {
            unit,
            iface: sig.iface.clone(),
            bind_id: target.bind_id,
            before_speed_mbps: sig.usb_speed_mbps,
            attempt: index,
        })
    }

    #[cfg(target_os = "linux")]
    fn emit_guard_blocked(&self, iface: &str, bind_id: &str, reason: &str, cooldown_s: u64) {
        self.events.emit(
            machine::USB_REHOME_KIND,
            ados_protocol::logd::Level::Warn,
            usb_rehome_detail(
                "guard_blocked",
                iface,
                bind_id,
                0,
                cooldown_s,
                None,
                None,
                Some(reason),
            ),
        );
    }

    #[cfg(target_os = "linux")]
    fn write_sidecar(&self, attempts: u32, cooldown_s: u64) {
        #[derive(serde::Serialize)]
        struct Snap<'a> {
            version: u16,
            usb_rehome_state: &'a str,
            usb_rehome_attempts: u32,
            usb_rehome_cooldown_s: u64,
            usb_rehome_last_result: &'a str,
            updated_at_unix: u64,
        }
        // The renderable state: idle / rehoming / guard_blocked. There is no
        // longer an `exhausted` state to render, because recovery no longer
        // reaches one -- `usb_rehome_attempts` is what tells an operator how
        // long this has been going on.
        let state = match self.last_result {
            "rehoming" | "retry" => "rehoming",
            "guard_blocked" => "guard_blocked",
            _ => "idle",
        };
        let snap = Snap {
            version: USB_REHOME_SIDECAR_VERSION,
            usb_rehome_state: state,
            usb_rehome_attempts: attempts,
            usb_rehome_cooldown_s: cooldown_s,
            usb_rehome_last_result: self.last_result,
            updated_at_unix: now_unix(),
        };
        if let Err(e) = write_json_atomic(std::path::Path::new(SIDECAR_PATH), &snap, 0o644) {
            tracing::debug!(error = %e, "usb_rehome sidecar write failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn decide(&mut self) -> Option<RehomePlan> {
        None
    }
}

/// Execute the sysfs unbind/rebind for an authorized plan. Called by the
/// supervisor BETWEEN stopping and starting the radio unit, so no live injector
/// races the rebind. Best-effort; a sysfs write failure is logged and the radio
/// restart still re-probes the adapter.
#[cfg(target_os = "linux")]
pub async fn execute_rebind(plan: &RehomePlan) {
    tracing::warn!(iface = %plan.iface, bind_id = %plan.bind_id, attempt = plan.attempt, "usb_rehome_unbind_rebind");
    if let Err(e) = sysfs_write(USB_UNBIND_PATH, &plan.bind_id).await {
        tracing::warn!(error = %e, "usb_rehome unbind failed");
    }
    tokio::time::sleep(REHOME_SETTLE_UNBIND).await;
    if let Err(e) = sysfs_write(USB_BIND_PATH, &plan.bind_id).await {
        tracing::warn!(error = %e, "usb_rehome bind failed");
    }
    // Wait (bounded) for the interface's device link to resolve again.
    let deadline = tokio::time::Instant::now() + REHOME_REENUM_CEILING;
    let link = format!("/sys/class/net/{}/device", plan.iface);
    loop {
        if tokio::fs::canonicalize(&link).await.is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(REHOME_REENUM_STEP).await;
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn execute_rebind(_plan: &RehomePlan) {}

#[cfg(target_os = "linux")]
async fn sysfs_write(path: &str, val: &str) -> std::io::Result<()> {
    tokio::fs::write(path, val).await
}

/// Read the rehome signals from the radio's `wfb-stats.json` sidecar. Returns
/// `None` when the sidecar is absent, malformed, or STALE (older than
/// [`WFB_STATS_FRESH_CEILING`]) — a frozen sidecar from a stopped/crashed radio
/// must not keep authorizing USB rebinds with its last-written degraded flags.
#[cfg(target_os = "linux")]
async fn read_wfb_signals() -> Option<WfbSignals> {
    // Freshness gate first: a writer that has stopped leaves the file's content
    // frozen but its mtime fixed, so an mtime past the ceiling means "no live
    // signals", not "the last signals are still true".
    let age = tokio::fs::metadata(WFB_STATS_PATH)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok());
    if age.map(|a| a > WFB_STATS_FRESH_CEILING).unwrap_or(true) {
        return None;
    }
    let txt = tokio::fs::read_to_string(WFB_STATS_PATH).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    // Best-effort schema-drift signal: warn (never reject) when the wfb-stats
    // sidecar was written by an agent with a different schema version. The writer
    // const lives in the `ados-radio` crate (shared with the ground-station
    // writer), so compare against the shared registry.
    let got = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("wfb-stats") {
        ados_protocol::sidecar::check_sidecar_version("wfb-stats", got, ours);
    }
    let iface = v.get("interface")?.as_str()?.to_string();
    if iface.is_empty() {
        return None;
    }
    let profile = v
        .get("profile")
        .and_then(|x| x.as_str())
        .unwrap_or("drone")
        .to_string();
    let usb_degraded = v
        .get("adapter_usb_degraded")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let rf_unverified = v
        .get("rf_unverified")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let usb_speed_mbps = v
        .get("adapter_usb_speed_mbps")
        .and_then(|x| x.as_u64())
        .map(|n| n as u32);
    Some(WfbSignals {
        iface,
        profile,
        usb_degraded,
        rf_unverified,
        usb_speed_mbps,
    })
}

#[cfg(target_os = "linux")]
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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

    #[test]
    fn usb_rehome_sidecar_version_matches_registry() {
        // The per-file const and the sidecar registry are the two sources of
        // truth for this sidecar's schema version; a drift is caught here. The
        // reader (`ados-control`'s status route) gates its drift warning on the
        // registry, so an unregistered sidecar would warn on every read.
        assert_eq!(
            USB_REHOME_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("usb-rehome").unwrap()
        );
    }

    #[test]
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(cfg.cooldown, Duration::from_secs(DEFAULT_COOLDOWN_S));
        assert_eq!(
            cfg.healthy_reset,
            Duration::from_secs(DEFAULT_HEALTHY_RESET_S)
        );
    }

    #[test]
    fn explicit_disable_and_tunables() {
        let cfg = read_config_from(
            "network:\n  usb_rehome:\n    enabled: false\n    cooldown_s: 20\n    healthy_reset_s: 90\n",
        );
        assert!(!cfg.enabled);
        assert_eq!(cfg.cooldown, Duration::from_secs(20));
        assert_eq!(cfg.healthy_reset, Duration::from_secs(90));
    }

    #[test]
    fn a_legacy_escalating_schedule_becomes_its_steady_state_value() {
        // A node in the field that tuned the old `[10, 30, 60]`-style schedule
        // asked for a particular steady-state pacing. There is no schedule any
        // more, so honour that intent by taking the largest rung rather than
        // silently reverting the node to the shipped default.
        let cfg =
            read_config_from("network:\n  usb_rehome:\n    cooldown_schedule_s: [5, 15, 45]\n");
        assert_eq!(cfg.cooldown, Duration::from_secs(45));
    }

    #[test]
    fn an_explicit_cooldown_beats_a_legacy_schedule() {
        let cfg = read_config_from(
            "network:\n  usb_rehome:\n    cooldown_s: 7\n    cooldown_schedule_s: [5, 15, 45]\n",
        );
        assert_eq!(cfg.cooldown, Duration::from_secs(7));
    }

    #[test]
    fn a_zero_or_empty_cooldown_floors_rather_than_hot_looping() {
        let cfg = read_config_from("network:\n  usb_rehome:\n    cooldown_s: 0\n");
        assert_eq!(cfg.cooldown, Duration::from_secs(1));
        let empty = read_config_from("network:\n  usb_rehome:\n    cooldown_schedule_s: []\n");
        assert_eq!(empty.cooldown, Duration::from_secs(DEFAULT_COOLDOWN_S));
    }

    #[test]
    fn a_stale_max_attempts_key_is_ignored_rather_than_rejected() {
        // The key is gone from the model. A node that still carries it must
        // load, not fail: an unparseable config would take the supervisor down
        // on exactly the nodes this change is for.
        let cfg =
            read_config_from("network:\n  usb_rehome:\n    max_attempts: 5\n    cooldown_s: 12\n");
        assert!(cfg.enabled);
        assert_eq!(cfg.cooldown, Duration::from_secs(12));
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        assert!(read_config_from(": : : not yaml").enabled);
    }
}
