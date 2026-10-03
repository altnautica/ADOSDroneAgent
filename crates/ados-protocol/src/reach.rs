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
//! When avahi is running, ask it for the host name it actually publishes
//! (`GetHostNameFqdn` over D-Bus). That is the only name proven to resolve to
//! this machine: on a collision (two boards flashed from one image) avahi
//! renames this host to `<host>-2.local`, and `<host>.local` then answers for
//! the OTHER board. Without avahi, read the system hostname, trim a trailing
//! dot, reject the values that cannot be another machine's reach
//! (`localhost`, a bare `127.*` literal, empty), and name its first label under
//! `.local`, which is what an mDNS responder publishes for it. When no usable
//! hostname exists, there is no resolvable reach: callers get `None` and must
//! say so rather than substituting a name.

/// The system hostname, unadorned, or `None` when the host has none usable.
///
/// Linux exposes it as a file, which is the cheap read on the boards this runs
/// on and which tracks a hostname changed while the process runs. A trailing
/// dot — legal in a FQDN and occasionally present in
/// `/proc/sys/kernel/hostname` on a host configured from DNS — is trimmed,
/// because `foo..local` resolves nowhere.
///
/// Everywhere else (a macOS workstation node, a dev host) the portable read is
/// `hostname(1)`. This function is reached from async route handlers
/// (`ados-control`'s `/api/pairing/*`), and spawning a process inline on the
/// reactor parks a worker for the life of the command, so that fallback is
/// probed at most once per [`HOSTNAME_PROBE_TTL`] rather than per request. No
/// SBC ever reaches that leg at all.
pub fn system_hostname() -> Option<String> {
    if let Ok(raw) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        return normalize_hostname(&raw);
    }
    probed_hostname_at(
        std::time::Instant::now(),
        HOSTNAME_PROBE_TTL,
        read_hostname_command,
    )
}

/// Bounded reuse window for the non-Linux `hostname(1)` probe.
///
/// What is held is the PROBE RESULT, not a reach verdict, and it expires. This
/// name is used to BUILD an advertised reach — `/api/pairing/info`'s
/// `mdns_host` and the `_ados._tcp` SRV target — and a reach surface may only
/// advertise a name that resolves. Holding the first answer for the process
/// lifetime would pin `None` on a node whose hostname was still `localhost`
/// when the front started, so it would advertise no reach even after the
/// operator set one, and would pin a dead name on a node that was renamed.
const HOSTNAME_PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// The last non-Linux hostname probe: when it was taken, and what it read.
static HOSTNAME_PROBE: std::sync::Mutex<Option<(std::time::Instant, Option<String>)>> =
    std::sync::Mutex::new(None);

/// The held probe while it is younger than `ttl`, else a fresh one. Clock- and
/// reader-injectable so the window is a unit under test rather than something a
/// test has to sleep out.
fn probed_hostname_at(
    now: std::time::Instant,
    ttl: std::time::Duration,
    probe: impl FnOnce() -> Option<String>,
) -> Option<String> {
    probed_at(&HOSTNAME_PROBE, now, ttl, probe)
}

/// A probe result held in `slot` while it is younger than `ttl`, else a fresh
/// one.
fn probed_at(
    slot: &std::sync::Mutex<Option<(std::time::Instant, Option<String>)>>,
    now: std::time::Instant,
    ttl: std::time::Duration,
    probe: impl FnOnce() -> Option<String>,
) -> Option<String> {
    // A poisoned lock means an earlier holder panicked mid-update. Fall through
    // to a fresh probe rather than propagate the panic: the pairing routes
    // behind this are required to answer.
    if let Ok(held) = slot.lock() {
        if let Some((at, value)) = held.as_ref() {
            if now.duration_since(*at) < ttl {
                return value.clone();
            }
        }
    }
    // Deliberately outside the lock: the probe spawns a process, and holding
    // the mutex across it would queue every concurrent pairing request behind
    // one spawn.
    let fresh = probe();
    if let Ok(mut held) = slot.lock() {
        *held = Some((now, fresh.clone()));
    }
    fresh
}

/// `hostname(1)`, normalized. Split out so [`probed_hostname_at`] is the only
/// thing guarding the spawn.
fn read_hostname_command() -> Option<String> {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())?;
    normalize_hostname(&raw)
}

/// The resolvable `.local` reach name for this host, or `None` when the host
/// has no hostname that could be another machine's reach.
///
/// avahi's own published name wins, so every surface names the host avahi
/// answers for even after a collision rename. `None` is a real answer: a node
/// whose hostname is `localhost` has no mDNS reach, and a caller must emit an
/// empty/absent value rather than a name it cannot prove.
pub fn mdns_hostname() -> Option<String> {
    let published = probed_at(
        &AVAHI_PROBE,
        std::time::Instant::now(),
        HOSTNAME_PROBE_TTL,
        read_avahi_host_fqdn,
    );
    published.or_else(|| system_hostname().map(|h| mdns_name_from(&h)))
}

/// The last avahi host-name probe.
static AVAHI_PROBE: std::sync::Mutex<Option<(std::time::Instant, Option<String>)>> =
    std::sync::Mutex::new(None);

/// avahi's published host name, read over D-Bus. `None` when avahi is not
/// running, D-Bus is unavailable, or the answer is not a usable `.local` name.
fn read_avahi_host_fqdn() -> Option<String> {
    let out = std::process::Command::new("busctl")
        .args([
            "--system",
            "--timeout=1",
            "call",
            "org.freedesktop.Avahi",
            "/",
            "org.freedesktop.Avahi.Server",
            "GetHostNameFqdn",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_busctl_string(&String::from_utf8(out.stdout).ok()?)
}

/// The name in a `busctl call` string reply (`s "skynode-2.local"`), kept only
/// when it is a usable `.local` reach.
fn parse_busctl_string(reply: &str) -> Option<String> {
    let quoted = reply.trim().strip_prefix("s ")?.trim();
    let name = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let name = normalize_hostname(name)?;
    name.to_ascii_lowercase()
        .ends_with(".local")
        .then_some(name)
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

/// The mDNS name for an already-normalized hostname: its first label under
/// `.local`. An mDNS responder publishes only that, so a host named
/// `skynode.lan` answers as `skynode.local`, and `skynode.lan` (a unicast DNS
/// name) is not something multicast DNS resolves. Split out so the rule is
/// unit-testable without a host read.
pub fn mdns_name_from(hostname: &str) -> String {
    let label = hostname.split('.').next().unwrap_or(hostname);
    format!("{label}.local")
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
    fn a_dotted_hostname_is_named_by_its_first_label_as_mdns_publishes_it() {
        // An mDNS responder publishes `<first label>.local`; a unicast DNS name
        // is not something a multicast browser resolves.
        assert_eq!(mdns_name_from("skynode.lan"), "skynode.local");
        assert_eq!(mdns_name_from("skynode.local"), "skynode.local");
    }

    #[test]
    fn avahis_published_name_is_read_from_the_busctl_reply() {
        // A collision rename is exactly what this read exists to see.
        assert_eq!(
            parse_busctl_string("s \"skynode-2.local\"\n").as_deref(),
            Some("skynode-2.local")
        );
        for bad in [
            "",
            "s \"\"",
            "s \"localhost\"",
            "s \"skynode.lan\"",
            "u 5",
            "skynode.local",
        ] {
            assert!(parse_busctl_string(bad).is_none(), "{bad:?} is not a reach");
        }
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

    #[test]
    fn the_hostname_probe_expires_so_a_renamed_or_newly_named_host_is_advertised() {
        // The probe is cached to keep a process spawn off a per-request path,
        // but what is cached must expire: this name BUILDS an advertised reach,
        // and a node whose hostname was still `localhost` when the front
        // started must stop advertising nothing once the operator sets one.
        let t0 = std::time::Instant::now();
        let ttl = std::time::Duration::from_secs(30);

        assert_eq!(
            probed_hostname_at(t0, ttl, || Some("skynode".to_string())).as_deref(),
            Some("skynode")
        );
        // Inside the window a request costs no spawn: the probe below would
        // have answered differently, and does not run.
        assert_eq!(
            probed_hostname_at(t0 + std::time::Duration::from_secs(1), ttl, || panic!(
                "the window was not honoured"
            ))
            .as_deref(),
            Some("skynode")
        );
        // Past the window a rename reaches the advert.
        assert_eq!(
            probed_hostname_at(t0 + ttl, ttl, || Some("skynode-2".to_string())).as_deref(),
            Some("skynode-2")
        );
        // And a `None` verdict is not sticky either.
        assert!(probed_hostname_at(t0 + ttl * 2, ttl, || None).is_none());
        assert_eq!(
            probed_hostname_at(t0 + ttl * 3, ttl, || Some("skynode-3".to_string())).as_deref(),
            Some("skynode-3")
        );
    }
}
