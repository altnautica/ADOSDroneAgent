//! Regulatory-domain set / verify / reconcile + the channel-readiness gate.
//!
//! Applies the global reg domain via `iw reg set` and verifies the readback
//! with bounded retry (failing fast + loud on a self-managed PHY's baked-country
//! override), reads the per-interface enabled / DFS channel sets, gates the
//! rendezvous channel against the domain, and reconciles the global domain back
//! to the configured value when an injection PHY's baked country displaces it.
//! The pure parsers + the channel gate are unit-testable without `iw`.

use super::{run_cmd, run_cmd_output};

mod channels;
mod reassert;

pub use channels::*;
pub use reassert::*;

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

#[cfg(test)]
mod tests;
