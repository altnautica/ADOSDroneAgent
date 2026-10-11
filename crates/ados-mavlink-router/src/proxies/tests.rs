// --- raw-socket posture (TCP / UDP), which have no handshake ------------

#[test]
fn the_bind_default_stays_wide_for_the_lifelines() {
    // A fresh unit's AP and USB lifelines are not loopback, so the listener
    // binds wide and the per-connection gate decides who is served.
    assert_eq!(DEFAULT_PROXY_BIND_ADDR, "0.0.0.0");
}

/// Serialises the env mutation below; the house pattern from the uplink
/// consumer's tests.
static BIND_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn an_operator_can_narrow_the_bind_and_blank_values_do_not_count() {
    // A unit with no desktop ground station on its LAN can remove the
    // unauthenticated path entirely rather than inspect callers on a socket
    // that stays open. A blank override is a mistake, not a request to bind
    // nowhere, so it falls back rather than failing to bind at all.
    let _guard = BIND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let restore = std::env::var("ADOS_MAVLINK_BIND_ADDR").ok();

    std::env::set_var("ADOS_MAVLINK_BIND_ADDR", "127.0.0.1");
    assert_eq!(proxy_bind_addr(), "127.0.0.1");

    std::env::set_var("ADOS_MAVLINK_BIND_ADDR", "  10.0.0.5  ");
    assert_eq!(
        proxy_bind_addr(),
        "10.0.0.5",
        "surrounding space is trimmed"
    );

    for blank in ["", "   "] {
        std::env::set_var("ADOS_MAVLINK_BIND_ADDR", blank);
        assert_eq!(
            proxy_bind_addr(),
            DEFAULT_PROXY_BIND_ADDR,
            "a blank override must not bind nowhere"
        );
    }

    std::env::remove_var("ADOS_MAVLINK_BIND_ADDR");
    assert_eq!(proxy_bind_addr(), DEFAULT_PROXY_BIND_ADDR);

    // The WebSocket follows its configured endpoint host, and the operator
    // override narrows it along with the raw edges.
    assert_eq!(ws_bind_addr("127.0.0.1"), "127.0.0.1");
    assert_eq!(ws_bind_addr("  "), DEFAULT_PROXY_BIND_ADDR);
    std::env::set_var("ADOS_MAVLINK_BIND_ADDR", "127.0.0.1");
    assert_eq!(ws_bind_addr("0.0.0.0"), "127.0.0.1");

    match restore {
        Some(v) => std::env::set_var("ADOS_MAVLINK_BIND_ADDR", v),
        None => std::env::remove_var("ADOS_MAVLINK_BIND_ADDR"),
    }
}

#[test]
fn an_unpaired_node_still_answers_its_own_lifelines() {
    // A headless node's only two operator routes are the first-boot AP and
    // the USB gadget network, and NEITHER is loopback or link-local. Losing
    // them would leave a fresh unit with no way in at all.
    let (_dir, auth) = unpaired_auth(false);
    for ip in ["127.0.0.1", "192.168.4.20", "192.168.7.2", "169.254.3.4"] {
        let (admit, access) = auth.classify(ip.parse().unwrap(), lifeline_local(ip));
        assert_eq!(access, Access::Accept, "{ip} is a lifeline");
        assert!(admit);
    }
}

/// The local address a peer on the AP or USB subnet reaches: the agent's
/// own gateway address there. `None` for every other peer.
fn lifeline_local(ip: &str) -> Option<std::net::IpAddr> {
    if ip.starts_with("192.168.4.") {
        Some("192.168.4.1".parse().unwrap())
    } else if ip.starts_with("192.168.7.") {
        Some("192.168.7.1".parse().unwrap())
    } else {
        None
    }
}

/// A LAN numbered like the AP subnet, reached on the node's own DHCP lease
/// rather than the AP gateway, is not a lifeline.
#[test]
fn an_ap_numbered_lan_is_not_a_lifeline_off_the_ap_address() {
    let (_dir, auth) = unpaired_auth(true);
    let (admit, access) = auth.classify(
        "192.168.4.20".parse().unwrap(),
        Some("192.168.4.57".parse().unwrap()),
    );
    assert_eq!(access, Access::Unauthorized);
    assert!(!admit);
}

/// An unreadable pairing file admits only the local operator, even with the
/// raw edges opened to the LAN and enforcement off.
#[test]
fn an_unreadable_pairing_file_admits_only_the_local_operator() {
    let dir = tempfile::tempdir().unwrap();
    let pairing = write_pairing(dir.path(), "not json");
    let auth = ProxyAuth::new(false, pairing).with_raw_lan_access(true);
    assert!(!auth.classify("192.168.1.50".parse().unwrap(), None).0);
    assert!(
        !auth
            .classify(
                "192.168.4.20".parse().unwrap(),
                lifeline_local("192.168.4.20")
            )
            .0
    );
    assert!(auth.classify("127.0.0.1".parse().unwrap(), None).0);
}

#[test]
fn an_unpaired_node_treats_the_wider_lan_as_unauthorized() {
    // `data_plane_access` alone says Unpaired => Accept, i.e. an unpaired
    // node accepts flight-controller bytes from the whole LAN. The
    // allowlist is what narrows that to the routes an operator actually
    // has.
    let (_dir, auth) = unpaired_auth(false);
    for ip in ["10.0.0.9", "172.16.4.4", "192.168.1.50", "8.8.8.8"] {
        let (_admit, access) = auth.classify(ip.parse().unwrap(), None);
        assert_eq!(access, Access::Unauthorized, "{ip} is not a lifeline");
    }
}

#[test]
fn observation_admits_everything_it_flags() {
    // The whole point of the first stage: this port is advertised as the
    // third-party GCS path, so refusing a caller would break it at the
    // operator's screen. Flagging must not change the data path.
    let (_dir, auth) = unpaired_auth(false);
    let (admit, access) = auth.classify("10.0.0.9".parse().unwrap(), None);
    assert_eq!(access, Access::Unauthorized);
    assert!(admit, "observe-only must still admit");
}

#[test]
fn enforcing_refuses_the_same_peer_it_would_have_flagged() {
    // Proves the second stage is a flag flip rather than new logic, so the
    // observation gathered now describes exactly what enforcement will do.
    let (_dir, auth) = unpaired_auth(true);
    let (admit, access) = auth.classify("10.0.0.9".parse().unwrap(), None);
    assert_eq!(access, Access::Unauthorized);
    assert!(!admit);
    // A lifeline is still admitted under enforcement.
    assert!(
        auth.classify(
            "192.168.4.20".parse().unwrap(),
            lifeline_local("192.168.4.20")
        )
        .0
    );
}

#[test]
fn a_raw_socket_peer_can_present_no_key_and_no_ticket() {
    // MAVLink is a byte stream: there is no header and no subprotocol, so
    // address is the only signal. This pins that classify never claims
    // otherwise — a paired node cannot admit an off-box raw peer.
    let dir = tempfile::tempdir().unwrap();
    let pairing = dir.path().join("pairing.json");
    std::fs::write(&pairing, r#"{"paired":true,"api_key":"secret-key"}"#).unwrap();
    let auth = ProxyAuth::new(false, pairing);
    let (_admit, access) = auth.classify("10.0.0.9".parse().unwrap(), None);
    assert_eq!(
        access,
        Access::Unauthorized,
        "a paired node must not admit an off-box peer that presented nothing"
    );
    assert_eq!(
        auth.classify("127.0.0.1".parse().unwrap(), None).1,
        Access::Accept
    );
}

#[test]
fn the_loop_verdict_serves_a_flagged_peer_with_the_flag_off() {
    // `enforcing_refuses_the_same_peer_it_would_have_flagged` covers
    // `classify`. This covers what the accept loops actually consume: with
    // the flag off an unauthorized peer must still be served, and served
    // with the anonymous provenance rather than as a trusted local caller.
    let (_dir, auth) = unpaired_auth(false);
    let lan: std::net::IpAddr = "10.0.0.9".parse().unwrap();
    for edge in [RawEdge::Tcp, RawEdge::Udp] {
        assert_eq!(
            admit_raw_peer(&auth, lan, None, 5760, edge),
            Some(ClientOrigin::Unauthenticated),
            "{edge:?}: observe-only must serve, and record the anonymity"
        );
    }
}

#[test]
fn the_loop_verdict_refuses_a_flagged_peer_on_both_edges_with_the_flag_on() {
    // The refusal must reach BOTH loops. A flag honoured on one edge and
    // dropped on the other closes the door an operator can see and leaves
    // the one they cannot.
    //
    // On UDP the `None` is what skips the peer-table insert as well as the
    // injection: enrolling a refused source would keep mirroring the FC's
    // whole telemetry stream back to it even with its own bytes discarded.
    let (_dir, auth) = unpaired_auth(true);
    let lan: std::net::IpAddr = "10.0.0.9".parse().unwrap();
    for edge in [RawEdge::Tcp, RawEdge::Udp] {
        assert_eq!(
            admit_raw_peer(&auth, lan, None, 5760, edge),
            None,
            "{edge:?}: enforcement must refuse, not log and continue"
        );
    }
}

#[test]
fn enforcement_still_serves_the_on_box_and_lifeline_routes() {
    // The regression that would matter most: enforcing must not strand the
    // local operator or a fresh unit's only two ways in.
    let (_dir, auth) = unpaired_auth(true);
    for ip in ["127.0.0.1", "192.168.4.20", "192.168.7.2", "169.254.3.4"] {
        for edge in [RawEdge::Tcp, RawEdge::Udp] {
            assert_eq!(
                admit_raw_peer(&auth, ip.parse().unwrap(), lifeline_local(ip), 14550, edge),
                Some(ClientOrigin::Trusted),
                "{ip} on {edge:?} must survive enforcement"
            );
        }
    }
}

/// The raw edges have no credential channel, so on a PAIRED node an off-box
/// peer can never authenticate. Serving it anyway made the pairing gate
/// cosmetic for flight control. It is refused unless the operator opted
/// the raw edges into LAN access, and even then enforcement still wins.
/// Built over an explicit pairing file rather than `from_config` +
/// `ADOS_PAIRING_JSON` on purpose: that env var is read by the injector
/// gate in this same test binary, and setting a process global would make
/// two unrelated suites race.
#[test]
fn a_paired_node_serves_an_off_box_raw_peer_only_when_opted_into_lan_access() {
    let dir = tempfile::tempdir().unwrap();
    let pairing = write_pairing(dir.path(), r#"{"paired": true, "api_key": "k"}"#);
    for ip in ["10.0.0.9", "192.168.4.20", "8.8.8.8"] {
        let peer: std::net::IpAddr = ip.parse().unwrap();
        for edge in [RawEdge::Tcp, RawEdge::Udp] {
            assert_eq!(
                admit_raw_peer(
                    &ProxyAuth::new(false, pairing.clone()),
                    peer,
                    lifeline_local(ip),
                    5760,
                    edge
                ),
                None,
                "{ip} on {edge:?}: a paired node's raw edges are on-box only by default"
            );
            assert_eq!(
                admit_raw_peer(
                    &ProxyAuth::new(false, pairing.clone()).with_raw_lan_access(true),
                    peer,
                    lifeline_local(ip),
                    5760,
                    edge
                ),
                Some(ClientOrigin::Unauthenticated),
                "{ip} on {edge:?}: the opt-in serves it, recorded as anonymous"
            );
            assert_eq!(
                admit_raw_peer(
                    &ProxyAuth::new(true, pairing.clone()).with_raw_lan_access(true),
                    peer,
                    lifeline_local(ip),
                    5760,
                    edge
                ),
                Some(ClientOrigin::Unauthenticated),
                "{ip} on {edge:?}: the opt-in serves the LAN under the enforcing default too"
            );
        }
    }
    // The local operator is served whatever the flags say.
    assert_eq!(
        admit_raw_peer(
            &ProxyAuth::new(true, pairing),
            "127.0.0.1".parse().unwrap(),
            None,
            5760,
            RawEdge::Tcp
        ),
        Some(ClientOrigin::Trusted)
    );
}

#[test]
fn by_default_an_unpaired_node_refuses_lan_peers_on_the_raw_edges() {
    // The shipped posture: with the config defaults, a host on the same
    // LAN or hotspot cannot open 5760/14550 and command a never-paired
    // drone, while the on-box operator and the first-boot lifelines still
    // get in. The operator's LAN opt-in is what serves a desktop station.
    let cfg = crate::config::MavlinkConfig::default();
    let dir = tempfile::tempdir().unwrap();
    let unpaired = dir.path().join("absent.json");
    let auth = ProxyAuth::new(cfg.raw_proxy_enforce_auth, unpaired.clone())
        .with_raw_lan_access(cfg.raw_proxy_lan_access);
    let lan: std::net::IpAddr = "192.168.1.50".parse().unwrap();
    for edge in [RawEdge::Tcp, RawEdge::Udp] {
        assert_eq!(
            admit_raw_peer(&auth, lan, None, 5760, edge),
            None,
            "{edge:?}"
        );
        assert_eq!(
            admit_raw_peer(
                &auth,
                "192.168.4.20".parse().unwrap(),
                lifeline_local("192.168.4.20"),
                5760,
                edge
            ),
            Some(ClientOrigin::Trusted),
            "{edge:?}: a lifeline is still served"
        );
        assert_eq!(
            admit_raw_peer(&auth, "127.0.0.1".parse().unwrap(), None, 5760, edge),
            Some(ClientOrigin::Trusted),
            "{edge:?}: the on-box operator is still served"
        );
    }
    let opted_in = ProxyAuth::new(cfg.raw_proxy_enforce_auth, unpaired).with_raw_lan_access(true);
    assert_eq!(
        admit_raw_peer(&opted_in, lan, None, 5760, RawEdge::Tcp),
        Some(ClientOrigin::Unauthenticated)
    );
}

use super::*;
use std::io::Write;

fn addr(n: u16) -> SocketAddr {
    format!("127.0.0.1:{n}").parse().unwrap()
}

/// Write a `pairing.json` into `dir` and return its path.
fn write_pairing(dir: &std::path::Path, body: &str) -> PathBuf {
    let path = dir.join("pairing.json");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path
}

/// An auth context over a paired-state file with the given enforcement.
fn paired_auth(enforce: bool, key: &str) -> (tempfile::TempDir, WsProxyAuth) {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pairing(
        dir.path(),
        &format!(r#"{{"paired": true, "api_key": "{key}"}}"#),
    );
    (dir, WsProxyAuth::new(enforce, path))
}

/// An auth context over an absent (unpaired) state file.
fn unpaired_auth(enforce: bool) -> (tempfile::TempDir, WsProxyAuth) {
    let dir = tempfile::tempdir().unwrap();
    let auth = WsProxyAuth::new(enforce, dir.path().join("absent.json"));
    (dir, auth)
}

/// A private-LAN caller, the ordinary off-box WebSocket client.
const LAN: CallerClass = CallerClass::OperatorLan;

/// An unpaired node's MAVLink WebSocket used to admit any caller, so a host
/// the HTTP edge would refuse could still arm the aircraft through it. It
/// now keeps the same lifeline-only posture as the raw edges.
#[test]
fn unpaired_refuses_a_caller_that_is_not_on_box_or_a_lifeline() {
    let (_d, auth) = unpaired_auth(true);
    for caller in [CallerClass::Remote, CallerClass::OperatorLan] {
        let (admit, access) = auth.should_admit(caller, None, &[]);
        assert!(!admit, "{caller:?} must be refused while unpaired");
        assert_eq!(access, Access::Unauthorized);
    }
    for caller in [CallerClass::OnBox, CallerClass::Lifeline] {
        let (admit, access) = auth.should_admit(caller, None, &[]);
        assert!(admit, "{caller:?} is how a fresh unit is reached");
        assert_eq!(access, Access::Accept);
    }
}

fn handshake_headers(
    pairs: &[(&'static str, &str)],
) -> tokio_tungstenite::tungstenite::http::HeaderMap {
    let mut h = tokio_tungstenite::tungstenite::http::HeaderMap::new();
    for (k, v) in pairs {
        h.insert(*k, v.parse().unwrap());
    }
    h
}

/// The handshake itself, headers and all: a tunnel terminating on loopback
/// is a remote caller, on either pairing state.
#[test]
fn a_loopback_handshake_with_a_forwarding_header_is_refused() {
    let lo: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    let tunnelled = handshake_headers(&[("cf-connecting-ip", "203.0.113.5")]);

    let (_d, unpaired) = unpaired_auth(true);
    assert_eq!(
        unpaired.handshake_decision(lo, Some(lo), &tunnelled),
        (false, Access::Unauthorized)
    );
    assert_eq!(
        unpaired.handshake_decision(lo, Some(lo), &handshake_headers(&[])),
        (true, Access::Accept),
        "the local operator is still served"
    );

    let (_d2, paired) = paired_auth(true, "k");
    assert_eq!(
        paired.handshake_decision(lo, Some(lo), &tunnelled),
        (false, Access::Unauthorized)
    );
    // The key still authenticates a tunnelled caller.
    let keyed = handshake_headers(&[("x-forwarded-for", "203.0.113.5"), ("x-ados-key", "k")]);
    assert_eq!(
        paired.handshake_decision(lo, Some(lo), &keyed),
        (true, Access::Accept)
    );
}

// Posture: paired + on-box admits without a key.
#[test]
fn paired_on_box_admits_without_a_key() {
    let (_d, auth) = paired_auth(true, "k");
    assert_eq!(auth.decide(CallerClass::OnBox, None), Access::Accept);
}

// Posture: paired + off-box + the valid key admits.
#[test]
fn paired_off_box_with_a_valid_key_admits() {
    let (_d, auth) = paired_auth(true, "ados_secret");
    assert_eq!(auth.decide(LAN, Some("ados_secret")), Access::Accept);
}

// Posture: paired + off-box + no key or a wrong key is unauthorized.
#[test]
fn paired_off_box_without_the_key_is_unauthorized() {
    let (_d, auth) = paired_auth(true, "ados_secret");
    assert_eq!(auth.decide(LAN, None), Access::Unauthorized);
    assert_eq!(auth.decide(LAN, Some("wrong")), Access::Unauthorized);
}

// Two-stage rollout: with the enforce flag OFF an unauthorized posture is
// still ADMITTED (the default build is inert/observe-only).
#[test]
fn enforce_off_admits_an_unauthorized_connection_for_log_only() {
    let (_d, auth) = paired_auth(false, "ados_secret");
    let (admit, access) = auth.should_admit(LAN, None, &[]);
    assert!(
        admit,
        "with enforcement off the connection is still admitted"
    );
    assert_eq!(
        access,
        Access::Unauthorized,
        "the decision is surfaced as unauthorized so it can be logged"
    );
}

// Two-stage rollout: with the enforce flag ON an unauthorized posture is
// REJECTED.
#[test]
fn enforce_on_rejects_an_unauthorized_connection() {
    let (_d, auth) = paired_auth(true, "ados_secret");
    let (admit, access) = auth.should_admit(LAN, None, &[]);
    assert!(!admit, "with enforcement on the connection is rejected");
    assert_eq!(access, Access::Unauthorized);
}

// An authorized connection is admitted whether or not enforcement is on.
#[test]
fn an_authorized_connection_is_admitted_under_either_flag() {
    for enforce in [false, true] {
        let (_d, auth) = paired_auth(enforce, "k");
        let (admit, access) = auth.should_admit(LAN, Some("k"), &[]);
        assert!(admit);
        assert_eq!(access, Access::Accept);
    }
}

// A valid ticket in the offered subprotocols authenticates a paired,
// off-box, keyless connection (the browser GCS path) under either flag.
#[test]
fn a_valid_ticket_admits_off_box_without_a_key() {
    use ados_protocol::ws_ticket::{WsTicketIssuer, SCOPE_MAVLINK_WS};
    for enforce in [false, true] {
        let (_d, auth) = paired_auth(enforce, "ados_secret");
        let token = WsTicketIssuer::from_api_key("ados_secret")
            .mint(SCOPE_MAVLINK_WS, 30)
            .unwrap()
            .token;
        let offered = vec!["ados-ws-ticket".to_string(), token];
        // off-box, no key, but a valid ticket => Accept under either flag.
        let (admit, access) = auth.should_admit(LAN, None, &offered);
        assert!(admit, "a valid ticket admits even with enforcement on");
        assert_eq!(access, Access::Accept);
    }
}

// A ticket minted for a DIFFERENT key does not authenticate.
#[test]
fn a_ticket_for_the_wrong_key_is_unauthorized() {
    use ados_protocol::ws_ticket::{WsTicketIssuer, SCOPE_MAVLINK_WS};
    let (_d, auth) = paired_auth(true, "ados_secret");
    let token = WsTicketIssuer::from_api_key("a-different-key")
        .mint(SCOPE_MAVLINK_WS, 30)
        .unwrap()
        .token;
    let offered = vec!["ados-ws-ticket".to_string(), token];
    let (admit, access) = auth.should_admit(LAN, None, &offered);
    assert!(!admit);
    assert_eq!(access, Access::Unauthorized);
}

// A ticket minted for another scope does not authenticate the MAVLink WS.
#[test]
fn a_ticket_for_the_wrong_scope_is_unauthorized() {
    use ados_protocol::ws_ticket::WsTicketIssuer;
    let (_d, auth) = paired_auth(true, "ados_secret");
    let token = WsTicketIssuer::from_api_key("ados_secret")
        .mint("gs.pic_events", 30)
        .unwrap()
        .token;
    let offered = vec!["ados-ws-ticket".to_string(), token];
    let (admit, _access) = auth.should_admit(LAN, None, &offered);
    assert!(!admit);
}

#[test]
fn extract_ticket_finds_the_value_after_the_marker() {
    let offered = vec!["ados-ws-ticket".to_string(), "v1|s|1|2|ff".to_string()];
    assert_eq!(extract_ticket(&offered), Some("v1|s|1|2|ff"));
    // Marker with no following value, and no marker at all, both yield None.
    assert_eq!(extract_ticket(&["ados-ws-ticket".to_string()]), None);
    assert_eq!(extract_ticket(&["mavlink".to_string()]), None);
}

// A pair/unpair that happens while the proxy runs is honoured for a new
// connection within the TTL once it lapses (the cache re-reads the file).
#[test]
fn posture_re_reads_after_the_ttl_lapses() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pairing(dir.path(), r#"{"paired": false}"#);
    let auth = WsProxyAuth::new(true, path.clone());
    // First read: unpaired => an off-box non-lifeline caller is refused.
    assert_eq!(auth.decide(LAN, None), Access::Unauthorized);
    assert_eq!(auth.decide(CallerClass::Lifeline, None), Access::Accept);
    // Now pair and let the TTL lapse so the next read picks it up.
    write_pairing(dir.path(), r#"{"paired": true, "api_key": "k2"}"#);
    {
        let mut c = auth.cache.lock().unwrap();
        c.at = Instant::now() - (PAIRING_TTL + Duration::from_secs(1));
    }
    // Paired: the lifeline is off-box like anyone else and needs the key.
    assert_eq!(
        auth.decide(CallerClass::Lifeline, None),
        Access::Unauthorized
    );
    assert_eq!(auth.decide(LAN, Some("k2")), Access::Accept);
}

#[test]
fn fresh_peers_are_kept_and_stale_peers_are_evicted() {
    let now = Instant::now();
    let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
    // A peer seen just now and one seen well past the TTL.
    peers.insert(addr(14550), now);
    peers.insert(addr(14551), now - (UDP_PEER_TTL + Duration::from_secs(1)));

    evict_stale_peers(&mut peers, now);

    assert!(peers.contains_key(&addr(14550)), "fresh peer must survive");
    assert!(
        !peers.contains_key(&addr(14551)),
        "stale peer must be evicted"
    );
}

#[test]
fn peer_exactly_at_ttl_boundary_is_kept() {
    let now = Instant::now();
    let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
    peers.insert(addr(14550), now - UDP_PEER_TTL);
    evict_stale_peers(&mut peers, now);
    assert!(
        peers.contains_key(&addr(14550)),
        "a peer exactly at the TTL boundary is still present"
    );
}

#[test]
fn cap_drops_least_recently_seen_first() {
    let now = Instant::now();
    let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
    // Three peers with distinct last-seen times; cap to two.
    peers.insert(addr(1), now - Duration::from_secs(3)); // oldest
    peers.insert(addr(2), now - Duration::from_secs(2));
    peers.insert(addr(3), now - Duration::from_secs(1)); // newest

    cap_peers(&mut peers, 2);

    assert_eq!(peers.len(), 2);
    assert!(
        !peers.contains_key(&addr(1)),
        "the least-recently-seen peer is dropped first"
    );
    assert!(peers.contains_key(&addr(2)));
    assert!(peers.contains_key(&addr(3)));
}

#[test]
fn cap_is_a_noop_under_the_limit() {
    let now = Instant::now();
    let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
    peers.insert(addr(1), now);
    peers.insert(addr(2), now);
    cap_peers(&mut peers, 64);
    assert_eq!(peers.len(), 2);
}

#[test]
fn reinserting_a_peer_refreshes_its_last_seen_so_it_survives_eviction() {
    let now = Instant::now();
    let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
    // Peer first seen long ago.
    peers.insert(addr(14550), now - (UDP_PEER_TTL + Duration::from_secs(5)));
    // A new datagram refreshes the timestamp (mirrors the recv path).
    peers.insert(addr(14550), now);
    evict_stale_peers(&mut peers, now);
    assert!(
        peers.contains_key(&addr(14550)),
        "a refreshed peer must not be evicted"
    );
}

/// Age the pairing cache so the next read re-loads `pairing.json`.
fn expire_pairing_cache(auth: &WsProxyAuth) {
    let mut c = auth.cache.lock().unwrap();
    c.at = Instant::now() - (PAIRING_TTL + Duration::from_secs(1));
}

fn test_fc(dir: &std::path::Path) -> Arc<FcConnection> {
    FcConnection::new(
        crate::config::MavlinkConfig::default(),
        Arc::new(tokio::sync::Mutex::new(
            crate::state::VehicleState::default(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::param_cache::ParamCache::new(dir.join("params.json")),
        )),
    )
}

/// A session admitted as a lifeline on an unpaired node loses the lane
/// once the node is paired.
#[test]
fn a_lifeline_ws_session_is_revoked_by_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let auth = WsProxyAuth::new(true, dir.path().join("pairing.json"));
    let ap: std::net::IpAddr = "192.168.4.20".parse().unwrap();
    let headers = handshake_headers(&[]);
    let (admit, access) = auth.handshake_decision(ap, lifeline_local("192.168.4.20"), &headers);
    assert!(admit);
    let session = auth.ws_session(ap, lifeline_local("192.168.4.20"), &headers, access);
    assert!(auth.ws_still_admitted(&session));
    write_pairing(dir.path(), r#"{"paired": true, "api_key": "k"}"#);
    expire_pairing_cache(&auth);
    assert!(!auth.ws_still_admitted(&session));
}

/// A ticket is spent on use, so a ticket session is kept while the key it
/// was verified under stands, and dropped when the key rotates.
#[test]
fn a_ticket_ws_session_ends_when_the_key_rotates() {
    use ados_protocol::ws_ticket::{WsTicketIssuer, SCOPE_MAVLINK_WS};
    let (dir, auth) = paired_auth(true, "k1");
    let token = WsTicketIssuer::from_api_key("k1")
        .mint(SCOPE_MAVLINK_WS, 30)
        .unwrap()
        .token;
    let headers = handshake_headers(&[(
        "sec-websocket-protocol",
        &format!("ados-ws-ticket, {token}"),
    )]);
    let lan: std::net::IpAddr = "192.168.1.50".parse().unwrap();
    let (admit, access) = auth.handshake_decision(lan, None, &headers);
    assert!(admit);
    let session = auth.ws_session(lan, None, &headers, access);
    assert!(session.ticket_posture.is_some());
    assert!(auth.ws_still_admitted(&session));
    write_pairing(dir.path(), r#"{"paired": true, "api_key": "k2"}"#);
    expire_pairing_cache(&auth);
    assert!(!auth.ws_still_admitted(&session));
}

/// A TCP session opened as a lifeline while unpaired closes within one
/// re-check period of the node being paired.
#[tokio::test]
async fn a_lifeline_tcp_session_closes_after_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let auth = ProxyAuth::new(true, dir.path().join("pairing.json"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    let ap: std::net::IpAddr = "192.168.4.20".parse().unwrap();
    let local = lifeline_local("192.168.4.20");
    let (admit, _) = auth.classify(ap, local);
    assert!(admit, "a lifeline is served while unpaired");
    let server = tokio::spawn(handle_tcp_client(
        test_fc(dir.path()),
        stream,
        ClientOrigin::Trusted,
        auth.clone(),
        ap,
        local,
    ));
    write_pairing(dir.path(), r#"{"paired": true, "api_key": "k"}"#);
    expire_pairing_cache(&auth);
    let mut buf = [0u8; 16];
    let closed = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the session must close after pairing");
    server.await.unwrap();
}

/// A message far over the FC write cap ends the WebSocket session instead
/// of being buffered.
#[tokio::test]
async fn an_oversize_ws_message_closes_the_socket() {
    let (dir, auth) = unpaired_auth(true);
    let fc = test_fc(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        handle_ws_client(fc, stream, peer, auth).await;
    });
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let url = format!("ws://{addr}/");
    let (mut ws, _) = tokio_tungstenite::client_async(url.as_str(), tcp)
        .await
        .unwrap();
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        ws.send(Message::Binary(vec![0u8; 1024 * 1024])),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server must end the oversize session")
        .unwrap();
}

#[test]
fn the_ws_limits_are_set() {
    let cfg = ws_config();
    assert_eq!(cfg.max_message_size, Some(64 * 1024));
    assert_eq!(cfg.max_frame_size, Some(64 * 1024));
}

/// A port held by something else is retried until it frees, and the
/// listening flag tells the truth throughout.
#[tokio::test]
async fn a_busy_port_is_retried_until_it_frees() {
    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let listening = ListenFlag::new();
    let cancel = Shutdown::new();
    let task = tokio::spawn({
        let fc = test_fc(dir.path());
        let auth = ProxyAuth::new(true, dir.path().join("pairing.json"));
        let listening = listening.clone();
        let cancel = cancel.clone();
        async move { run_tcp_proxy(fc, "127.0.0.1", port, auth, listening, cancel).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!listening.get(), "a failed bind must not read as listening");
    drop(holder);
    let deadline = Instant::now() + Duration::from_secs(8);
    while !listening.get() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(listening.get(), "the retry binds once the port frees");
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok());
    cancel.trigger();
    task.await.unwrap();
    assert!(!listening.get(), "the flag clears when the proxy returns");
}
