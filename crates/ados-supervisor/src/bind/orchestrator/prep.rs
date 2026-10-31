//! Injection-adapter monitor-mode prep run before the bind unit starts.

#[cfg(target_os = "linux")]
use std::time::Instant;

use super::InjectionPrepOutcome;
use crate::bind::fsm::BindPrecheck;

/// Max times to run the monitor-mode prep before giving up on a verified readback.
#[cfg(target_os = "linux")]
const BIND_PREP_MAX_ATTEMPTS: u32 = 3;

/// Clear overall wall-clock ceiling on the whole injection-iface prep (across
/// every candidate + every retry). Each interface command is already bounded to
/// 5s and the retry loop to [`BIND_PREP_MAX_ATTEMPTS`], but only a single ceiling
/// makes the total provable: with every tool timing out on every attempt the
/// loop could otherwise run tens of seconds — and it runs in the Idle setup
/// window, BEFORE the `OpeningTunnel` transition, so the per-phase no-progress
/// watchdog does not cover it. A healthy prep verifies monitor mode in well
/// under a second; this only bites when the interface tools hang, and lets the
/// prep bail to an honest `monitor_unverified` precheck instead of stalling the
/// bind.
#[cfg(target_os = "linux")]
const BIND_PREP_MAX_DURATION: std::time::Duration = std::time::Duration::from_secs(45);

/// Put the WFB injection adapter(s) into MONITOR mode before the bind unit starts,
/// and VERIFY the iface actually reached it (readback + retry) rather than firing
/// the commands and hoping.
///
/// History: the vendored wfb-ng `wfb-server` aborted its init when its
/// `nmcli device show <iface>` pre-check could not find the device — the case for
/// a managed-mode iface up with no carrier, which NetworkManager does not
/// enumerate. That pre-check is now skipped at the wfb-server config layer, so the
/// load-bearing job here is simply that the iface reaches a radiating monitor PHY:
/// `iw dev <if> set monitor none` initialises the PHY un-muted, unlike
/// `iw <if> set type monitor`, which leaves the RTL8812EU pinned at the muted
/// txpower floor (-100 dBm, carrier down). A command "succeeding" does not prove
/// the mode took, so each candidate is read back via `iw <if> info` and the prep
/// retried with backoff until `type monitor` is observed. Best-effort + idempotent;
/// the `is_injection && !is_virtual` filter never touches the management link.
/// Returns one outcome per injection candidate for the caller to record + emit.
#[cfg(target_os = "linux")]
pub(super) async fn prepare_injection_iface_for_bind() -> Vec<InjectionPrepOutcome> {
    let candidates = crate::mgmt_link_guardian::detection::collect_candidates().await;
    let mut outcomes = Vec::new();
    // A single, clear wall-clock ceiling across all candidates + retries. If the
    // interface tools hang, the prep bails to an honest unverified outcome
    // rather than eating tens of seconds in the unwatched Idle setup window.
    let deadline = Instant::now() + BIND_PREP_MAX_DURATION;
    for c in candidates
        .iter()
        .filter(|c| c.is_injection && !c.is_virtual)
    {
        if Instant::now() >= deadline {
            break;
        }
        let iface = c.name.as_str();
        let mut injection_mode = "unknown".to_string();
        let mut monitor_verified = false;
        for attempt in 0..BIND_PREP_MAX_ATTEMPTS {
            if Instant::now() >= deadline {
                break;
            }
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            }
            // Release from NetworkManager, then bring the iface into monitor mode.
            let _ = run_iface_cmd("nmcli", &["dev", "set", iface, "managed", "no"]).await;
            let _ = run_iface_cmd("ip", &["link", "set", iface, "down"]).await;
            // `set monitor none` is primary (it initialises the PHY un-muted); the
            // `set type monitor` form is the fallback for any adapter that rejects it.
            if !run_iface_cmd("iw", &["dev", iface, "set", "monitor", "none"]).await {
                let _ = run_iface_cmd("iw", &[iface, "set", "type", "monitor"]).await;
            }
            let _ = run_iface_cmd("ip", &["link", "set", iface, "up"]).await;
            injection_mode = read_injection_iface_mode(iface)
                .await
                .unwrap_or_else(|| "unknown".to_string());
            if injection_mode == "monitor" {
                monitor_verified = true;
                break;
            }
        }
        let nm_enumerable = iface_nm_enumerable(iface).await;
        tracing::info!(
            iface,
            injection_mode = %injection_mode,
            monitor_verified,
            nm_enumerable,
            "bind_prepared_injection_iface"
        );
        outcomes.push(InjectionPrepOutcome {
            iface: iface.to_string(),
            injection_mode,
            monitor_verified,
            nm_enumerable,
        });
    }
    outcomes
}

/// Off-Linux dev hosts have no injection adapter to prepare.
#[cfg(not(target_os = "linux"))]
pub(super) async fn prepare_injection_iface_for_bind() -> Vec<InjectionPrepOutcome> {
    Vec::new()
}

/// Reduce the per-iface prep outcomes to one session-level summary. `ok` requires
/// every candidate to have reached verified monitor mode (a managed-mode adapter
/// radiates nothing); the representative `injection_mode`/`iface` prefer a failing
/// candidate so the surfaced reason points at the actual problem.
pub(super) fn summarize_precheck(outcomes: &[InjectionPrepOutcome]) -> BindPrecheck {
    if outcomes.is_empty() {
        return BindPrecheck {
            ok: false,
            reason: Some("iface_not_found"),
            injection_mode: "unknown".to_string(),
            nm_enumerable: false,
            iface: None,
        };
    }
    let ok = outcomes.iter().all(|o| o.monitor_verified);
    let nm_enumerable = outcomes.iter().all(|o| o.nm_enumerable);
    let representative = outcomes
        .iter()
        .find(|o| !o.monitor_verified)
        .unwrap_or(&outcomes[0]);
    BindPrecheck {
        ok,
        reason: if ok { None } else { Some("monitor_unverified") },
        injection_mode: representative.injection_mode.clone(),
        nm_enumerable,
        iface: Some(representative.iface.clone()),
    }
}

/// Run a short interface-management command, bounded so a hung tool never stalls
/// the bind. Returns whether it exited successfully.
#[cfg(target_os = "linux")]
async fn run_iface_cmd(bin: &str, args: &[&str]) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(bin).args(args).output(),
        )
        .await,
        Ok(Ok(o)) if o.status.success()
    )
}

/// Read the operating mode (`monitor` | `managed` | …) of an injection iface from
/// `iw <if> info`, or `None` when it cannot be read. The parse mirrors the radio's
/// `set_monitor_mode_verified` readback (the `type ` line) without taking a crate
/// dependency on `ados-radio` (which would be heavy + cyclic from the supervisor).
#[cfg(target_os = "linux")]
async fn read_injection_iface_mode(iface: &str) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("iw")
            .args([iface, "info"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_iface_mode(&String::from_utf8_lossy(&out.stdout))
}

/// Pure parse of the `type <mode>` line out of `iw <iface> info`. Unit-tested
/// independently of `iw`. (Only the linux reader calls it outside tests.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn parse_iface_mode(info: &str) -> Option<String> {
    for line in info.lines() {
        if let Some(rest) = line.trim().strip_prefix("type ") {
            let mode = rest.trim();
            if !mode.is_empty() {
                return Some(mode.to_string());
            }
        }
    }
    None
}

/// Whether NetworkManager enumerates the iface: `nmcli device show <iface>` exits
/// 0 when NM knows the device and non-zero (RC 10) when it does not. Informational
/// now the wfb-server precheck is skipped, but it is exactly the condition that
/// precheck tested, so it predicts that abort if the skip is ever reverted.
#[cfg(target_os = "linux")]
async fn iface_nm_enumerable(iface: &str) -> bool {
    run_iface_cmd("nmcli", &["-t", "device", "show", iface]).await
}
