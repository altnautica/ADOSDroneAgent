//! Pairing posture: the data-plane auth primitives shared by the agent's
//! native surfaces.
//!
//! The agent is paired-or-unpaired. Physical presence on the LAN is the gate
//! for an UNPAIRED agent (the claim window); once paired, a data-plane caller
//! reaching the agent from off-box must present the stored pairing key. The
//! local operator (a loopback peer that was not relayed by a proxy or tunnel)
//! already holds shell-level privilege that exceeds API auth and is trusted past
//! the gate.
//!
//! Who the caller is gets decided ONCE per request, at the transport edge, by
//! [`classify_caller`]: the peer address plus the request's forwarding headers
//! give one [`CallerClass`]. Every gate (the HTTP unpaired-node gate, the
//! dashboard-PIN first set, the pairing claim, the direct MAVLink proxies) is a
//! function of that value and the pairing state, so no two gates can read the
//! same caller differently.
//!
//! These primitives are protocol-level on purpose: more than one native surface
//! enforces the same posture (the HTTP control surface and the direct MAVLink
//! proxies), and a single implementation keeps them from drifting.
//! Surface-specific concerns (request-rate limiting, the HTTP public-path
//! exempt set, the short-TTL caching wrapper) live with each surface, not here.
//!
//! The pairing state is the agent's `pairing.json` (`{ "paired": bool,
//! "api_key": "..." }`). Read it with [`load_pairing`]: an absent file, or one
//! that is not `paired:true`, reads as [`Pairing::Unpaired`] (open). A file that
//! exists but cannot be read or parsed, or claims `paired:true` with no key, is
//! [`Pairing::Unreadable`] and fails closed: it may be a paired node's record on
//! a failing card, and reading it as unpaired would open the claim to anyone.

use std::net::IpAddr;
use std::path::Path;

/// Proxy / tunnel relay headers. Their presence means the request was forwarded by a
/// reverse proxy or tunnel (e.g. a Cloudflare Tunnel terminating on 127.0.0.1) rather
/// than originating on this host, so it must NOT qualify for on-box loopback trust.
pub const FORWARDED_HEADERS: [&str; 4] = [
    "x-forwarded-for",
    "x-real-ip",
    "forwarded",
    "cf-connecting-ip",
];

/// The resolved pairing posture read from `pairing.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pairing {
    /// No pairing on file: the data plane is open (LAN presence is the gate).
    Unpaired,
    /// Paired with this exact key required from an off-box caller.
    Paired(String),
    /// The file exists but could not be read or parsed. Treated as paired with
    /// a key nobody can present: only the on-box operator is served.
    Unreadable,
}

/// A data-plane access decision for a paired-or-unpaired agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Admit the connection (unpaired, or on-box, or a valid key).
    Accept,
    /// Reject: the agent is paired, the caller is off-box, and it presented no
    /// key or the wrong key.
    Unauthorized,
}

/// Decide whether a data-plane connection may be admitted, independent of any
/// transport:
///
/// - **Unpaired ⇒ Accept.** A fresh agent has no key to check against. Each
///   surface narrows the unpaired posture by [`CallerClass`] itself before it
///   gets here (the HTTP edge PIN-gates the operator LAN and refuses remote
///   callers; the direct MAVLink proxies admit only
///   [`CallerClass::is_first_boot_reach`]).
/// - **Paired + [`CallerClass::OnBox`] ⇒ Accept.** The local operator already
///   holds shell-level privilege that exceeds API auth.
/// - **Paired + any other caller + a valid key ⇒ Accept.** Compared in
///   constant time.
/// - **Paired + any other caller + a missing or wrong key ⇒ Unauthorized.**
///
/// `presented_key` is the key the caller supplied (e.g. an `X-ADOS-Key`
/// header), if any.
pub fn data_plane_access(
    pairing: &Pairing,
    caller: CallerClass,
    presented_key: Option<&str>,
) -> Access {
    match pairing {
        Pairing::Unpaired => Access::Accept,
        Pairing::Unreadable if caller == CallerClass::OnBox => Access::Accept,
        Pairing::Unreadable => Access::Unauthorized,
        Pairing::Paired(expected) => {
            if caller == CallerClass::OnBox {
                return Access::Accept;
            }
            match presented_key {
                Some(presented) if constant_time_eq(presented.as_bytes(), expected.as_bytes()) => {
                    Access::Accept
                }
                _ => Access::Unauthorized,
            }
        }
    }
}

/// Compare two byte slices in time independent of where they first differ, so
/// the bearer-secret check leaks no timing signal about a partial match. A
/// length mismatch is rejected up front (the length of the stored key is not
/// itself a secret); equal-length slices are then folded together with a running
/// difference accumulator that always visits every byte. The compiler is told
/// via `std::hint::black_box` not to short-circuit the loop once a difference is
/// seen.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Who is on the other end of a request, derived once at the transport edge by
/// [`classify_caller`].
///
/// An unpaired node is a unit someone powered on and has not claimed yet, in a
/// hangar, an office or a hotel. The obvious remedy for its exposure — bind only
/// loopback and link-local until paired — cannot be used, and the reason is
/// worth stating so nobody reaches for it again. A headless node has exactly two
/// operator lifelines and NEITHER is loopback or link-local: the AP hotspot on
/// `192.168.4.1` (the primary first-boot route) and the USB gadget on
/// `192.168.7.1`. Binding them away would leave a fresh unit reachable only from
/// a shell the customer does not have. There is also no runtime re-bind:
/// listeners are bound once at startup, so a bind keyed on pairing would need a
/// service restart at the exact moment the operator is mid-claim on that socket.
///
/// So the gate is drawn at the caller, per request, where it follows pairing
/// state in both directions with no restart. The honest limitation is that this
/// is request-layer defence: the port stays open and a refused caller receives a
/// refusal rather than finding nothing listening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerClass {
    /// A loopback peer whose request carries none of [`FORWARDED_HEADERS`]: the
    /// local operator (the `ados` CLI, a root-owned job), who already holds
    /// shell-level privilege that exceeds API auth.
    OnBox,
    /// A first-boot lifeline: IPv4/IPv6 link-local, or a peer reached through
    /// the agent's own AP hotspot (`192.168.4.1`) or USB gadget (`192.168.7.1`)
    /// address. These are the surfaces a fresh unit is reached from and its PIN
    /// is first set on. The subnet alone is not enough: a home or office LAN can
    /// be numbered `192.168.4.0/24` too, and every host on it would otherwise be
    /// a lifeline.
    Lifeline,
    /// Any other RFC1918 private-LAN peer (`10/8`, `172.16/12`, `192.168/16`):
    /// plausibly the operator's own browser on the local network rather than a
    /// public-WAN host. Trusted less than a lifeline: while unpaired the HTTP
    /// surface serves it data only behind a dashboard PIN session, and the
    /// direct MAVLink proxies do not serve it at all.
    OperatorLan,
    /// Everything else: a public-WAN host, ANY request that carries a
    /// proxy-forwarding header (a reverse proxy or tunnel, including one that
    /// terminates on this host's loopback), or a caller whose address is unknown.
    Remote,
}

impl CallerClass {
    /// Whether this caller may reach an UNPAIRED node's data plane with no
    /// credential at all: the local operator and the first-boot lifelines.
    pub fn is_first_boot_reach(self) -> bool {
        matches!(self, CallerClass::OnBox | CallerClass::Lifeline)
    }
}

/// The agent-owned first-boot addresses: the AP hotspot gateway and the USB
/// gadget address. A private-LAN peer is a lifeline only when it reached one of
/// these.
pub const LIFELINE_LOCAL_ADDRS: [std::net::Ipv4Addr; 2] = [
    std::net::Ipv4Addr::new(192, 168, 4, 1),
    std::net::Ipv4Addr::new(192, 168, 7, 1),
];

/// Classify a caller from its peer address, the local address it reached, and
/// its request headers.
///
/// `has_header` answers whether the request carries a header by (lowercase)
/// name; a transport with no headers (a raw TCP or UDP socket) passes
/// `|_| false`. A request carrying any of [`FORWARDED_HEADERS`] was relayed by a
/// proxy or tunnel, so its socket address says nothing about where it came
/// from: it is [`CallerClass::Remote`] whatever the peer is. That is what keeps
/// a tunnel terminating on `127.0.0.1` from reading as the local operator, a
/// first-boot lifeline, or a PIN claimant.
///
/// `local` is the address the connection arrived on (the accepted socket's
/// local address; see [`local_addr_toward`] for a transport that has none).
/// Lifeline trust for a private-LAN peer requires it to be one of
/// [`LIFELINE_LOCAL_ADDRS`], so a LAN that merely shares the AP's numbering is
/// the operator LAN, not a lifeline. `None` grants no such trust.
///
/// `peer` is `None` when the address could not be determined, which is
/// [`CallerClass::Remote`]: an unidentifiable caller is exactly the one these
/// gates exist for. An IPv4 address mapped onto IPv6 is classified as the IPv4
/// address it carries.
pub fn classify_caller(
    peer: Option<IpAddr>,
    local: Option<IpAddr>,
    has_header: impl Fn(&str) -> bool,
) -> CallerClass {
    let Some(peer) = peer else {
        return CallerClass::Remote;
    };
    if FORWARDED_HEADERS.iter().any(|h| has_header(h)) {
        return CallerClass::Remote;
    }
    let on_lifeline_addr = matches!(
        local.map(|l| l.to_canonical()),
        Some(IpAddr::V4(l)) if LIFELINE_LOCAL_ADDRS.contains(&l)
    );
    match peer.to_canonical() {
        ip if ip.is_loopback() => CallerClass::OnBox,
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_link_local() || (on_lifeline_addr && is_rfc1918_v4(o)) {
                CallerClass::Lifeline
            } else if is_rfc1918_v4(o) {
                CallerClass::OperatorLan
            } else {
                CallerClass::Remote
            }
        }
        // fe80::/10 — IPv6 link-local unicast.
        IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => CallerClass::Lifeline,
        IpAddr::V6(_) => CallerClass::Remote,
    }
}

/// The local address this host would use to reach `peer`: the kernel's route
/// choice, read by connecting an unbound UDP socket (no packet is sent). For a
/// transport that has no per-connection local address (a UDP listener bound to
/// the wildcard), this is the address a reply to the peer leaves from, which is
/// the interface the peer sits on. `None` when no route exists.
pub fn local_addr_toward(peer: IpAddr) -> Option<IpAddr> {
    let peer = peer.to_canonical();
    let bind: std::net::SocketAddr = match peer {
        IpAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        IpAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = std::net::UdpSocket::bind(bind).ok()?;
    socket.connect((peer, 9)).ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// True when an IPv4 octet array is in an RFC1918 private range: `10.0.0.0/8`,
/// `172.16.0.0/12`, or `192.168.0.0/16`.
fn is_rfc1918_v4(o: [u8; 4]) -> bool {
    match o {
        [10, _, _, _] => true,
        [172, b, _, _] => (16..=31).contains(&b),
        [192, 168, _, _] => true,
        _ => false,
    }
}

/// Load the pairing posture from a `pairing.json`. An absent file, or a state
/// that is not `paired:true`, is [`Pairing::Unpaired`] (open). A file that
/// exists but cannot be read or parsed as a JSON object, or that says
/// `paired:true` without a key, is [`Pairing::Unreadable`] (closed).
pub fn load_pairing(path: &Path) -> Pairing {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Pairing::Unpaired,
        Err(_) => return Pairing::Unreadable,
    };
    let Ok(serde_json::Value::Object(value)) = serde_json::from_str::<serde_json::Value>(&text)
    else {
        return Pairing::Unreadable;
    };
    let paired = value
        .get("paired")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let key = value.get("api_key").and_then(|v| v.as_str());
    match (paired, key) {
        (true, Some(k)) if !k.is_empty() => Pairing::Paired(k.to_string()),
        (true, _) => Pairing::Unreadable,
        _ => Pairing::Unpaired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// Classify a peer that reached this host on an ordinary LAN address.
    fn class(ip: &str) -> CallerClass {
        class_on(ip, Some("192.168.1.20"))
    }

    fn class_on(ip: &str, local: Option<&str>) -> CallerClass {
        classify_caller(
            Some(ip.parse().unwrap()),
            local.map(|l| l.parse().unwrap()),
            |_| false,
        )
    }

    /// The local operator: a loopback peer with no relay header.
    #[test]
    fn a_loopback_peer_with_no_forwarding_header_is_on_box() {
        for ip in ["127.0.0.1", "127.0.0.53", "::1", "::ffff:127.0.0.1"] {
            assert_eq!(class(ip), CallerClass::OnBox, "{ip}");
        }
    }

    /// A tunnel terminating on this host (a tunnel's ingress to localhost)
    /// delivers every internet request from 127.0.0.1 with one of these headers
    /// set. Reading that caller as on-box, as a lifeline or as a PIN claimant is
    /// the exposure this classification exists to close.
    #[test]
    fn a_loopback_peer_carrying_any_forwarding_header_is_remote() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        for header in FORWARDED_HEADERS {
            assert_eq!(
                classify_caller(Some(lo), Some(lo), |h| h == header),
                CallerClass::Remote,
                "loopback + {header} is a relayed caller"
            );
        }
    }

    /// A reverse proxy elsewhere on the LAN is no more trustworthy than one on
    /// this host: once a request was forwarded, its socket address no longer
    /// says where it came from.
    #[test]
    fn a_forwarded_request_is_remote_from_any_peer() {
        for ip in ["192.168.4.20", "192.168.1.50", "169.254.3.4", "fe80::1"] {
            let peer: IpAddr = ip.parse().unwrap();
            assert_eq!(
                classify_caller(Some(peer), Some("192.168.4.1".parse().unwrap()), |h| {
                    h == "x-forwarded-for"
                }),
                CallerClass::Remote,
                "{ip} + x-forwarded-for"
            );
        }
    }

    #[test]
    fn an_unknown_peer_is_remote() {
        assert_eq!(classify_caller(None, None, |_| false), CallerClass::Remote);
    }

    /// The lifelines a headless unpaired unit is actually reached from. These
    /// are the reason this is a caller filter and not a narrowed bind: a literal
    /// loopback+link-local bind would remove the AP and USB paths and leave a
    /// fresh device unreachable to its own operator.
    #[test]
    fn the_first_boot_lifelines_classify_as_lifelines() {
        for (ip, local) in [
            // A phone joined to the hotspot, reaching the AP address.
            ("192.168.4.37", Some("192.168.4.1")),
            // A laptop on the USB gadget net.
            ("192.168.7.42", Some("192.168.7.1")),
            // Link-local needs no particular local address.
            ("169.254.11.9", None),
            ("fe80::1", None),
            // An IPv4 lifeline mapped onto v6, on a v6 listener.
            ("::ffff:192.168.4.20", Some("::ffff:192.168.4.1")),
        ] {
            assert_eq!(class_on(ip, local), CallerClass::Lifeline, "{ip}");
        }
    }

    /// A home or office LAN numbered like the AP subnet is the operator LAN,
    /// not a lifeline: the drone is a client there, so the connection arrives
    /// on its DHCP lease, never on the AP gateway address.
    #[test]
    fn a_lan_that_shares_the_ap_numbering_is_not_a_lifeline() {
        for (ip, local) in [
            ("192.168.4.37", Some("192.168.4.12")),
            ("192.168.7.42", Some("192.168.7.9")),
            ("192.168.4.37", None),
        ] {
            assert_eq!(
                class_on(ip, local),
                CallerClass::OperatorLan,
                "{ip} via {local:?}"
            );
        }
    }

    /// The route-derived local address names the interface a peer sits on.
    #[test]
    fn local_addr_toward_loopback_is_loopback() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(local_addr_toward(lo), Some(lo));
    }

    /// A browser on a private LAN is an operator-LAN caller, distinct from the
    /// lifelines, so the MAVLink proxies can stay lifeline-only while the HTTP
    /// gate layers the PIN on top of it.
    #[test]
    fn a_private_lan_peer_is_an_operator_lan_caller() {
        for ip in [
            "192.168.1.10",
            "192.168.1.50",
            "10.0.0.5",
            "10.255.255.1",
            "172.16.4.4",
            "172.31.255.1",
            "::ffff:192.168.1.50",
            // Neighbouring subnets must not be taken for the lifelines by a
            // sloppy prefix match.
            "192.168.40.1",
            "192.168.70.1",
            "192.168.5.1",
            "192.168.6.1",
        ] {
            assert_eq!(class(ip), CallerClass::OperatorLan, "{ip}");
        }
    }

    /// Public-WAN and non-RFC1918 addresses are never operator-LAN callers.
    #[test]
    fn a_public_wan_peer_is_remote() {
        for ip in [
            "8.8.8.8",
            "203.0.113.5",  // documentation range
            "172.15.255.1", // just below 172.16/12
            "172.32.0.1",   // just above
            "2001:db8::1",
        ] {
            assert_eq!(class(ip), CallerClass::Remote, "{ip}");
        }
    }

    #[test]
    fn only_on_box_and_lifelines_reach_an_unpaired_node_without_a_credential() {
        assert!(CallerClass::OnBox.is_first_boot_reach());
        assert!(CallerClass::Lifeline.is_first_boot_reach());
        assert!(!CallerClass::OperatorLan.is_first_boot_reach());
        assert!(!CallerClass::Remote.is_first_boot_reach());
    }

    fn write_pairing(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("pairing.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn constant_time_eq_matches_byte_equality() {
        assert!(constant_time_eq(b"ados_secret", b"ados_secret"));
        assert!(!constant_time_eq(b"ados_secret", b"ados_secre1"));
        assert!(!constant_time_eq(b"ados_secret", b"xdos_secret"));
        assert!(!constant_time_eq(b"ados_secret", b"ados_secret_longer"));
        assert!(!constant_time_eq(b"ados_secret", b"short"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn absent_file_reads_as_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_pairing(&dir.path().join("absent.json")),
            Pairing::Unpaired
        );
    }

    #[test]
    fn paired_with_a_key_reads_as_paired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "ados_secret"}"#);
        assert_eq!(load_pairing(&path), Pairing::Paired("ados_secret".into()));
    }

    #[test]
    fn paired_without_a_key_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": ""}"#);
        assert_eq!(load_pairing(&path), Pairing::Unreadable);
    }

    /// A file that exists but cannot be parsed is not "no pairing": it may be a
    /// paired node's record on a failing card, and unpaired opens the claim.
    #[test]
    fn a_malformed_file_is_unreadable_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        for body in [
            "this is not json",
            "[]",
            r#"{"paired": true, "api_key": "k""#,
        ] {
            let path = write_pairing(dir.path(), body);
            assert_eq!(load_pairing(&path), Pairing::Unreadable, "{body}");
        }
        for caller in [
            CallerClass::Lifeline,
            CallerClass::OperatorLan,
            CallerClass::Remote,
        ] {
            assert_eq!(
                data_plane_access(&Pairing::Unreadable, caller, Some("anything")),
                Access::Unauthorized
            );
        }
        assert_eq!(
            data_plane_access(&Pairing::Unreadable, CallerClass::OnBox, None),
            Access::Accept
        );
    }

    #[test]
    fn a_file_without_paired_true_is_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), "{}");
        assert_eq!(load_pairing(&path), Pairing::Unpaired);
    }

    #[test]
    fn unpaired_accepts_any_caller_at_this_layer() {
        for caller in [CallerClass::Remote, CallerClass::OperatorLan] {
            assert_eq!(
                data_plane_access(&Pairing::Unpaired, caller, None),
                Access::Accept
            );
            assert_eq!(
                data_plane_access(&Pairing::Unpaired, caller, Some("anything")),
                Access::Accept
            );
        }
    }

    #[test]
    fn paired_on_box_accepts_without_a_key() {
        let p = Pairing::Paired("k".into());
        assert_eq!(
            data_plane_access(&p, CallerClass::OnBox, None),
            Access::Accept
        );
    }

    /// Only the local operator skips the key. A lifeline or LAN caller on a
    /// paired node is off-box like any other.
    #[test]
    fn paired_non_on_box_callers_need_the_key() {
        let p = Pairing::Paired("ados_secret".into());
        for caller in [
            CallerClass::Lifeline,
            CallerClass::OperatorLan,
            CallerClass::Remote,
        ] {
            assert_eq!(
                data_plane_access(&p, caller, Some("ados_secret")),
                Access::Accept,
                "{caller:?} with the key"
            );
            assert_eq!(
                data_plane_access(&p, caller, None),
                Access::Unauthorized,
                "{caller:?} with no key"
            );
            assert_eq!(
                data_plane_access(&p, caller, Some("wrong")),
                Access::Unauthorized,
                "{caller:?} with a wrong key"
            );
        }
    }
}
