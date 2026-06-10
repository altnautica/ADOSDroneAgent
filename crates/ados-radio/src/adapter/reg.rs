//! Regulatory-domain set / verify / reconcile + the channel-readiness gate.
//!
//! Applies the global reg domain via `iw reg set` and verifies the readback
//! with bounded retry (failing fast + loud on a self-managed PHY's baked-country
//! override), reads the per-interface enabled / DFS channel sets, gates the
//! rendezvous channel against the domain, and reconciles the global domain back
//! to the configured value when an injection PHY's baked country displaces it.
//! The pure parsers + the channel gate are unit-testable without `iw`.

use super::{run_cmd, run_cmd_output};

/// True when `domain` is a well-formed regulatory domain code: exactly two
/// characters, each an uppercase ASCII letter or digit (`/^[A-Z0-9]{2}$/`).
/// Pure so the format gate is unit-testable without `iw`.
fn is_valid_reg_domain(domain: &str) -> bool {
    domain.len() == 2
        && domain
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// A failed regulatory-domain precondition. The radio bring-up treats this as a
/// hard precondition: the interface is never brought into monitor mode and no
/// channel is set while one of these holds, so the driver can never radiate on a
/// band the active domain forbids (the silent power-cap class).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegError {
    /// The `iw reg set` command failed to run or returned non-zero.
    CommandFailed,
    /// The domain string is not a 2-char ISO 3166-1 alpha-2 / `00` world code.
    InvalidFormat,
    /// After the bounded retries, `iw reg get` never reported the wanted domain.
    /// `got` is the last-observed global country, when readable.
    VerifyTimeout { want: String, got: Option<String> },
    /// A self-managed phy carries a baked country that overrides the global set:
    /// the global `iw reg set` cannot displace it, so the radio would run capped
    /// on the wanted band. Surfaced distinctly so the operator sees the conflict
    /// rather than a silently power-capped link.
    EepromOverride { want: String, got: String },
    /// The rendezvous channel is not in the domain's enabled channel set.
    ChannelNotEnabled { channel: u8 },
    /// The rendezvous channel needs DFS clearance and `dfs_allowed` is off.
    ChannelIsDfs { channel: u8 },
}

impl std::fmt::Display for RegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegError::CommandFailed => write!(f, "reg command failed or unavailable"),
            RegError::InvalidFormat => write!(f, "invalid regulatory domain format"),
            RegError::VerifyTimeout { want, got } => {
                write!(f, "reg domain verify timeout (want={want}, got={got:?})")
            }
            RegError::EepromOverride { want, got } => {
                write!(f, "phy override (want={want}, got={got})")
            }
            RegError::ChannelNotEnabled { channel } => {
                write!(f, "channel {channel} not enabled in this domain")
            }
            RegError::ChannelIsDfs { channel } => {
                write!(f, "channel {channel} needs DFS clearance")
            }
        }
    }
}

impl std::error::Error for RegError {}

impl RegError {
    /// A short, stable token for the wfb-stats `reg_block_reason` field and the
    /// structured log. Bland and reader-facing; no internal identifiers.
    pub fn reason_code(&self) -> &'static str {
        match self {
            RegError::CommandFailed => "command_failed",
            RegError::InvalidFormat => "invalid_format",
            RegError::VerifyTimeout { .. } => "verify_timeout",
            RegError::EepromOverride { .. } => "phy_override",
            RegError::ChannelNotEnabled { .. } => "channel_not_enabled",
            RegError::ChannelIsDfs { .. } => "channel_dfs",
        }
    }
}

/// Number of `iw reg set` attempts before the gate declares a verify timeout.
const REG_SET_MAX_ATTEMPTS: u32 = 3;
/// Pause between reg-set attempts. With 3 attempts this spans ~6 s, matching the
/// bounded-retry budget; the per-attempt readback poll adds up to ~2 s each.
const REG_SET_RETRY_INTERVAL_MS: u64 = 2000;
/// Ceiling on the per-attempt `iw reg get` readback poll (the set is async).
const REG_VERIFY_POLL_CEILING_MS: u64 = 2000;
/// Cadence of the per-attempt readback poll.
const REG_VERIFY_POLL_STEP_MS: u64 = 100;

/// Apply the regulatory domain via `iw reg set <domain>` and verify the readback
/// with bounded retry. Returns `Ok(())` only when `iw reg get` reports the wanted
/// global country.
///
/// This is a hard precondition for the radio bring-up. It must run BEFORE the
/// interface is brought up in monitor mode: the kernel maps the permitted channel
/// set and the per-channel TX-power ceiling when the driver initialises, so a
/// domain set afterwards is too late and leaves the home channel (149, U-NII-3 /
/// 5745 MHz) capped to the startup domain's limits (the -100 dBm "not permitted"
/// sentinel, zero injected frames).
///
/// On an empty `domain` this is a no-op (`Ok(())`) — the caller opted out of
/// setting one. A malformed domain returns `InvalidFormat`. After
/// [`REG_SET_MAX_ATTEMPTS`] failed verifications it returns `VerifyTimeout`. When
/// a self-managed phy re-asserts a baked country that overrides the global set, it
/// returns `EepromOverride` instead of silently running capped.
///
/// This never touches an interface — `iw reg set` is a global per-phy call — so
/// it cannot disturb the operator's management link.
pub async fn set_reg_domain(domain: &str) -> Result<(), RegError> {
    if domain.is_empty() {
        return Ok(());
    }
    // An ISO 3166-1 alpha-2 country / `00` world domain is exactly two chars,
    // each `A-Z` or `0-9`. Reject anything else before it reaches `iw reg set`
    // so a malformed value (stray whitespace, a full name, an injected token)
    // is never handed to the command.
    if !is_valid_reg_domain(domain) {
        tracing::warn!(domain, "wfb_reg_domain_rejected_format");
        return Err(RegError::InvalidFormat);
    }
    let want = domain.to_ascii_uppercase();
    let mut cmd_ran_at_least_once = false;
    for attempt in 0..REG_SET_MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(
                REG_SET_RETRY_INTERVAL_MS,
            ))
            .await;
        }
        if run_cmd("iw", &["reg", "set", &want]).await.is_err() {
            tracing::warn!(domain = %want, attempt, "wfb_reg_set_cmd_failed");
            continue;
        }
        cmd_ran_at_least_once = true;
        // Poll the readback for this attempt.
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(REG_VERIFY_POLL_CEILING_MS);
        loop {
            if active_global_reg_domain().await.as_deref() == Some(want.as_str()) {
                tracing::info!(domain = %want, verified = true, "wfb_reg_domain_verified");
                return Ok(());
            }
            // A self-managed phy that re-asserts a different baked country is an
            // unrecoverable conflict, not a timing issue — fail fast and loud.
            if let Some((phy, baked)) = first_conflicting_self_managed_phy(&want).await {
                tracing::error!(
                    want = %want,
                    got = %baked,
                    phy = %phy,
                    "wfb_reg_phy_override"
                );
                return Err(RegError::EepromOverride { want, got: baked });
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(REG_VERIFY_POLL_STEP_MS)).await;
        }
    }
    if !cmd_ran_at_least_once {
        tracing::error!(domain = %want, "wfb_reg_set_unavailable");
        return Err(RegError::CommandFailed);
    }
    let got = active_global_reg_domain().await;
    tracing::error!(want = %want, got = ?got, "wfb_reg_domain_verify_timeout");
    Err(RegError::VerifyTimeout { want, got })
}

/// Validate that the radio is clear to bring up on `channel`: the channel must be
/// in the domain's enabled set, and (unless `dfs_allowed`) must not be a DFS
/// channel. Call after [`set_reg_domain`] succeeds and after `enabled_channels`
/// has been read for the interface.
///
/// `enabled` is the regulatory-permitted set from [`enabled_channels`], which
/// already filters DFS / no-IR / disabled channels. An empty set means the wiphy
/// list could not be read; the gate treats that as "could not determine" and
/// passes (matching the existing "empty = do not restrict" convention) so a board
/// whose channel list is unreadable still comes up rather than wedging.
///
/// `dfs_channels` is the set of channels the same readout flagged as needing DFS
/// clearance. When the rendezvous channel sits in that set and `dfs_allowed` is
/// off, this returns `ChannelIsDfs` so a DFS home is refused at preflight.
pub fn assert_reg_ready(
    channel: u8,
    enabled: &std::collections::BTreeSet<u8>,
    dfs_channels: &std::collections::BTreeSet<u8>,
    dfs_allowed: bool,
) -> Result<(), RegError> {
    // Could not read the wiphy channel list: do not restrict (the radio may
    // still come up on a permissive driver). Never wedge on unknown.
    if enabled.is_empty() {
        return Ok(());
    }
    if !dfs_allowed && dfs_channels.contains(&channel) {
        return Err(RegError::ChannelIsDfs { channel });
    }
    if !enabled.contains(&channel) {
        return Err(RegError::ChannelNotEnabled { channel });
    }
    Ok(())
}

/// The 5 GHz channel numbers this adapter can actually use for the link.
///
/// Parses `iw <iface> info` to find the wiphy, then `iw phy <phyN> channels`,
/// keeping only channels that are not `(disabled)` and not radar / `no IR` (DFS
/// channels need a channel-availability check the link does not perform). The
/// drone and ground frequently run different regulatory domains, so the air
/// channel must be in the intersection of both sides' enabled sets; this exposes
/// the local half. An empty set means "could not determine"; callers treat that
/// as "do not restrict".
#[cfg(target_os = "linux")]
pub async fn enabled_channels(iface: &str) -> std::collections::BTreeSet<u8> {
    let info = match run_cmd_output("iw", &[iface, "info"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    let Some(phy) = parse_wiphy(&info) else {
        return std::collections::BTreeSet::new();
    };
    let chans = match run_cmd_output("iw", &["phy", &phy, "channels"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    parse_enabled_channels(&chans)
}

#[cfg(not(target_os = "linux"))]
pub async fn enabled_channels(_iface: &str) -> std::collections::BTreeSet<u8> {
    std::collections::BTreeSet::new()
}

/// Extract the `phyN` wiphy name from `iw <iface> info` output (the `wiphy <N>`
/// line). Returns e.g. `"phy0"`, or `None` when absent.
#[cfg(any(target_os = "linux", test))]
fn parse_wiphy(info: &str) -> Option<String> {
    for line in info.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("wiphy ") {
            let n = rest.split_whitespace().next()?;
            if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() {
                return Some(format!("phy{}", n));
            }
        }
    }
    None
}

/// Parse `iw phy <phy> channels` output into the set of usable channel numbers.
/// A line carries a `[<channel>]` token; it is kept only when the line is not
/// marked `disabled`, `no ir`, or `radar`.
#[cfg(any(target_os = "linux", test))]
fn parse_enabled_channels(text: &str) -> std::collections::BTreeSet<u8> {
    let mut out = std::collections::BTreeSet::new();
    for line in text.lines() {
        // The channel number sits inside square brackets, e.g.
        //   "* 5745 MHz [149]"                       (usable)
        //   "* 5180 MHz [36] (disabled)"             (skip)
        //   "* 5260 MHz [52] (no IR, radar detection)" (skip)
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

/// The DFS / no-IR / radar channels for this interface's domain — the channels a
/// rendezvous home must avoid unless `dfs_allowed`. Reads the same
/// `iw phy <phy> channels` output as [`enabled_channels`] and keeps the channels
/// it marks `no ir` / `radar`. An empty set means "could not determine".
#[cfg(target_os = "linux")]
pub async fn dfs_channels(iface: &str) -> std::collections::BTreeSet<u8> {
    let info = match run_cmd_output("iw", &[iface, "info"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    let Some(phy) = parse_wiphy(&info) else {
        return std::collections::BTreeSet::new();
    };
    let chans = match run_cmd_output("iw", &["phy", &phy, "channels"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    parse_dfs_channels(&chans)
}

#[cfg(not(target_os = "linux"))]
pub async fn dfs_channels(_iface: &str) -> std::collections::BTreeSet<u8> {
    std::collections::BTreeSet::new()
}

/// Parse `iw phy <phy> channels` into the set of channels that need DFS
/// clearance (lines marked `no ir` or `radar`). A `disabled` channel is not a
/// DFS channel — it is simply unavailable — so it is excluded here.
#[cfg(any(target_os = "linux", test))]
fn parse_dfs_channels(text: &str) -> std::collections::BTreeSet<u8> {
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
        if low.contains("disabled") {
            continue;
        }
        if low.contains("no ir") || low.contains("radar") {
            out.insert(ch);
        }
    }
    out
}

/// Return the first self-managed phy whose baked country differs from
/// `global_want`, or `None`. This is the unrecoverable EEPROM-override case: a
/// global `iw reg set` cannot displace a self-managed phy's baked country, so the
/// radio on that phy would run capped on the wanted band.
async fn first_conflicting_self_managed_phy(global_want: &str) -> Option<(String, String)> {
    let out = run_cmd_output("iw", &["reg", "get"]).await.ok()?;
    parse_conflicting_self_managed_phy(&out, global_want)
}

/// Pure parser for the EEPROM-override detection. Walks `iw reg get` output: a
/// `phyN (self-managed)` header opens a block, and the first `country XX:` line
/// inside it is that phy's baked country. Returns the first `(phy, country)` whose
/// country differs from `global_want`. Tolerant of the `self managed` /
/// `self-managed` spelling variants `iw` has used. Pure so it is unit-testable
/// without `iw`.
fn parse_conflicting_self_managed_phy(text: &str, global_want: &str) -> Option<(String, String)> {
    let want = global_want.to_ascii_uppercase();
    let mut current_phy: Option<String> = None;
    for line in text.lines() {
        let s = line.trim();
        let low = s.to_lowercase();
        // A self-managed phy block header, e.g. "phy#3 (self-managed)" or
        // "phy3 (self managed)". The phy token may carry a '#'.
        if low.starts_with("phy") && (low.contains("self managed") || low.contains("self-managed"))
        {
            let raw = s.split_whitespace().next().unwrap_or("");
            let phy = raw.trim_start_matches("phy#").trim_start_matches("phy");
            current_phy = Some(format!("phy{phy}"));
            continue;
        }
        if let Some(rest) = s.strip_prefix("country ") {
            let cc: String = rest
                .chars()
                .take(2)
                .collect::<String>()
                .to_ascii_uppercase();
            if cc.len() == 2 {
                if let Some(phy) = current_phy.take() {
                    if cc != want {
                        return Some((phy, cc));
                    }
                }
            }
        }
    }
    None
}

/// The regulatory domain actually in force plus whether it matches the wanted
/// domain. `domain` is the live global country from `iw reg get` (e.g. `US`,
/// `BO`, `00`), or `None` when it could not be read. `verified` is true only
/// when the live domain equals `want` (case-insensitive). Surfaced on the
/// wfb-stats sidecar so a future regression (a forbidden domain the global set
/// could not displace) is visible in one glance instead of masked by a
/// configured-channel-and-locked report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegStatus {
    pub domain: Option<String>,
    pub verified: bool,
}

/// Read the live regulatory domain and report whether it matches `want`. A
/// read-only `iw reg get` call; it never touches an interface, so it cannot
/// disturb the operator's management link. `want` is the domain the gate asked
/// for (the resolved `reg_domain`); an empty `want` reports the live domain with
/// `verified=false` (nothing to match against).
pub async fn read_reg_status(want: &str) -> RegStatus {
    let domain = active_global_reg_domain().await;
    let verified = reg_is_verified(domain.as_deref(), want);
    RegStatus { domain, verified }
}

/// Pure verification decision: true only when a known live `domain` equals the
/// wanted domain (case-insensitive) and `want` is non-empty. Split out from
/// [`read_reg_status`] so the match logic is testable without `iw`.
fn reg_is_verified(domain: Option<&str>, want: &str) -> bool {
    !want.is_empty()
        && domain
            .map(|d| d.eq_ignore_ascii_case(want))
            .unwrap_or(false)
}

/// The outcome of one regulatory-domain reconcile attempt. Returned so the
/// caller can emit the durable `radio.reg_reasserted` event with the from/to and
/// the channel-safety result without re-reading any state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassertOutcome {
    /// The live global domain already equalled the wanted domain: no action.
    InSync,
    /// The wanted domain was empty / malformed / the world default, so there was
    /// nothing safe to force.
    NoWanted,
    /// The wanted domain would not permit the configured channel, so forcing it
    /// would cap the radio. The re-assert was skipped.
    SkippedChannelUnsafe,
    /// The wanted domain was re-asserted. Carries the from/to countries and
    /// whether `iw reg set` verified (false = the set was issued but the readback
    /// did not confirm within the bounded retry, e.g. an EEPROM-override that the
    /// global set cannot displace — still worth recording the attempt).
    Reasserted {
        from: Option<String>,
        to: String,
        verified: bool,
    },
}

/// Reconcile the GLOBAL regulatory domain back to the configured `wanted` value,
/// re-asserting it when a self-managed injection PHY has left a different baked
/// country (e.g. `BO`) as the effective global domain.
///
/// This is the PREVENTION layer for the onboard-WiFi data-path break: a normal
/// onboard FullMAC adapter obeys the global domain, and when an injection PHY's
/// baked country becomes the global domain the onboard WiFi can keep its
/// association yet lose its data path. Re-asserting the sane wanted domain keeps
/// the onboard link working. The reactive WiFi self-heal stays as the backstop.
///
/// Safety: the re-assert is gated on the wanted domain PERMITTING the configured
/// `channel`. The caller passes the channel-vs-domain validation already used by
/// the bring-up gate (`assert_reg_ready` over the interface's `enabled_channels`
/// / `dfs_channels`), so this can never force a domain that caps the radio onto a
/// forbidden frequency. The world default (`00`) and any malformed domain are
/// refused. The call is idempotent — a no-op when the live domain already equals
/// the wanted value.
///
/// `channel_permitted_by_wanted` is the precomputed result of the channel gate
/// under the wanted domain; the caller computes it once (it already reads the
/// enabled set for the bring-up) and hands it in so this function does not repeat
/// the `iw phy channels` read. Returns the [`ReassertOutcome`] for the event.
pub async fn reconcile_reg_domain(
    wanted: &str,
    channel: u8,
    channel_permitted_by_wanted: bool,
) -> ReassertOutcome {
    let live = active_global_reg_domain().await;
    match crate::reg_reassert::reconcile_decision(
        live.as_deref(),
        wanted,
        channel_permitted_by_wanted,
    ) {
        crate::reg_reassert::ReassertDecision::InSync => ReassertOutcome::InSync,
        crate::reg_reassert::ReassertDecision::NoWanted => ReassertOutcome::NoWanted,
        crate::reg_reassert::ReassertDecision::SkipChannelUnsafe => {
            tracing::warn!(
                wanted,
                channel,
                live = ?live,
                note = "wanted domain would not permit the rendezvous channel; not re-asserting",
                "wfb_reg_reassert_skipped_channel_unsafe"
            );
            ReassertOutcome::SkippedChannelUnsafe
        }
        crate::reg_reassert::ReassertDecision::Reassert { from, to } => {
            // Re-issue the global set + verify with the same bounded retry the
            // bring-up gate uses. A self-managed PHY that re-asserts its baked
            // country yields EepromOverride / VerifyTimeout here; we still record
            // the attempt (verified=false) so the action is visible.
            let verified = set_reg_domain(&to).await.is_ok();
            if verified {
                tracing::info!(
                    from = ?from,
                    to = %to,
                    channel,
                    "wfb_reg_domain_reasserted"
                );
            } else {
                tracing::warn!(
                    from = ?from,
                    to = %to,
                    channel,
                    note = "re-assert issued but readback did not confirm (possible phy override)",
                    "wfb_reg_domain_reassert_unconfirmed"
                );
            }
            ReassertOutcome::Reasserted { from, to, verified }
        }
    }
}

/// Return the global regulatory country from `iw reg get`, or None. The first
/// `country XX:` line is the global domain; per-phy self-managed blocks come
/// after it. The injection phy follows the global domain, so that is the one
/// that matters.
async fn active_global_reg_domain() -> Option<String> {
    let out = run_cmd_output("iw", &["reg", "get"]).await.ok()?;
    for line in out.lines() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reg_domain_format_accepts_valid_rejects_malformed() {
        // Two uppercase letters or digits → accepted.
        assert!(is_valid_reg_domain("IN"));
        assert!(is_valid_reg_domain("US"));
        assert!(is_valid_reg_domain("00")); // world domain
                                            // Anything else → rejected before it reaches `iw reg set`.
        assert!(!is_valid_reg_domain("in")); // lowercase
        assert!(!is_valid_reg_domain("USA")); // too long
        assert!(!is_valid_reg_domain("I")); // too short
        assert!(!is_valid_reg_domain("")); // empty
        assert!(!is_valid_reg_domain("I N")); // whitespace / wrong length
        assert!(!is_valid_reg_domain("U;")); // injected punctuation
    }

    #[test]
    fn parse_dfs_channels_keeps_radar_and_no_ir_only() {
        let text = "\
Band 2:
	Frequencies:
		* 5180 MHz [36] (disabled)
		* 5200 MHz [40] (20.0 dBm)
		* 5260 MHz [52] (no IR, radar detection)
		* 5300 MHz [60] (radar detection)
		* 5745 MHz [149] (30.0 dBm)
";
        let got = parse_dfs_channels(text);
        // 36 is disabled (not DFS), 40/149 are usable (not DFS); 52/60 are DFS.
        let want: std::collections::BTreeSet<u8> = [52, 60].into_iter().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn assert_reg_ready_passes_when_channel_enabled_and_non_dfs() {
        let enabled: std::collections::BTreeSet<u8> = [36, 40, 149, 153].into_iter().collect();
        let dfs: std::collections::BTreeSet<u8> = [52, 60].into_iter().collect();
        assert!(assert_reg_ready(149, &enabled, &dfs, false).is_ok());
    }

    #[test]
    fn assert_reg_ready_rejects_channel_not_enabled() {
        let enabled: std::collections::BTreeSet<u8> = [36, 40, 149].into_iter().collect();
        let dfs = std::collections::BTreeSet::new();
        assert_eq!(
            assert_reg_ready(165, &enabled, &dfs, false),
            Err(RegError::ChannelNotEnabled { channel: 165 })
        );
    }

    #[test]
    fn assert_reg_ready_rejects_dfs_home_unless_allowed() {
        let enabled: std::collections::BTreeSet<u8> = [52, 149].into_iter().collect();
        let dfs: std::collections::BTreeSet<u8> = [52].into_iter().collect();
        // DFS home refused by default.
        assert_eq!(
            assert_reg_ready(52, &enabled, &dfs, false),
            Err(RegError::ChannelIsDfs { channel: 52 })
        );
        // Opt-in clears it (the channel is still in the enabled set).
        assert!(assert_reg_ready(52, &enabled, &dfs, true).is_ok());
    }

    #[test]
    fn assert_reg_ready_passes_when_enabled_set_unknown() {
        // An empty enabled set means "could not read the wiphy list" — the gate
        // must not wedge a board whose channel list is unreadable.
        let empty = std::collections::BTreeSet::new();
        let dfs = std::collections::BTreeSet::new();
        assert!(assert_reg_ready(149, &empty, &dfs, false).is_ok());
    }

    #[test]
    fn self_managed_phy_with_conflicting_country_is_detected() {
        // The live override shape: global says US, a self-managed phy bakes BO.
        let text = "\
global
country US: DFS-FCC
	(5170 - 5250 @ 80), (N/A, 17), (N/A)
phy#3 (self-managed)
country BO: DFS-UNSET
	(5170 - 5250 @ 80), (N/A, 20), (N/A)
";
        assert_eq!(
            parse_conflicting_self_managed_phy(text, "US"),
            Some(("phy3".to_string(), "BO".to_string()))
        );
    }

    #[test]
    fn self_managed_phy_matching_global_is_not_a_conflict() {
        // A self-managed phy that already carries the wanted country is fine.
        let text = "\
global
country US: DFS-FCC
phy#0 (self-managed)
country US: DFS-FCC
";
        assert_eq!(parse_conflicting_self_managed_phy(text, "US"), None);
    }

    #[test]
    fn non_self_managed_country_block_is_not_an_override() {
        // The plain global block (no self-managed phy) is never an override, even
        // when it differs from the wanted domain — that is the retry/timeout path,
        // not the unrecoverable EEPROM case.
        let text = "\
global
country BO: DFS-UNSET
	(5170 - 5250 @ 80), (N/A, 20), (N/A)
";
        assert_eq!(parse_conflicting_self_managed_phy(text, "US"), None);
    }

    #[test]
    fn self_managed_spelling_variants_both_parse() {
        // `iw` has emitted both "self managed" and "self-managed".
        let spaced = "phy3 (self managed)\ncountry BO: DFS-UNSET\n";
        assert_eq!(
            parse_conflicting_self_managed_phy(spaced, "US"),
            Some(("phy3".to_string(), "BO".to_string()))
        );
    }

    #[test]
    fn reg_error_reason_codes_are_stable_and_bland() {
        assert_eq!(RegError::CommandFailed.reason_code(), "command_failed");
        assert_eq!(RegError::InvalidFormat.reason_code(), "invalid_format");
        assert_eq!(
            RegError::VerifyTimeout {
                want: "US".into(),
                got: Some("BO".into())
            }
            .reason_code(),
            "verify_timeout"
        );
        assert_eq!(
            RegError::EepromOverride {
                want: "US".into(),
                got: "BO".into()
            }
            .reason_code(),
            "phy_override"
        );
        assert_eq!(
            RegError::ChannelNotEnabled { channel: 165 }.reason_code(),
            "channel_not_enabled"
        );
        assert_eq!(
            RegError::ChannelIsDfs { channel: 52 }.reason_code(),
            "channel_dfs"
        );
    }

    #[test]
    fn reg_is_verified_matches_wanted_domain_case_insensitively() {
        // Live domain equals the wanted domain → verified.
        assert!(reg_is_verified(Some("US"), "US"));
        // Case does not matter (iw can emit either).
        assert!(reg_is_verified(Some("us"), "US"));
        assert!(reg_is_verified(Some("US"), "us"));
        // A different live domain (the forbidden-band case) → not verified.
        assert!(!reg_is_verified(Some("BO"), "US"));
        // Unknown live domain (iw unreadable) → not verified.
        assert!(!reg_is_verified(None, "US"));
        // Empty wanted domain → nothing to match, never verified.
        assert!(!reg_is_verified(Some("US"), ""));
        assert!(!reg_is_verified(None, ""));
    }

    #[test]
    fn parse_wiphy_extracts_phy_name() {
        let info = "Interface wlan1\n\ttype monitor\n\twiphy 0\n";
        assert_eq!(parse_wiphy(info).as_deref(), Some("phy0"));
        let info2 = "Interface wlan1\n\twiphy 3\n";
        assert_eq!(parse_wiphy(info2).as_deref(), Some("phy3"));
    }

    #[test]
    fn parse_wiphy_missing_is_none() {
        assert!(parse_wiphy("Interface wlan1\n\ttype monitor\n").is_none());
    }

    #[test]
    fn parse_enabled_channels_keeps_usable_skips_disabled_and_dfs() {
        let text = "\
Band 2:
	Frequencies:
		* 5180 MHz [36] (disabled)
		* 5200 MHz [40] (20.0 dBm)
		* 5260 MHz [52] (no IR, radar detection)
		* 5300 MHz [60] (radar detection)
		* 5745 MHz [149] (30.0 dBm)
		* 5765 MHz [153] (30.0 dBm)
";
        let got = parse_enabled_channels(text);
        let want: std::collections::BTreeSet<u8> = [40, 149, 153].into_iter().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn parse_enabled_channels_empty_input_is_empty() {
        assert!(parse_enabled_channels("").is_empty());
        // A line with no bracket token contributes nothing.
        assert!(parse_enabled_channels("Band 2:\n\tFrequencies:\n").is_empty());
    }
}
