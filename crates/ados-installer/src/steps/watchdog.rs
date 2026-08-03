//! Hardware watchdog arming at install time.
//!
//! A self-managed radio bind churn can hard-lock the kernel on some SoCs (the
//! RK3588 class is a known offender): every interface — USB radios AND the
//! on-SoC ethernet — goes dark at once with no clean shutdown, and the box stays
//! stranded until someone physically power-cycles it. No agent software can
//! recover a frozen kernel; the only thing that can is the SoC hardware watchdog.
//!
//! This step arms `/dev/watchdog` through systemd's `RuntimeWatchdogSec`: PID 1
//! pets the device on a timer, and if the kernel freezes (PID 1 can no longer
//! pet it) the SoC auto-resets and the box reboots itself — coming back on its
//! pinned MAC, no cable or manual power-cycle. `RebootWatchdogSec` covers the
//! second-order case where a reboot/shutdown itself hangs.
//!
//! **DEFAULT OFF** (`network.watchdog.enabled: true` to arm), and the step runs
//! LAST, after `health`. Both because arming a hard-reset-on-stall before the
//! step that starts every service turned a slow startup into an unrecoverable
//! box: the board reset itself mid-install, mid-write, and again on every boot,
//! each reset degrading the card until it would not boot at all. See
//! [`watchdog_enabled`] for the reasoning, and why the capability is kept rather
//! than deleted.
//!
//! Also gated on the watchdog device existing (a VM or a board without one is a
//! clean no-op), EXCEPT the disarm path, which runs regardless so an upgrade
//! actively reverts a rig that already carries the drop-in. Optional: a
//! write/re-exec problem degrades, never aborts the install.

use std::path::Path;

use serde::Deserialize;

use crate::ctx::Ctx;
use crate::env::CONFIG_YAML;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// The SoC watchdog character device. Its presence is the hardware gate.
const WATCHDOG_DEV: &str = "/dev/watchdog";
/// The systemd manager drop-in this step writes.
const CONF_PATH: &str = "/etc/systemd/system.conf.d/ados-watchdog.conf";
/// The runtime timeout we WANT on hardware that can carry it: ample time for a
/// busy-but-alive box before a kernel lockup is declared. systemd pets
/// `/dev/watchdog` at HALF this interval.
const RUNTIME_SEC_DESIRED: u32 = 30;
/// The fallback runtime timeout used ONLY when the hardware ceiling is
/// unreadable (some drivers — e.g. the Allwinner `sunxi-wdt` on the A733 — expose
/// no timeout in sysfs). Chosen safe on the shortest SoC watchdogs we ship on
/// (sunxi-wdt 16s, bcm2835 15s): 14s -> systemd pets at ~7s, comfortably under
/// the reset, and 14s is at or below those hardware maxes so it is never clamped.
const RUNTIME_SEC_FALLBACK: u32 = 14;
/// How long a reboot/shutdown itself may hang before the SoC force-resets. Only
/// armed during teardown, so the hardware clamp is harmless here.
const REBOOT_SEC: &str = "2min";

/// The SoC watchdog's hardware timeout ceiling in seconds, when the driver
/// exposes it via sysfs (`max_timeout`, else the current `timeout`). Many drivers
/// do NOT (the `sunxi-wdt` leaves these attributes empty), so callers must treat
/// `None` as "unknown," not "no watchdog."
fn hw_watchdog_ceiling() -> Option<u32> {
    for attr in ["max_timeout", "timeout"] {
        if let Ok(v) = std::fs::read_to_string(format!("/sys/class/watchdog/watchdog0/{attr}")) {
            if let Ok(n) = v.trim().parse::<u32>() {
                if n > 0 {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Resolve a safe `RuntimeWatchdogSec`. The one invariant that keeps every board
/// healthy: the value must never EXCEED the hardware's real reset timeout — if it
/// does, the hardware silently clamps it while systemd keeps petting at half the
/// *requested* value, and on a board with a short watchdog (e.g. the 16s
/// sunxi-wdt) that pet interval lands after the reset, hard-looping the box every
/// boot. So: when the ceiling is READABLE, keep the desired 30s but cap it a
/// margin below the ceiling (this preserves 30s on capable boards like the RK3588
/// and only trims it on short-watchdog boards); when the ceiling is UNKNOWN, use
/// the conservative fallback rather than the aggressive 30s. Pure — unit-tested
/// without the filesystem.
fn safe_runtime_secs(hw_ceiling: Option<u32>) -> u32 {
    match hw_ceiling {
        Some(c) => RUNTIME_SEC_DESIRED.min(c.saturating_sub(2)).max(5),
        None => RUNTIME_SEC_FALLBACK,
    }
}

/// The `config.yaml` slice this step reads. Everything optional so a config with
/// no `network.watchdog` block resolves to the default-on posture.
#[derive(Debug, Deserialize, Default)]
struct RootView {
    #[serde(default)]
    network: Option<NetworkView>,
}
#[derive(Debug, Deserialize, Default)]
struct NetworkView {
    #[serde(default)]
    watchdog: Option<WatchdogView>,
}
#[derive(Debug, Deserialize, Default)]
struct WatchdogView {
    #[serde(default)]
    enabled: Option<bool>,
}

/// True only when the config explicitly enables the watchdog.
///
/// **Default OFF.** It was default-on, and that turned a slow start into an
/// unrecoverable box. This step arms a timer that HARD-RESETS the SoC — no
/// shutdown, no log, no trace — if PID 1 is late by more than a few seconds, and
/// it ran BEFORE the step that starts every service. A ground station whose
/// startup exceeded the timeout reset itself mid-install, mid-write, then did
/// the same on the next boot, and each reset landed during a write so the card
/// degraded and the next boot was slower still. A loop that ends in a card that
/// will not boot at all.
///
/// That is the same failure this tree already removed once, by another
/// mechanism: `kernel.hung_task_panic` was turned off because "a hung task is
/// nearly always slow hardware, and the box is still correct — it needs to be
/// allowed to finish, not shot". The hardware watchdog was doing exactly that by
/// a different route, and was left armed.
///
/// The capability is worth keeping and is why this is a default rather than a
/// deletion: a genuinely frozen kernel in the field cannot be recovered by any
/// software, and the SoC watchdog brings the box back with nobody driving out to
/// it. That is a field property. On a bench, where a human can power-cycle, it
/// has cost more outages than it has prevented. So it is opt-in:
/// `network.watchdog.enabled: true`.
///
/// Pure so it is unit-tested without the filesystem.
fn watchdog_enabled(text: &str) -> bool {
    serde_norway::from_str::<RootView>(text)
        .ok()
        .and_then(|r| r.network)
        .and_then(|n| n.watchdog)
        .and_then(|w| w.enabled)
        .unwrap_or(false)
}

/// Render the systemd `system.conf` drop-in that arms the hardware watchdog.
/// `runtime_secs` is the resolved-safe runtime timeout (see `safe_runtime_secs`).
/// Pure.
fn render_conf(runtime_secs: u32) -> String {
    format!(
        "# ADOS Drone Agent — hardware watchdog.\n\
         # Arms the SoC watchdog so a kernel hard-lockup auto-reboots the box\n\
         # instead of stranding it. Generated by the agent; do not edit by hand.\n\
         [Manager]\n\
         RuntimeWatchdogSec={runtime_secs}s\n\
         RebootWatchdogSec={REBOOT_SEC}\n"
    )
}

/// Hardware-watchdog arming step.
pub struct Watchdog;

impl Step for Watchdog {
    fn id(&self) -> &str {
        "watchdog"
    }
    fn requires(&self) -> &[&str] {
        // After deps so the apt systemd upgrade (its own daemon-reexec) is done
        // and won't clobber our drop-in.
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: re-affirm the drop-in on every upgrade.
        None
    }
    fn kind(&self) -> StepKind {
        // Optional: a write/re-exec problem must degrade, never abort the install.
        StepKind::Optional
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // A previously-armed box must be DISARMED even if the device check would
        // skip, so an upgrade actively reverts a rig that already carries the
        // drop-in. A default that only affected fresh installs would leave every
        // existing rig looping, which is the state this change exists to end.
        let path = Path::new(CONF_PATH);
        let enabled_now = std::fs::read_to_string(CONFIG_YAML)
            .map(|t| watchdog_enabled(&t))
            .unwrap_or(false);
        if !enabled_now && path.exists() {
            let _ = std::fs::remove_file(path);
            let _ = exec::run("systemctl", &["daemon-reexec"]);
            tracing::info!(
                path = CONF_PATH,
                "removed a previously-armed hardware watchdog drop-in"
            );
            return StepOutcome::Ok;
        }

        // No watchdog device → nothing to arm (a VM or a board without one).
        if !Path::new(WATCHDOG_DEV).exists() {
            tracing::info!("no hardware watchdog device present; skipping");
            return StepOutcome::Ok;
        }
        // An unreadable config resolves to OFF, matching the default. Failing
        // open here would arm a hard-reset timer on exactly the box whose config
        // could not be read, which is the least likely one to survive it.
        let enabled = std::fs::read_to_string(CONFIG_YAML)
            .map(|t| watchdog_enabled(&t))
            .unwrap_or(false);
        let path = Path::new(CONF_PATH);
        if !enabled {
            // Remove a previously-written drop-in + re-exec so the disable takes.
            if path.exists() {
                let _ = std::fs::remove_file(path);
                let _ = exec::run("systemctl", &["daemon-reexec"]);
            }
            tracing::info!("hardware watchdog disabled by config");
            return StepOutcome::Ok;
        }
        let runtime_secs = safe_runtime_secs(hw_watchdog_ceiling());
        let body = render_conf(runtime_secs);
        // Idempotent: skip the write + re-exec when the drop-in is already current.
        if std::fs::read_to_string(path)
            .map(|c| c == body)
            .unwrap_or(false)
        {
            tracing::info!("hardware watchdog drop-in already current");
            return StepOutcome::Ok;
        }
        if let Err(e) = write_file(path, &body) {
            tracing::warn!(error = %e, "failed to write hardware watchdog drop-in");
            return StepOutcome::Failed(format!("could not write {CONF_PATH}: {e}"));
        }
        // Re-exec PID 1 so it re-reads system.conf and opens /dev/watchdog at the
        // configured timeout. A plain daemon-reload does NOT re-read system.conf.
        let r = exec::run("systemctl", &["daemon-reexec"]);
        if !r.spawned || r.code != Some(0) {
            tracing::warn!(code = ?r.code, "daemon-reexec after watchdog write returned non-zero");
        }
        tracing::info!(
            runtime_secs,
            reboot = REBOOT_SEC,
            hw_ceiling = ?hw_watchdog_ceiling(),
            "armed the hardware watchdog"
        );
        StepOutcome::Ok
    }
}

/// Write `body` to `path` atomically (tmp + rename), creating the parent dir.
fn write_file(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_leaves_the_watchdog_disarmed() {
        // DEFAULT OFF. This arms a timer that hard-resets the SoC with no
        // shutdown, no log and no trace when PID 1 is a few seconds late. On a
        // ground station whose service startup exceeded it, the board reset
        // itself mid-install and again on every boot after, each reset landing
        // during a write, until the card would not boot at all.
        //
        // The capability is kept because a genuinely frozen kernel in the field
        // cannot be recovered by software — but it is opt-in now, because a
        // reset-on-slowness default is the same failure this tree already
        // removed once as kernel.hung_task_panic.
        assert!(!watchdog_enabled(""));
        assert!(!watchdog_enabled("agent:\n  name: x\n"));
        // An unrelated network block does not arm it either.
        assert!(!watchdog_enabled(
            "network:\n  regulatory:\n    mode: region\n"
        ));
        // Nor does an unparseable config: failing open would arm a hard-reset
        // timer on exactly the box whose config could not be read.
        assert!(!watchdog_enabled("{{{ not yaml"));
    }

    #[test]
    fn the_watchdog_step_runs_after_health() {
        // Arming a hard-reset-on-stall BEFORE the step that starts every service
        // is what turned a slow start into an unrecoverable box. Even
        // default-off, someone will opt in, so the ordering has to be right.
        let steps = crate::steps::full_install_chain();
        let pos = |id: &str| steps.iter().position(|s| s.id() == id).unwrap();
        assert!(
            pos("start") < pos("watchdog"),
            "must arm after services start"
        );
        assert!(pos("health") < pos("watchdog"), "and after health passes");
    }

    #[test]
    fn explicit_disable_is_honored() {
        assert!(!watchdog_enabled(
            "network:\n  watchdog:\n    enabled: false\n"
        ));
    }

    #[test]
    fn explicit_enable_is_honored() {
        assert!(watchdog_enabled(
            "network:\n  watchdog:\n    enabled: true\n"
        ));
    }

    #[test]
    fn conf_body_has_manager_section_and_both_timeouts() {
        let b = render_conf(safe_runtime_secs(None));
        assert!(b.starts_with("# ADOS Drone Agent"));
        assert!(b.contains("[Manager]"));
        // The default (no readable hw ceiling) must be safe on a 16s SoC wdt.
        assert!(b.contains("RuntimeWatchdogSec=14s"));
        assert!(b.contains("RebootWatchdogSec=2min"));
        assert!(b.ends_with('\n'));
    }

    #[test]
    fn runtime_secs_never_exceeds_the_hardware_ceiling() {
        // No readable ceiling (e.g. the sunxi-wdt) -> the conservative fallback,
        // safe on a 16s wdt (systemd pets at ~7s, well under the reset).
        assert_eq!(safe_runtime_secs(None), 14);
        // A 16s hardware max (Allwinner sunxi-wdt, if it DID expose it) -> 14s
        // (2s margin), so systemd is never clamped and its pet stays accurate.
        // The old 30s request was clamped to 16s while systemd kept petting at
        // ~15s -> the reboot loop.
        assert_eq!(safe_runtime_secs(Some(16)), 14);
        // A short ceiling caps below the desired value, with a 5s floor.
        assert_eq!(safe_runtime_secs(Some(10)), 8);
        assert_eq!(safe_runtime_secs(Some(6)), 5);
        // BOARD-AWARE: a capable board (RK3588-class, 30s+ hardware max) KEEPS
        // the full desired 30s — the fix must NOT lower the watchdog on boards
        // that were already fine.
        assert_eq!(safe_runtime_secs(Some(30)), 28);
        assert_eq!(safe_runtime_secs(Some(60)), 30);
        assert_eq!(safe_runtime_secs(Some(120)), 30);
        // The invariant: the resolved value is always strictly under the
        // hardware ceiling, so systemd is never clamped.
        for ceiling in [6u32, 10, 16, 30, 60, 120] {
            let rt = safe_runtime_secs(Some(ceiling));
            assert!(
                rt < ceiling,
                "runtime {rt}s must be under hw ceiling {ceiling}s"
            );
        }
    }
}
