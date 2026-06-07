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

/// Default attempt budget per episode.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Default escalating cooldown (seconds) between attempts.
const DEFAULT_COOLDOWN_SCHEDULE_S: [u64; 3] = [10, 30, 60];
/// Default sustained-healthy window that resets the episode budget (anti-flap).
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
#[cfg(target_os = "linux")]
const WFB_STATS_PATH: &str = "/run/ados/wfb-stats.json";
/// The USB core driver's bind/unbind sysfs attributes.
#[cfg(target_os = "linux")]
const USB_UNBIND_PATH: &str = "/sys/bus/usb/drivers/usb/unbind";
#[cfg(target_os = "linux")]
const USB_BIND_PATH: &str = "/sys/bus/usb/drivers/usb/bind";

/// Configuration, read from `network.usb_rehome`. Default-ON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbRehomeConfig {
    pub enabled: bool,
    pub max_attempts: u32,
    pub cooldown_schedule: Vec<u64>,
    pub healthy_reset: Duration,
    pub tick_interval: Duration,
}

impl Default for UsbRehomeConfig {
    fn default() -> Self {
        UsbRehomeConfig {
            enabled: true,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            cooldown_schedule: DEFAULT_COOLDOWN_SCHEDULE_S.to_vec(),
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
        max_attempts: Option<u32>,
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
                let cooldown_schedule = r
                    .cooldown_schedule_s
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| DEFAULT_COOLDOWN_SCHEDULE_S.to_vec());
                UsbRehomeConfig {
                    enabled: r.enabled,
                    max_attempts: r.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS).max(1),
                    cooldown_schedule,
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

        let schedule = cfg.cooldown_schedule.clone();
        let cooldown_for = |n: u32| {
            let idx = (n.saturating_sub(1) as usize).min(schedule.len().saturating_sub(1));
            Duration::from_secs(*schedule.get(idx).unwrap_or(&DEFAULT_COOLDOWN_SCHEDULE_S[0]))
        };

        let step = self.machine.step(
            armed,
            verified_healthy,
            cfg.max_attempts,
            cooldown_for,
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
                        cfg.max_attempts,
                        None,
                        sig.usb_speed_mbps,
                        None,
                    ),
                );
                None
            }
            RehomeStep::Exhausted => {
                if self.last_result != "exhausted" {
                    self.events.emit(
                        machine::USB_REHOME_KIND,
                        ados_protocol::logd::Level::Warn,
                        usb_rehome_detail(
                            "exhausted",
                            &sig.iface,
                            "",
                            self.machine.attempts(),
                            cfg.max_attempts,
                            None,
                            sig.usb_speed_mbps,
                            None,
                        ),
                    );
                }
                self.last_result = "exhausted";
                None
            }
            RehomeStep::Cooldown { .. } => {
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
                    self.authorize_attempt(unit, &sig, index, cfg.max_attempts)
                        .await
                }
            }
        };

        self.write_sidecar(self.machine.attempts(), cfg.max_attempts);
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
        max_attempts: u32,
    ) -> Option<RehomePlan> {
        let Some(target) = topo::resolve_usb_topo(&sig.iface).await else {
            // The WFB interface is not USB-backed: nothing to rebind.
            self.machine.refund_attempt();
            self.guard_blocked = true;
            self.last_result = "guard_blocked";
            self.emit_guard_blocked(&sig.iface, "", "not_usb", max_attempts);
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
                max_attempts,
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
                max_attempts,
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
    fn emit_guard_blocked(&self, iface: &str, bind_id: &str, reason: &str, max_attempts: u32) {
        self.events.emit(
            machine::USB_REHOME_KIND,
            ados_protocol::logd::Level::Warn,
            usb_rehome_detail(
                "guard_blocked",
                iface,
                bind_id,
                0,
                max_attempts,
                None,
                None,
                Some(reason),
            ),
        );
    }

    #[cfg(target_os = "linux")]
    fn write_sidecar(&self, attempts: u32, max_attempts: u32) {
        #[derive(serde::Serialize)]
        struct Snap<'a> {
            usb_rehome_state: &'a str,
            usb_rehome_attempts: u32,
            usb_rehome_max_attempts: u32,
            usb_rehome_last_result: &'a str,
            updated_at_unix: u64,
        }
        // The renderable state: idle / rehoming / exhausted / guard_blocked.
        let state = match self.last_result {
            "rehoming" | "retry" => "rehoming",
            "exhausted" => "exhausted",
            "guard_blocked" => "guard_blocked",
            _ => "idle",
        };
        let snap = Snap {
            usb_rehome_state: state,
            usb_rehome_attempts: attempts,
            usb_rehome_max_attempts: max_attempts,
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

/// Read the rehome signals from the radio's `wfb-stats.json` sidecar.
#[cfg(target_os = "linux")]
async fn read_wfb_signals() -> Option<WfbSignals> {
    let txt = tokio::fs::read_to_string(WFB_STATS_PATH).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
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
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(cfg.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(cfg.cooldown_schedule, vec![10, 30, 60]);
        assert_eq!(
            cfg.healthy_reset,
            Duration::from_secs(DEFAULT_HEALTHY_RESET_S)
        );
    }

    #[test]
    fn explicit_disable_and_tunables() {
        let cfg = read_config_from(
            "network:\n  usb_rehome:\n    enabled: false\n    max_attempts: 5\n    cooldown_schedule_s: [5, 15]\n    healthy_reset_s: 90\n",
        );
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_attempts, 5);
        assert_eq!(cfg.cooldown_schedule, vec![5, 15]);
        assert_eq!(cfg.healthy_reset, Duration::from_secs(90));
    }

    #[test]
    fn zero_max_attempts_floors_to_one_and_empty_schedule_defaults() {
        let cfg = read_config_from(
            "network:\n  usb_rehome:\n    max_attempts: 0\n    cooldown_schedule_s: []\n",
        );
        assert_eq!(cfg.max_attempts, 1);
        assert_eq!(cfg.cooldown_schedule, vec![10, 30, 60]);
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        assert!(read_config_from(": : : not yaml").enabled);
    }
}
