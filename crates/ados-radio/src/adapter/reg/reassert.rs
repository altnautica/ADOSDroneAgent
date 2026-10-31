//! Live regulatory-domain readback and reconcile.

use super::*;

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
pub(super) fn reg_is_verified(domain: Option<&str>, want: &str) -> bool {
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
pub(super) async fn active_global_reg_domain() -> Option<String> {
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
