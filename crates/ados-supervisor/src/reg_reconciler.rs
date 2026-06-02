//! Periodic regulatory-domain reconciler for the supervisor monitor pass.
//!
//! A self-managed-regulatory USB injection PHY (the RTL family) asserts its
//! EEPROM-baked country (e.g. `BO`) as the GLOBAL cfg80211 regulatory domain
//! when it loads and enters / re-enters monitor mode. A normal onboard FullMAC
//! adapter (the Pi-family Broadcom, the Rock-family AIC8800) obeys that global
//! domain. When the baked country is one whose rules the onboard driver cannot
//! satisfy on its associated channel, the onboard WiFi keeps its association and
//! IP but loses its data path entirely (the gateway never resolves, 100% loss),
//! so the management link dies with no failover.
//!
//! The radio service re-asserts the configured wanted domain right after its
//! monitor-mode bring-up (the prevention layer). This supervisor reconciler is
//! the symmetric, always-running half: it runs on BOTH profiles from the monitor
//! tick (the same place as the reactive WiFi self-heal) and catches any LATER
//! drift — a bind re-entry, a monitor re-init, or a profile/role change that
//! re-churns the injection PHY long after the radio's one-shot reconcile. When
//! the live global domain drifts off the configured wanted value, it re-asserts
//! the wanted domain so the onboard WiFi is never left under a foreign domain.
//!
//! Safety invariants (it can never cap the WFB radio):
//! - It only ever forces a domain that PERMITS the configured rendezvous
//!   channel. It reads the injection interface's enabled channel set (`iw phy
//!   channels`, which already excludes DFS / disabled / no-IR) and re-asserts
//!   only when the rendezvous channel is in that set (or the set is unknown,
//!   matching the bring-up gate's "empty = do not restrict").
//! - It never forces the all-restrictive world default (`00`) or a malformed
//!   domain.
//! - It is idempotent: a no-op when the live domain already equals the wanted
//!   value (the cheap steady-state path, one `iw reg get` + a compare).
//! - It NEVER touches an interface — `iw reg set` is a global per-phy call — so
//!   it cannot disturb the operator's management link directly. The onboard
//!   WiFi recovers because it re-reads the now-sane global domain; the reactive
//!   self-heal remains the backstop for a link that needs an explicit rebuild.
//!
//! Default-ON, configurable under `network.reg_reconciler`. The pure decision
//! logic and config parsing are unit-tested on every host; the OS edges (iw)
//! are Linux-only and the tick is an inert no-op on a non-Linux dev host.

use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

/// The event kind recorded when the reconciler re-asserts the global domain.
/// Bland and reader-facing: it names what the code did. Mirrors the radio-side
/// event kind so an RCA queries one classifier across both halves.
pub const REG_REASSERT_KIND: &str = "radio.reg_reasserted";

/// Default reconcile cadence. The monitor pass already runs on its own
/// interval; this gate throttles the reconcile so a fast monitor loop does not
/// shell `iw reg get` more often than needed. 30 s is well inside the window in
/// which a drifted domain would otherwise sit broken.
const DEFAULT_TICK_INTERVAL_S: u64 = 30;

/// The default wanted regulatory domain, byte-identical to the radio config's
/// `default_reg_domain`. Permits the home channel (149 / 5745 MHz, U-NII-3,
/// non-DFS) at usable power. Operators override per region in config.
const DEFAULT_REG_DOMAIN: &str = "US";

/// The default rendezvous channel, byte-identical to the radio config's
/// `default_channel`. Used as the channel-safety target when the config omits a
/// channel / rendezvous pin.
const DEFAULT_CHANNEL: u8 = 149;

/// Configuration for the regulatory reconciler, read from
/// `network.reg_reconciler`. Default-ON so a fresh board keeps its onboard WiFi
/// out of the box; an operator can disable it cleanly if a bespoke regulatory
/// setup ever conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegReconcilerConfig {
    /// Whether the reconciler runs at all. Default true.
    pub enabled: bool,
    /// Minimum spacing between reconcile attempts.
    pub tick_interval: Duration,
}

impl Default for RegReconcilerConfig {
    fn default() -> Self {
        RegReconcilerConfig {
            enabled: true,
            tick_interval: Duration::from_secs(DEFAULT_TICK_INTERVAL_S),
        }
    }
}

/// Parse `network.reg_reconciler` out of a config body. An absent section reads
/// as the all-defaults (enabled) config so the reconciler is on out of the box.
/// A malformed config also falls back to enabled rather than silently disabling
/// the onboard-WiFi protection.
pub fn read_config_from(text: &str) -> RegReconcilerConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        reg_reconciler: Option<Recon>,
    }
    #[derive(serde::Deserialize)]
    struct Recon {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        tick_interval_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.reg_reconciler {
            Some(r) => RegReconcilerConfig {
                enabled: r.enabled,
                // Floor at 1 s so a zero in config cannot spin the reconcile.
                tick_interval: Duration::from_secs(
                    r.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1),
                ),
            },
            None => RegReconcilerConfig::default(),
        },
        Err(_) => RegReconcilerConfig::default(),
    }
}

/// The wanted regulatory domain + rendezvous channel, read from the same
/// `video.wfb` block the radio uses. The reconciler never invents a domain; it
/// reuses the operator's configured value (or the safe default when absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantedReg {
    pub domain: String,
    pub channel: u8,
}

/// Parse the wanted regulatory domain + rendezvous channel out of a config body.
/// Reads `video.wfb.reg_domain` (default `US`) and the rendezvous channel
/// (`video.wfb.rendezvous_channel` when pinned, else `video.wfb.channel`,
/// default 149). Byte-identical resolution to the radio's `WfbConfig`.
pub fn read_wanted_from(text: &str) -> WantedReg {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        video: Video,
    }
    #[derive(serde::Deserialize, Default)]
    struct Video {
        #[serde(default)]
        wfb: Wfb,
    }
    #[derive(serde::Deserialize, Default)]
    struct Wfb {
        #[serde(default)]
        reg_domain: Option<String>,
        #[serde(default)]
        channel: Option<u8>,
        #[serde(default)]
        rendezvous_channel: Option<u8>,
    }
    let wfb = serde_norway::from_str::<Raw>(text)
        .map(|r| r.video.wfb)
        .unwrap_or_default();
    let domain = wfb
        .reg_domain
        .map(|d| d.trim().to_ascii_uppercase())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| DEFAULT_REG_DOMAIN.to_string());
    let channel = wfb
        .rendezvous_channel
        .or(wfb.channel)
        .unwrap_or(DEFAULT_CHANNEL);
    WantedReg { domain, channel }
}

/// True when a wanted domain is a concrete, forceable country: exactly two
/// uppercase-ASCII-or-digit characters and NOT the all-restrictive world code
/// `00`. The world default permits almost nothing at usable power, so forcing it
/// would cap the radio — the reconciler refuses it. Pure.
pub fn is_forceable_domain(domain: &str) -> bool {
    domain.len() == 2
        && domain != "00"
        && domain
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// What the reconciler decided for one observation. Pure so the policy is
/// testable without any OS call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileDecision {
    /// The live global domain already equals the wanted domain: nothing to do.
    InSync,
    /// The wanted domain is missing / malformed / the world default: there is
    /// nothing safe to force, leave the live domain as-is.
    NoWanted,
    /// The wanted domain would not permit the configured channel, so forcing it
    /// would cap the radio. Skip the re-assert.
    SkipChannelUnsafe,
    /// The live domain differs from the wanted domain and the wanted domain
    /// permits the channel: re-assert. Carries the from/to countries.
    Reassert { from: Option<String>, to: String },
}

/// Pure reconcile policy. Decides what to do given the live global domain, the
/// wanted domain, and whether the wanted domain permits the configured channel.
/// Identical contract to the radio-side reconcile policy so both halves behave
/// the same. SAFETY: never returns `Reassert` for a malformed/world domain or
/// when the channel is not permitted.
pub fn reconcile_decision(
    live: Option<&str>,
    wanted: &str,
    channel_permitted_by_wanted: bool,
) -> ReconcileDecision {
    let want = wanted.trim().to_ascii_uppercase();
    if !is_forceable_domain(&want) {
        return ReconcileDecision::NoWanted;
    }
    if let Some(d) = live {
        if d.eq_ignore_ascii_case(&want) {
            return ReconcileDecision::InSync;
        }
    }
    if !channel_permitted_by_wanted {
        return ReconcileDecision::SkipChannelUnsafe;
    }
    ReconcileDecision::Reassert {
        from: live.map(|d| d.to_ascii_uppercase()),
        to: want,
    }
}

/// The periodic regulatory reconciler. Holds the last-attempt timestamp so the
/// reconcile is throttled to the configured interval regardless of how fast the
/// monitor pass runs. The `events` shipper is only driven on a real re-assert.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct RegReconciler {
    last_tick: Option<Instant>,
    events: EventEmitter,
}

impl RegReconciler {
    /// Build a reconciler that records re-assert events through `events`.
    pub fn new(events: EventEmitter) -> Self {
        RegReconciler {
            last_tick: None,
            events,
        }
    }

    /// Whether the reconcile is due given the configured interval and the last
    /// attempt time. Pure so the throttle is testable without a real clock.
    #[cfg(any(target_os = "linux", test))]
    fn due(&self, interval: Duration, now: Instant) -> bool {
        match self.last_tick {
            None => true,
            Some(last) => now.duration_since(last) >= interval,
        }
    }

    /// One reconcile tick: throttle to the interval, read the wanted domain +
    /// the live global domain, and re-assert the wanted domain when the live one
    /// has drifted and the wanted one permits the rendezvous channel. Re-reads
    /// config each tick so an edit takes effect without a restart. A no-op when
    /// disabled, when not due, when `iw` is absent, or when the domain is already
    /// in sync.
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

        if !iw_available().await {
            return;
        }
        let wanted = read_wanted();
        let live = active_global_reg_domain().await;

        // Cheap common path: already correct, no `iw phy channels` read needed.
        if let ReconcileDecision::InSync = reconcile_decision(live.as_deref(), &wanted.domain, true)
        {
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
                self.events.emit(
                    REG_REASSERT_KIND,
                    ados_protocol::logd::Level::Info,
                    reg_reassert_detail(from.as_deref(), &to, wanted.channel, true),
                );
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn tick(&mut self) {}
}

/// Build the `radio.reg_reasserted` detail map. All fields are bland and
/// reader-facing. Mirrors the radio-side detail shape so the two halves write
/// the same event schema. Built only on the Linux re-assert path.
#[cfg(any(target_os = "linux", test))]
fn reg_reassert_detail(
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
fn read_config() -> RegReconcilerConfig {
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
// Pure parsing (unit-tested on every host)
// ---------------------------------------------------------------------------

/// Parse the global regulatory country from `iw reg get` output: the first
/// `country XX:` line (before any per-phy self-managed block). Returns the
/// uppercase two-character code, or `None`. Pure.
#[cfg(any(target_os = "linux", test))]
fn parse_global_reg_domain(text: &str) -> Option<String> {
    for line in text.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("country ") {
            let cc: String = rest.chars().take(2).collect();
            if cc.len() == 2 {
                return Some(cc.to_ascii_uppercase());
            }
        }
    }
    None
}

/// Extract the `phyN` wiphy name from `iw <iface> info` output (the `wiphy <N>`
/// line). Returns e.g. `"phy0"`, or `None`. Pure.
#[cfg(any(target_os = "linux", test))]
fn parse_wiphy(info: &str) -> Option<String> {
    for line in info.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("wiphy ") {
            let n = rest.split_whitespace().next()?;
            if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                return Some(format!("phy{}", n));
            }
        }
    }
    None
}

/// Parse `iw phy <phy> channels` output into the set of usable channel numbers
/// (the `[<channel>]` token on a line not marked `disabled` / `no ir` /
/// `radar`). An empty set means "could not determine". Pure. Identical filter
/// to the radio-side `parse_enabled_channels` so the two halves agree.
#[cfg(any(target_os = "linux", test))]
fn parse_enabled_channels(text: &str) -> std::collections::BTreeSet<u8> {
    let mut out = std::collections::BTreeSet::new();
    for line in text.lines() {
        let Some(start) = line.find('[') else {
            continue;
        };
        let Some(len) = line[start + 1..].find(']') else {
            continue;
        };
        let token = &line[start + 1..start + 1 + len];
        let Ok(ch) = token.parse::<u8>() else {
            continue;
        };
        let low = line.to_lowercase();
        if low.contains("disabled") || low.contains("no ir") || low.contains("radar") {
            continue;
        }
        out.insert(ch);
    }
    out
}

/// First WFB-compatible injection interface from `iw dev` output, or `None`. The
/// channel-safety read needs the injection adapter's wiphy. We do not parse the
/// driver here (that needs sysfs); the wiphy channel set is the same for any
/// interface on that phy, and the only interface whose enabled set matters for
/// the WFB channel is the injection adapter — which is the only one whose phy
/// would carry the U-NII-3 channels in the first place. We pick the first phy
/// whose enabled set contains the target channel, so an onboard 2.4 GHz phy is
/// naturally skipped. Pure.
#[cfg(any(target_os = "linux", test))]
fn parse_interfaces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("Interface ") {
            let name = rest.trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    out
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
    parse_global_reg_domain(&out)
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
    let ifaces = parse_interfaces(&dev);
    if ifaces.is_empty() {
        return true;
    }
    let mut any_set_read = false;
    for iface in ifaces {
        let Some(info) = run_output("iw", &[&iface, "info"]).await else {
            continue;
        };
        let Some(phy) = parse_wiphy(&info) else {
            continue;
        };
        let Some(chans) = run_output("iw", &["phy", &phy, "channels"]).await else {
            continue;
        };
        let enabled = parse_enabled_channels(&chans);
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

    // ----- reconciler config parsing -----

    #[test]
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(
            cfg.tick_interval,
            Duration::from_secs(DEFAULT_TICK_INTERVAL_S)
        );
    }

    #[test]
    fn explicit_disable_is_honored() {
        let cfg = read_config_from("network:\n  reg_reconciler:\n    enabled: false\n");
        assert!(!cfg.enabled);
    }

    #[test]
    fn explicit_interval_parses_and_floors_at_one() {
        let cfg = read_config_from(
            "network:\n  reg_reconciler:\n    enabled: true\n    tick_interval_s: 15\n",
        );
        assert!(cfg.enabled);
        assert_eq!(cfg.tick_interval, Duration::from_secs(15));
        let zero = read_config_from("network:\n  reg_reconciler:\n    tick_interval_s: 0\n");
        assert_eq!(zero.tick_interval, Duration::from_secs(1));
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        let cfg = read_config_from(": : : not yaml");
        assert!(cfg.enabled);
        assert_eq!(
            cfg.tick_interval,
            Duration::from_secs(DEFAULT_TICK_INTERVAL_S)
        );
    }

    // ----- wanted domain + channel resolution (shared with the radio config) -----

    #[test]
    fn wanted_defaults_when_absent() {
        let w = read_wanted_from("agent:\n  name: x\n");
        assert_eq!(w.domain, "US");
        assert_eq!(w.channel, 149);
    }

    #[test]
    fn wanted_reads_reg_domain_and_channel() {
        let w = read_wanted_from("video:\n  wfb:\n    reg_domain: in\n    channel: 165\n");
        // Uppercased.
        assert_eq!(w.domain, "IN");
        assert_eq!(w.channel, 165);
    }

    #[test]
    fn wanted_rendezvous_pin_overrides_home_channel() {
        let w = read_wanted_from(
            "video:\n  wfb:\n    channel: 149\n    rendezvous_channel: 153\n    reg_domain: US\n",
        );
        assert_eq!(w.channel, 153);
        assert_eq!(w.domain, "US");
    }

    #[test]
    fn wanted_empty_reg_domain_falls_back_to_default() {
        let w = read_wanted_from("video:\n  wfb:\n    reg_domain: ''\n    channel: 149\n");
        assert_eq!(w.domain, "US");
    }

    // ----- forceable-domain predicate -----

    #[test]
    fn forceable_domain_predicate() {
        assert!(is_forceable_domain("US"));
        assert!(is_forceable_domain("IN"));
        assert!(is_forceable_domain("BO"));
        // World default is never forced (would cap the radio).
        assert!(!is_forceable_domain("00"));
        assert!(!is_forceable_domain("USA"));
        assert!(!is_forceable_domain(""));
    }

    // ----- pure reconcile policy -----

    #[test]
    fn in_sync_no_op() {
        assert_eq!(
            reconcile_decision(Some("US"), "US", true),
            ReconcileDecision::InSync
        );
        assert_eq!(
            reconcile_decision(Some("us"), "US", true),
            ReconcileDecision::InSync
        );
    }

    #[test]
    fn reassert_away_from_bolivia() {
        assert_eq!(
            reconcile_decision(Some("BO"), "US", true),
            ReconcileDecision::Reassert {
                from: Some("BO".to_string()),
                to: "US".to_string(),
            }
        );
    }

    #[test]
    fn reassert_when_live_unreadable() {
        assert_eq!(
            reconcile_decision(None, "IN", true),
            ReconcileDecision::Reassert {
                from: None,
                to: "IN".to_string(),
            }
        );
    }

    #[test]
    fn skip_when_channel_not_permitted_by_wanted() {
        assert_eq!(
            reconcile_decision(Some("BO"), "US", false),
            ReconcileDecision::SkipChannelUnsafe
        );
    }

    #[test]
    fn never_force_world_or_malformed() {
        assert_eq!(
            reconcile_decision(Some("BO"), "00", true),
            ReconcileDecision::NoWanted
        );
        assert_eq!(
            reconcile_decision(Some("BO"), "", true),
            ReconcileDecision::NoWanted
        );
    }

    #[test]
    fn never_forces_bolivia_as_target() {
        // Even if BO is somehow the live value, the reconcile only ever moves
        // TOWARD the configured (sane) wanted domain, never toward BO.
        match reconcile_decision(Some("BO"), "IN", true) {
            ReconcileDecision::Reassert { to, .. } => assert_eq!(to, "IN"),
            other => panic!("expected re-assert to IN, got {other:?}"),
        }
    }

    // ----- the throttle gate -----

    #[tokio::test]
    async fn due_when_never_ticked_then_throttled() {
        let r = RegReconciler::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ));
        let now = Instant::now();
        // Never ticked → due.
        assert!(r.due(Duration::from_secs(30), now));
        // Simulate a recent tick by constructing one with last_tick set.
        let mut r2 = RegReconciler::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ));
        r2.last_tick = Some(now);
        // Not yet due inside the interval.
        assert!(!r2.due(Duration::from_secs(30), now + Duration::from_secs(10)));
        // Due once the interval elapses.
        assert!(r2.due(Duration::from_secs(30), now + Duration::from_secs(31)));
    }

    // ----- iw parsers -----

    #[test]
    fn parses_global_reg_domain_before_self_managed_block() {
        let text = "\
global
country BO: DFS-FCC
        (5170 - 5250 @ 80), (24)
phy#3 (self-managed)
country US: DFS-FCC
";
        // The FIRST country line is the global domain.
        assert_eq!(parse_global_reg_domain(text).as_deref(), Some("BO"));
    }

    #[test]
    fn parses_wiphy_and_channels() {
        let info = "Interface wlan1\n\twiphy 3\n\ttype monitor\n";
        assert_eq!(parse_wiphy(info).as_deref(), Some("phy3"));
        let chans = "\
* 5745 MHz [149] (24.0 dBm)
* 5765 MHz [153] (disabled)
* 5260 MHz [52] (no IR, radar detection)
* 5825 MHz [165] (24.0 dBm)
";
        let enabled = parse_enabled_channels(chans);
        assert!(enabled.contains(&149));
        assert!(enabled.contains(&165));
        assert!(!enabled.contains(&153)); // disabled
        assert!(!enabled.contains(&52)); // radar / no IR
    }

    #[test]
    fn parses_interface_list() {
        let dev = "\
phy#3
\tInterface wlan1
\t\ttype monitor
phy#0
\tInterface wlan0
\t\ttype managed
";
        assert_eq!(parse_interfaces(dev), vec!["wlan1", "wlan0"]);
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
