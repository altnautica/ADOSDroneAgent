//! Regulatory-domain reconciler for the radio bring-up.
//!
//! A self-managed-regulatory USB injection PHY (the RTL family) asserts its
//! EEPROM-baked country as the GLOBAL cfg80211 regulatory domain when it loads
//! and enters monitor mode. A normal onboard FullMAC adapter (the Pi-family
//! Broadcom, the Rock-family AIC8800) then obeys that global domain. When the
//! baked country is one whose rules the onboard driver cannot satisfy on its
//! associated channel, the onboard WiFi keeps its association and IP but loses
//! its data path entirely (the gateway neighbor never resolves, 100% loss), so
//! the management link dies and there is no failover.
//!
//! The reg gate already DETECTS this — `set_reg_domain` returns the
//! `EepromOverride` variant when a self-managed phy re-asserts a different baked
//! country — but it only reports it. The break is left in place. This module is
//! the PREVENTION layer: after the RTL monitor-mode churn (and on a periodic
//! check), it reads the live global domain and, when it is NOT the configured
//! wanted domain, RE-ASSERTS the wanted domain so the baked country never
//! remains the effective global domain. The onboard WiFi then runs under the
//! sane domain and keeps its data path.
//!
//! Safety: it only ever forces a domain that PERMITS the configured rendezvous
//! channel. The caller reuses the existing channel-vs-domain validation
//! (`assert_reg_ready`) before forcing, so the reconciler can never cap the
//! radio onto a frequency the wanted domain forbids. It never forces the
//! all-restrictive world default, and it is idempotent — a no-op when the live
//! domain already equals the wanted value.
//!
//! The detail builder is pure (testable without the daemon) and the decision is
//! a pure function so the reconcile contract is unit-tested without `iw`.

use ados_protocol::logd::{Fields, Level, Value as MpVal};

/// The event kind recorded when the reconciler re-asserts the global domain.
/// Bland and reader-facing: it names what the code did.
pub const REG_REASSERT_KIND: &str = "radio.reg_reasserted";

/// What the reconciler decided for one observation. Pure so the policy is
/// testable without any OS call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassertDecision {
    /// The live global domain already equals the wanted domain: nothing to do.
    InSync,
    /// The wanted domain is missing / malformed, so there is nothing safe to
    /// force. Leave the live domain as-is rather than asserting garbage.
    NoWanted,
    /// The wanted domain would not permit the configured channel, so forcing it
    /// would cap the radio. Skip the re-assert and surface the safety skip.
    SkipChannelUnsafe,
    /// The live domain differs from the wanted domain and the wanted domain
    /// permits the channel: re-assert. Carries the from/to countries.
    Reassert { from: Option<String>, to: String },
}

/// Pure reconcile policy. Decides what to do given the live global domain, the
/// wanted domain, and whether the wanted domain permits the configured channel.
///
/// - An empty / malformed `wanted` yields [`ReassertDecision::NoWanted`] (never
///   force an invalid domain).
/// - A `live` that already equals `wanted` (case-insensitive) yields
///   [`ReassertDecision::InSync`] (idempotent no-op).
/// - When the wanted domain does NOT permit the channel, yields
///   [`ReassertDecision::SkipChannelUnsafe`] so the reconciler can never cap the
///   radio onto a forbidden frequency.
/// - Otherwise yields [`ReassertDecision::Reassert`] with the from/to.
pub fn reconcile_decision(
    live: Option<&str>,
    wanted: &str,
    channel_permitted_by_wanted: bool,
) -> ReassertDecision {
    let want = wanted.trim().to_ascii_uppercase();
    // Never force a malformed / world / empty domain: it would cap the radio.
    if !is_forceable_domain(&want) {
        return ReassertDecision::NoWanted;
    }
    // Already correct: idempotent no-op.
    if let Some(d) = live {
        if d.eq_ignore_ascii_case(&want) {
            return ReassertDecision::InSync;
        }
    }
    // The wanted domain must permit the configured channel before we force it,
    // so the reconciler can never cap the WFB radio onto a forbidden frequency.
    if !channel_permitted_by_wanted {
        return ReassertDecision::SkipChannelUnsafe;
    }
    ReassertDecision::Reassert {
        from: live.map(|d| d.to_ascii_uppercase()),
        to: want,
    }
}

/// True when a wanted domain is a concrete, forceable country: exactly two
/// uppercase-ASCII-or-digit characters and NOT the all-restrictive world code
/// `00`. The world default permits almost nothing at usable power, so forcing it
/// would cap the radio — the reconciler refuses it, matching the safety
/// invariant "only ever force a domain that permits the configured channel".
pub fn is_forceable_domain(domain: &str) -> bool {
    domain.len() == 2
        && domain != "00"
        && domain
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Build the `radio.reg_reasserted` detail map. All fields are bland and
/// reader-facing:
///
/// - `interface` — the injection interface whose churn drove the live domain;
/// - `from_country` — the live domain that was wrong (omitted when unreadable);
/// - `to_country` — the wanted domain that was re-asserted;
/// - `wfb_channel` — the configured rendezvous channel the to-domain permits;
/// - `channel_permitted` — whether the to-domain permits that channel (the
///   safety check result; always true on an actual re-assert).
pub fn reg_reassert_detail(
    interface: &str,
    from_country: Option<&str>,
    to_country: &str,
    wfb_channel: u8,
    channel_permitted: bool,
) -> Fields {
    let mut d = Fields::new();
    d.insert("interface".to_string(), MpVal::from(interface));
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

/// The severity for a re-assert event: a proactive heal is informational.
pub const REG_REASSERT_SEVERITY: Level = Level::Info;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_sync_when_live_equals_wanted() {
        assert_eq!(
            reconcile_decision(Some("US"), "US", true),
            ReassertDecision::InSync
        );
        // Case-insensitive match.
        assert_eq!(
            reconcile_decision(Some("us"), "US", true),
            ReassertDecision::InSync
        );
    }

    #[test]
    fn reassert_when_live_differs_and_channel_permitted() {
        assert_eq!(
            reconcile_decision(Some("BO"), "US", true),
            ReassertDecision::Reassert {
                from: Some("BO".to_string()),
                to: "US".to_string(),
            }
        );
    }

    #[test]
    fn reassert_when_live_unreadable_and_channel_permitted() {
        // A momentarily-unreadable live domain still drives a re-assert: the
        // safe wanted domain is asserted so the global never lingers wrong.
        assert_eq!(
            reconcile_decision(None, "US", true),
            ReassertDecision::Reassert {
                from: None,
                to: "US".to_string(),
            }
        );
    }

    #[test]
    fn skip_when_wanted_would_not_permit_the_channel() {
        // Never force a domain that caps the WFB radio onto a forbidden channel.
        assert_eq!(
            reconcile_decision(Some("BO"), "US", false),
            ReassertDecision::SkipChannelUnsafe
        );
    }

    #[test]
    fn no_wanted_for_empty_or_malformed_or_world() {
        assert_eq!(
            reconcile_decision(Some("BO"), "", true),
            ReassertDecision::NoWanted
        );
        assert_eq!(
            reconcile_decision(Some("BO"), "USA", true),
            ReassertDecision::NoWanted
        );
        // The all-restrictive world default is never forced.
        assert_eq!(
            reconcile_decision(Some("BO"), "00", true),
            ReassertDecision::NoWanted
        );
    }

    #[test]
    fn never_forces_bolivia_when_it_is_the_live_domain() {
        // The trigger country (BO) as the live value with a sane wanted domain
        // drives a re-assert AWAY from BO, never toward it.
        match reconcile_decision(Some("BO"), "IN", true) {
            ReassertDecision::Reassert { from, to } => {
                assert_eq!(from.as_deref(), Some("BO"));
                assert_eq!(to, "IN");
            }
            other => panic!("expected a re-assert away from BO, got {other:?}"),
        }
    }

    #[test]
    fn forceable_domain_predicate() {
        assert!(is_forceable_domain("US"));
        assert!(is_forceable_domain("IN"));
        assert!(is_forceable_domain("BO"));
        // World default is not forceable (would cap the radio).
        assert!(!is_forceable_domain("00"));
        // Wrong length / lowercase / punctuation are not forceable.
        assert!(!is_forceable_domain("USA"));
        assert!(!is_forceable_domain("u"));
        assert!(!is_forceable_domain(""));
        assert!(!is_forceable_domain("u-"));
    }

    #[test]
    fn detail_is_bland_and_complete() {
        let d = reg_reassert_detail("wlan1", Some("BO"), "US", 149, true);
        assert_eq!(d.get("interface").and_then(|v| v.as_str()), Some("wlan1"));
        assert_eq!(d.get("from_country").and_then(|v| v.as_str()), Some("BO"));
        assert_eq!(d.get("to_country").and_then(|v| v.as_str()), Some("US"));
        assert_eq!(d.get("wfb_channel").and_then(|v| v.as_u64()), Some(149));
        assert_eq!(
            d.get("channel_permitted").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn detail_omits_from_country_when_unreadable() {
        let d = reg_reassert_detail("wlan1", None, "US", 149, true);
        assert!(!d.contains_key("from_country"));
        assert_eq!(d.get("to_country").and_then(|v| v.as_str()), Some("US"));
    }
}
