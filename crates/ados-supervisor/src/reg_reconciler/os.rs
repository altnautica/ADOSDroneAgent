//! OS edges for the regulatory reconciler: the canonical-path config reads, the
//! channel-safety-gated reconcile body, and the `iw` shells.
//!
//! All write/read edges are Linux-only; on a non-Linux dev host
//! `reconcile_global_domain` is an inert no-op so the bind-orchestrator call
//! site stays portable. The pure policy + parsers live in `policy` / `parse`.

use ados_protocol::logd::emitter::EventEmitter;

#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

#[cfg(target_os = "linux")]
use super::config::{read_config_from, read_wanted_from, RegReconcilerConfig, WantedReg};
#[cfg(target_os = "linux")]
use super::config::{DEFAULT_CHANNEL, DEFAULT_REG_DOMAIN};
#[cfg(target_os = "linux")]
use super::policy::{reconcile_decision, ReconcileDecision};
#[cfg(target_os = "linux")]
use super::REG_REASSERT_KIND;

/// Run one channel-safety-gated reconcile of the GLOBAL regulatory domain back
/// to the configured wanted value, emitting `radio.reg_reasserted` through
/// `events` when a re-assert actually fires. This is the unthrottled body shared
/// by the periodic reconciler tick AND the post-bind immediate re-assert: the
/// bind orchestrator calls it the instant the bind tunnel comes up, so the
/// foreign baked country the bind re-entry just re-asserted is corrected within
/// a couple of seconds (before the onboard WiFi can blip), without waiting for
/// the next throttled supervisor tick.
///
/// SAFETY (identical to the periodic path): re-asserts ONLY a forceable operator
/// country (never the world default / a malformed code) and ONLY when that
/// domain permits the configured rendezvous channel (`channel_permitted` reads
/// the live enabled set), so it can never cap the WFB radio onto a forbidden
/// frequency, and never moves toward the injection PHY's baked country.
/// Idempotent: a cheap no-op (one `iw reg get` + a compare) when already in sync.
#[cfg(target_os = "linux")]
pub async fn reconcile_global_domain(events: &EventEmitter) {
    if !iw_available().await {
        return;
    }
    let wanted = read_wanted();
    let live = active_global_reg_domain().await;

    // Cheap common path: already correct, no `iw phy channels` read needed.
    if let ReconcileDecision::InSync = reconcile_decision(live.as_deref(), &wanted.domain, true) {
        return;
    }
    // Out of sync (or unreadable live): determine whether the wanted domain
    // permits the rendezvous channel before forcing it, so we can never cap
    // the radio onto a forbidden frequency.
    let channel_ok = channel_permitted(wanted.channel).await;
    match reconcile_decision(live.as_deref(), &wanted.domain, channel_ok) {
        ReconcileDecision::InSync | ReconcileDecision::NoWanted => {}
        ReconcileDecision::SkipChannelUnsafe => {
            tracing::warn!(
                wanted = %wanted.domain,
                channel = wanted.channel,
                live = ?live,
                note = "wanted domain would not permit the rendezvous channel; not re-asserting",
                "reg_reconciler_skipped_channel_unsafe"
            );
        }
        ReconcileDecision::Reassert { from, to } => {
            let verified = set_reg_domain(&to).await;
            if verified {
                tracing::info!(from = ?from, to = %to, "reg_reconciler_reasserted");
            } else {
                tracing::warn!(
                    from = ?from,
                    to = %to,
                    note = "re-assert issued but readback did not confirm (possible phy override)",
                    "reg_reconciler_reassert_unconfirmed"
                );
            }
            events.emit(
                REG_REASSERT_KIND,
                ados_protocol::logd::Level::Info,
                reg_reassert_detail(from.as_deref(), &to, wanted.channel, true),
            );
        }
    }
}

/// Non-Linux build: the reconcile has no OS edges to drive, so it is an inert
/// no-op. Keeps the call site in the bind orchestrator portable across the dev
/// host and CI.
#[cfg(not(target_os = "linux"))]
pub async fn reconcile_global_domain(_events: &EventEmitter) {}

/// Build the `radio.reg_reasserted` detail map. All fields are bland and
/// reader-facing. Mirrors the radio-side detail shape so the two halves write
/// the same event schema. Built only on the Linux re-assert path.
#[cfg(any(target_os = "linux", test))]
pub(super) fn reg_reassert_detail(
    from_country: Option<&str>,
    to_country: &str,
    wfb_channel: u8,
    channel_permitted: bool,
) -> ados_protocol::logd::Fields {
    use ados_protocol::logd::{Fields, Value as MpVal};
    let mut d = Fields::new();
    // The supervisor reconcile is interface-agnostic (it acts on the global
    // domain), so the source field names the agent half rather than an iface.
    d.insert("source".to_string(), MpVal::from("supervisor"));
    if let Some(from) = from_country {
        d.insert("from_country".to_string(), MpVal::from(from));
    }
    d.insert("to_country".to_string(), MpVal::from(to_country));
    d.insert("wfb_channel".to_string(), MpVal::from(wfb_channel as u64));
    d.insert(
        "channel_permitted".to_string(),
        MpVal::from(channel_permitted),
    );
    d
}

// ---------------------------------------------------------------------------
// Config reads (canonical path)
// ---------------------------------------------------------------------------

/// Read `network.reg_reconciler` from the canonical config path. Re-read each
/// tick so a config edit takes effect without restarting the supervisor.
#[cfg(target_os = "linux")]
pub(super) fn read_config() -> RegReconcilerConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => RegReconcilerConfig::default(),
    }
}

/// Read the wanted regulatory domain + rendezvous channel from the canonical
/// config path (the same `video.wfb` block the radio uses).
#[cfg(target_os = "linux")]
fn read_wanted() -> WantedReg {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_wanted_from(&t),
        Err(_) => WantedReg {
            domain: DEFAULT_REG_DOMAIN.to_string(),
            channel: DEFAULT_CHANNEL,
        },
    }
}

// ---------------------------------------------------------------------------
// Linux OS edges
// ---------------------------------------------------------------------------

/// True when the `iw` binary is on PATH.
#[cfg(target_os = "linux")]
async fn iw_available() -> bool {
    run_status("sh", &["-c", "command -v iw"]).await
}

/// Read the live global regulatory domain via `iw reg get`. Read-only.
#[cfg(target_os = "linux")]
async fn active_global_reg_domain() -> Option<String> {
    let out = run_output("iw", &["reg", "get"]).await?;
    super::parse::parse_global_reg_domain(&out)
}

/// Whether the configured rendezvous channel is permitted on any present phy.
/// Reads each interface's wiphy channel set; the channel is permitted when it is
/// in the enabled set of at least one phy (the injection adapter's), or when no
/// phy's set could be read (matching the bring-up gate's "empty = do not
/// restrict"). Never restricts on an unknown — it can only ever ALLOW a
/// re-assert it is sure is safe, and otherwise falls through to allow rather than
/// wedge (the wanted domain is, by construction, a sane operator country).
#[cfg(target_os = "linux")]
async fn channel_permitted(channel: u8) -> bool {
    let Some(dev) = run_output("iw", &["dev"]).await else {
        return true; // could not enumerate — do not restrict
    };
    let ifaces = super::parse::parse_interfaces(&dev);
    if ifaces.is_empty() {
        return true;
    }
    let mut any_set_read = false;
    for iface in ifaces {
        let Some(info) = run_output("iw", &[&iface, "info"]).await else {
            continue;
        };
        let Some(phy) = super::parse::parse_wiphy(&info) else {
            continue;
        };
        let Some(chans) = run_output("iw", &["phy", &phy, "channels"]).await else {
            continue;
        };
        let enabled = super::parse::parse_enabled_channels(&chans);
        if enabled.is_empty() {
            continue;
        }
        any_set_read = true;
        if enabled.contains(&channel) {
            return true;
        }
    }
    // If we read at least one non-empty channel set and the target was in none,
    // the wanted domain would NOT permit the channel on the present radios — do
    // not force it. If no set could be read, do not restrict.
    !any_set_read
}

/// Apply the regulatory domain via `iw reg set <domain>` and verify the readback
/// with bounded retry. Returns true only when `iw reg get` reports the wanted
/// domain. Never touches an interface (a global per-phy call), so it cannot
/// disturb the operator's management link.
#[cfg(target_os = "linux")]
async fn set_reg_domain(domain: &str) -> bool {
    const MAX_ATTEMPTS: u32 = 3;
    const RETRY_INTERVAL_MS: u64 = 2000;
    const VERIFY_CEILING_MS: u64 = 2000;
    const VERIFY_STEP_MS: u64 = 100;
    let want = domain.to_ascii_uppercase();
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(RETRY_INTERVAL_MS)).await;
        }
        if !run_status("iw", &["reg", "set", &want]).await {
            continue;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(VERIFY_CEILING_MS);
        loop {
            if active_global_reg_domain().await.as_deref() == Some(want.as_str()) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(VERIFY_STEP_MS)).await;
        }
    }
    false
}

/// Run a command, returning true on a zero exit. stdout/stderr are discarded.
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

    #[tokio::test]
    async fn reconcile_global_domain_runs_without_a_logd_socket() {
        // The shared reconcile (used by both the periodic tick and the post-bind
        // immediate re-assert) must never panic when the logd socket is absent.
        // On a non-Linux dev host it is an inert no-op; on Linux CI it shells
        // `iw` read-only and falls through safely when the wanted domain is
        // already in sync or the tools are unavailable. Either way: no panic.
        let events = EventEmitter::with_socket("ados-test", "/nonexistent/ados/logd.sock");
        reconcile_global_domain(&events).await;
    }

    #[test]
    fn reassert_detail_is_bland_and_complete() {
        let d = reg_reassert_detail(Some("BO"), "US", 149, true);
        assert_eq!(d.get("source").and_then(|v| v.as_str()), Some("supervisor"));
        assert_eq!(d.get("from_country").and_then(|v| v.as_str()), Some("BO"));
        assert_eq!(d.get("to_country").and_then(|v| v.as_str()), Some("US"));
        assert_eq!(d.get("wfb_channel").and_then(|v| v.as_u64()), Some(149));
        assert_eq!(
            d.get("channel_permitted").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn reassert_detail_omits_from_when_unreadable() {
        let d = reg_reassert_detail(None, "US", 149, true);
        assert!(!d.contains_key("from_country"));
    }
}
