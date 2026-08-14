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
//! These primitives are protocol-level on purpose: more than one native surface
//! enforces the same posture (the HTTP control surface and the direct MAVLink
//! WebSocket proxy), and a single implementation keeps the two from drifting.
//! Surface-specific concerns (request-rate limiting, the HTTP public-path
//! exempt set, the short-TTL caching wrapper) live with each surface, not here.
//!
//! The pairing state is the agent's `pairing.json` (`{ "paired": bool,
//! "api_key": "..." }`). Read it with [`load_pairing`]; an absent, unreadable,
//! or not-`paired:true`-with-a-key file reads as [`Pairing::Unpaired`] (open),
//! matching the agent's "no key on file means open" stance.

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
/// transport. This is the single posture rule the native surfaces share:
///
/// - **Unpaired ⇒ Accept.** A fresh agent has no key; LAN presence is the gate.
/// - **Paired + on-box ⇒ Accept.** The local operator already holds shell-level
///   privilege that exceeds API auth.
/// - **Paired + off-box + a valid key ⇒ Accept.** Compared in constant time.
/// - **Paired + off-box + a missing or wrong key ⇒ Unauthorized.**
///
/// `on_box` is the resolved [`is_on_box`] result for this peer (loopback and not
/// relayed). `presented_key` is the key the caller supplied (e.g. an
/// `X-ADOS-Key` header), if any.
pub fn data_plane_access(pairing: &Pairing, on_box: bool, presented_key: Option<&str>) -> Access {
    match pairing {
        Pairing::Unpaired => Access::Accept,
        Pairing::Paired(expected) => {
            if on_box {
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

/// True when the request originates on this host's loopback interface and was not
/// relayed by a proxy or tunnel. An on-box caller (the local `ados` CLI, a root-owned
/// job) already holds shell-level privilege that strictly exceeds API auth, so it is
/// trusted past the pairing gate. A proxy or tunnel that terminates on loopback is
/// excluded by the forwarding-header check.
///
/// `peer_is_loopback` is whether the connection peer is `127.0.0.1`/`::1`;
/// `has_forwarding_header` is whether any of [`FORWARDED_HEADERS`] is present on
/// the request.
pub fn is_on_box(peer_is_loopback: bool, has_forwarding_header: bool) -> bool {
    peer_is_loopback && !has_forwarding_header
}

/// Whether an UNPAIRED node should answer a request from this peer.
///
/// An unpaired node accepts every route from anyone, including flight control,
/// for as long as it stays unpaired. That is defensible on a bench, where
/// physical presence on the LAN is the gate. It is not defensible on a unit a
/// customer powers on in an office or a hotel and does not pair immediately.
///
/// The obvious remedy — bind only loopback and link-local until paired — cannot
/// be used here, and the reason is worth stating so nobody reaches for it again.
/// A headless node has exactly two operator lifelines and NEITHER is loopback or
/// link-local: the AP hotspot on `192.168.4.1` (the primary first-boot route)
/// and the USB gadget on `192.168.7.1`. Binding them away would leave a fresh
/// unit reachable only from a shell the customer does not have, which is a
/// worse failure than the exposure it closes. There is also no runtime re-bind:
/// listeners are bound once at startup, so a bind keyed on pairing would need a
/// service restart at the exact moment the operator is mid-claim on that socket.
///
/// So the gate is drawn here, at the peer address, where the decision is
/// re-evaluated per request and follows pairing state in both directions with no
/// restart. The honest limitation is that this is request-layer defence: the
/// port stays open and an unauthorised peer receives a refusal rather than
/// finding nothing listening.
///
/// Returns true for loopback, IPv4/IPv6 link-local, and the two agent-owned
/// provisioning subnets. Everything else is refused while unpaired.
///
/// This is the FIRST-BOOT LIFELINE set, deliberately left unchanged by the
/// private-LAN operator scope: it is also consulted by the direct MAVLink WebSocket
/// proxy (an off-`ados-control` surface that re-uses this crate), whose unpaired
/// posture must stay restricted to the lifelines. A private-LAN browser is instead
/// trusted for the HTTP operator-UI scope via [`trusted_operator_lan_peer`] and gated
/// there behind the dashboard PIN — see that predicate for why the two sets are
/// siblings rather than folded into one.
pub fn unpaired_peer_allowed(peer: &std::net::IpAddr) -> bool {
    match peer {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local()
                // The AP hotspot the operator joins on first boot.
                || v4.octets()[..3] == [192, 168, 4]
                // The USB gadget network, the headless fallback.
                || v4.octets()[..3] == [192, 168, 7]
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                // fe80::/10 — link-local unicast.
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // An IPv4 lifeline arriving mapped onto v6.
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|m| unpaired_peer_allowed(&std::net::IpAddr::V4(m)))
        }
    }
}

/// True when an IPv4 octet array is in an RFC1918 private range: `10.0.0.0/8`,
/// `172.16.0.0/12`, or `192.168.0.0/16`. Link-local (`169.254/16`) and loopback
/// are NOT RFC1918 — the former is handled by the first-boot lifeline set, the
/// latter is on-box.
fn is_rfc1918_v4(o: [u8; 4]) -> bool {
    match o {
        [10, _, _, _] => true,
        [172, b, _, _] => (16..=31).contains(&b),
        [192, 168, _, _] => true,
        _ => false,
    }
}

/// Whether a peer is a trusted operator-LAN peer — it sits on an RFC1918 private
/// LAN (`10/8`, `172.16/12`, `192.168/16`), i.e. plausibly the operator's own
/// browser on the local network rather than a random public-WAN host.
///
/// These are the peers the founder's PIN-gated cockpit design trusts for the
/// operator-UI DATA scope while the node is unpaired: they may reach status/video/
/// command through the HTTP control surface, but (unlike the first-boot lifelines)
/// only when they present a dashboard PIN session, which the `ados-control` gate
/// enforces. Public-WAN addresses never match this.
///
/// Deliberately a SIBLING of, not folded into, [`unpaired_peer_allowed`]: the
/// first-boot lifelines keep their unrestricted unpaired access on EVERY surface,
/// including the direct MAVLink WebSocket proxy that also consults this module. If
/// the private-LAN set were folded into `unpaired_peer_allowed`, that proxy would
/// blindly open flight-control bytes to the whole LAN while unpaired — exactly the
/// blind trusted-network-open the founder rejected — and the PIN gate would be
/// bypassable off the HTTP surface. So the HTTP gate layers this predicate on top
/// of the lifeline set and applies the PIN requirement itself.
pub fn trusted_operator_lan_peer(peer: &std::net::IpAddr) -> bool {
    match peer {
        std::net::IpAddr::V4(v4) => is_rfc1918_v4(v4.octets()),
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .is_some_and(|m| is_rfc1918_v4(m.octets())),
    }
}

/// Load the pairing posture from a `pairing.json`. An absent file, an
/// unreadable file, or a state that is not `paired:true` with a non-empty
/// `api_key` is treated as unpaired (open), matching the agent: when not paired,
/// access is open.
pub fn load_pairing(path: &Path) -> Pairing {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Pairing::Unpaired;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Pairing::Unpaired;
    };
    let paired = value
        .get("paired")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let key = value.get("api_key").and_then(|v| v.as_str());
    match (paired, key) {
        (true, Some(k)) if !k.is_empty() => Pairing::Paired(k.to_string()),
        _ => Pairing::Unpaired,
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    /// The two lifelines a headless unpaired unit is actually reached from.
    /// These are the reason this is a peer filter and not a narrowed bind: a
    /// literal loopback+link-local bind would remove both and leave a fresh
    /// device unreachable to its own operator.
    #[test]
    fn the_first_boot_lifelines_are_allowed_while_unpaired() {
        for ip in [
            "127.0.0.1",    // on-box
            "::1",          // on-box, v6
            "192.168.4.1",  // the AP hotspot itself
            "192.168.4.37", // a phone joined to the hotspot
            "192.168.7.1",  // the USB gadget
            "192.168.7.42", // a laptop on the USB gadget net
            "169.254.11.9", // IPv4 link-local
            "fe80::1",      // IPv6 link-local
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                unpaired_peer_allowed(&addr),
                "{ip} is a first-boot reach path and must not be refused"
            );
        }
    }

    /// The exposure being closed: an ordinary LAN peer must not command an
    /// unpaired aircraft.
    #[test]
    fn an_ordinary_lan_peer_is_refused_while_unpaired() {
        for ip in [
            "192.168.200.50", // the office LAN this is typically on
            "192.168.1.10",
            "10.0.0.5",
            "172.16.4.4",
            "8.8.8.8",
            "2001:db8::1",
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                !unpaired_peer_allowed(&addr),
                "{ip} must not reach a non-public route on an unpaired device"
            );
        }
    }

    /// The founder's ask: a browser on a private trusted LAN is a trusted
    /// operator-LAN peer for the PIN-gated operator-UI scope. The predicate is
    /// the RFC1918 signal, kept distinct from the first-boot lifeline set so the
    /// MAVLink surface stays lifeline-only while the HTTP gate layers the PIN on
    /// top of this.
    #[test]
    fn a_private_lan_peer_is_a_trusted_operator_peer() {
        for ip in [
            "192.168.1.10",
            "192.168.200.50", // the office LAN the founder hit
            "10.0.0.5",
            "10.255.255.1",
            "172.16.4.4",
            "172.31.255.1",
            // The provisioning subnets are RFC1918 too, so they are trusted
            // operator-LAN peers as well (the HTTP gate prioritises their
            // lifeline status and serves them without a PIN).
            "192.168.4.37",
            "192.168.7.42",
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                trusted_operator_lan_peer(&addr),
                "{ip} is on a private LAN and must be a trusted operator peer"
            );
        }
        // An IPv4 private address arriving mapped onto v6 is trusted too.
        let mapped: IpAddr = "::ffff:192.168.1.50".parse().unwrap();
        assert!(trusted_operator_lan_peer(&mapped));
    }

    /// Public-WAN hosts and non-RFC1918 addresses must never be trusted as
    /// operator-LAN peers — the PIN-gated scope must not open to the internet.
    #[test]
    fn a_public_wan_peer_is_not_a_trusted_operator_peer() {
        for ip in [
            "8.8.8.8",
            "203.0.113.5",  // documentation range
            "172.15.255.1", // just below 172.16/12
            "172.32.0.1",   // just above
            "169.254.1.1",  // link-local, not RFC1918
            "127.0.0.1",    // loopback, not RFC1918
            "2001:db8::1",
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                !trusted_operator_lan_peer(&addr),
                "{ip} is not a private-LAN peer and must not be trusted"
            );
        }
    }

    /// A neighbouring subnet must not be admitted by a sloppy prefix match.
    #[test]
    fn adjacent_subnets_are_not_mistaken_for_the_lifelines() {
        for ip in ["192.168.40.1", "192.168.70.1", "192.168.5.1", "192.168.6.1"] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(!unpaired_peer_allowed(&addr), "{ip} must not be allowed");
        }
    }

    /// A lifeline arriving mapped onto v6 is the same lifeline. The listener is
    /// dual-stack, so this is a real shape, not a hypothetical.
    #[test]
    fn an_ipv4_lifeline_mapped_onto_v6_is_still_allowed() {
        let mapped: IpAddr = "::ffff:192.168.4.20".parse().unwrap();
        assert!(unpaired_peer_allowed(&mapped));
        let mapped_lan: IpAddr = "::ffff:192.168.200.50".parse().unwrap();
        assert!(!unpaired_peer_allowed(&mapped_lan));
    }

    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

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
    fn on_box_trust_is_loopback_and_no_forwarding_header() {
        assert!(is_on_box(true, false));
        assert!(!is_on_box(true, true));
        assert!(!is_on_box(false, false));
        assert!(!is_on_box(false, true));
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
    fn paired_without_a_key_reads_as_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": ""}"#);
        assert_eq!(load_pairing(&path), Pairing::Unpaired);
    }

    #[test]
    fn malformed_file_reads_as_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), "this is not json");
        assert_eq!(load_pairing(&path), Pairing::Unpaired);
    }

    #[test]
    fn unpaired_accepts_any_caller() {
        assert_eq!(
            data_plane_access(&Pairing::Unpaired, false, None),
            Access::Accept
        );
        assert_eq!(
            data_plane_access(&Pairing::Unpaired, false, Some("anything")),
            Access::Accept
        );
    }

    #[test]
    fn paired_on_box_accepts_without_a_key() {
        let p = Pairing::Paired("k".into());
        assert_eq!(data_plane_access(&p, true, None), Access::Accept);
    }

    #[test]
    fn paired_off_box_with_a_valid_key_accepts() {
        let p = Pairing::Paired("ados_secret".into());
        assert_eq!(
            data_plane_access(&p, false, Some("ados_secret")),
            Access::Accept
        );
    }

    #[test]
    fn paired_off_box_with_no_key_is_unauthorized() {
        let p = Pairing::Paired("ados_secret".into());
        assert_eq!(data_plane_access(&p, false, None), Access::Unauthorized);
    }

    #[test]
    fn paired_off_box_with_a_wrong_key_is_unauthorized() {
        let p = Pairing::Paired("ados_secret".into());
        assert_eq!(
            data_plane_access(&p, false, Some("wrong")),
            Access::Unauthorized
        );
    }
}
