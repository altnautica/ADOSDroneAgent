//! The node's resolvable mDNS reach name — one definition for every surface
//! that hands an operator or a GCS a hostname to dial.
//!
//! # Why this is not a formatted device id
//!
//! Publishing an mDNS service record with an arbitrary `server=` does not
//! create a matching A/AAAA record. On a systemd host, avahi publishes exactly
//! one resolvable `<hostname>.local`: the system hostname. A constructed name
//! like `ados-<6hex>.local` therefore resolves on no network anywhere, which is
//! why the Python discovery service already refuses to report one
//! (`ados/services/discovery/__init__.py`).
//!
//! Anything that advertises a reach — a pairing probe, a claim response, an
//! mDNS SRV target, an installer success card — advertises the name below, so
//! the name a GCS stores as a node's canonical reach is the name the host
//! actually answers to.
//!
//! # The rule
//!
//! Read the system hostname, trim a trailing dot, and reject the values that
//! cannot be another machine's reach (`localhost`, a bare `127.*` literal,
//! empty). Return it verbatim when it already carries a domain, else append
//! `.local`. When no usable hostname exists, there is no resolvable reach:
//! callers get `None` and must say so rather than substituting a name.

/// The system hostname, unadorned, or `None` when the host has none usable.
///
/// Linux exposes it as a file, which is the cheap read on the boards this runs
/// on; elsewhere (a macOS workstation node, a dev host) fall back to the
/// `hostname` command. A trailing dot — legal in a FQDN and occasionally
/// present in `/proc/sys/kernel/hostname` on a host configured from DNS — is
/// trimmed, because `foo..local` resolves nowhere.
pub fn system_hostname() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
        })?;
    normalize_hostname(&raw)
}

/// The resolvable `.local` reach name for this host, or `None` when the host
/// has no hostname that could be another machine's reach.
///
/// `None` is a real answer: a node whose hostname is `localhost` has no mDNS
/// reach, and a caller must emit an empty/absent value rather than a name it
/// cannot prove.
pub fn mdns_hostname() -> Option<String> {
    system_hostname().map(|h| mdns_name_from(&h))
}

/// The whole rule applied to a hostname a caller already holds: reject the
/// values that cannot be another machine's reach, then name it.
///
/// This is [`mdns_hostname`] without the host read, for a caller that read the
/// hostname at a different point in its run (the installer holds one in its
/// summary data). Use it rather than pairing [`mdns_name_from`] with a
/// hand-rolled rejection — that split is how `localhost.local` gets onto an
/// operator's screen.
pub fn mdns_name_for(hostname: &str) -> Option<String> {
    normalize_hostname(hostname).map(|h| mdns_name_from(&h))
}

/// The reach name for an already-normalized hostname. Split out so the rule is
/// unit-testable without a host read, and so a caller that already holds a
/// hostname (the installer reads one at a different point in its run) applies
/// the identical rule.
pub fn mdns_name_from(hostname: &str) -> String {
    if hostname.contains('.') {
        hostname.to_string()
    } else {
        format!("{hostname}.local")
    }
}

/// Trim and validate a raw hostname read. `None` for anything that cannot be a
/// reach for a *different* machine: empty, `localhost` (any case), or a bare
/// IPv4 loopback literal.
fn normalize_hostname(raw: &str) -> Option<String> {
    let name = raw.trim().trim_end_matches('.').trim();
    if name.is_empty() {
        return None;
    }
    if name.eq_ignore_ascii_case("localhost") || name.eq_ignore_ascii_case("localhost.localdomain")
    {
        return None;
    }
    if name.starts_with("127.") {
        return None;
    }
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_hostname_becomes_the_dot_local_name_avahi_publishes() {
        assert_eq!(normalize_hostname("skynode\n").as_deref(), Some("skynode"));
        assert_eq!(mdns_name_from("skynode"), "skynode.local");
    }

    #[test]
    fn a_hostname_that_already_carries_a_domain_is_returned_verbatim() {
        // Appending `.local` to a DNS name produces a third name that resolves
        // nowhere — exactly the defect this module exists to prevent.
        assert_eq!(mdns_name_from("skynode.lan"), "skynode.lan");
        assert_eq!(mdns_name_from("skynode.local"), "skynode.local");
    }

    #[test]
    fn a_trailing_dot_is_trimmed_before_the_suffix_is_applied() {
        assert_eq!(normalize_hostname("skynode.").as_deref(), Some("skynode"));
    }

    #[test]
    fn a_hostname_that_cannot_be_another_machines_reach_has_no_reach_name() {
        for raw in ["", "   ", "localhost", "LocalHost", "localhost.localdomain"] {
            assert!(
                normalize_hostname(raw).is_none(),
                "{raw:?} must not be advertised as a reach"
            );
        }
        // A loopback literal is a reach for the caller and nobody else.
        assert!(normalize_hostname("127.0.0.1").is_none());
    }
}
