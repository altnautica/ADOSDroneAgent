//! Pure reconcile policy for the regulatory-domain reconciler.
//!
//! The forceable-domain predicate and the decision function are OS-free so the
//! safety contract (never force the world default / a malformed code, never
//! re-assert onto a channel-unsafe domain) is unit-tested on every host.

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

#[cfg(test)]
mod tests;
