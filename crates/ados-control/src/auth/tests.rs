use super::*;
use std::io::Write;

const GET: http::Method = http::Method::GET;
const HEAD: http::Method = http::Method::HEAD;
const POST: http::Method = http::Method::POST;

/// Claiming a device over the LAN is the documented local-first flow, so
/// the pairing handshake must stay public. The unpaired-peer filter refuses
/// non-public routes from an ordinary LAN peer; if these were gated too, a
/// fresh device could not be paired from the network it sits on - trading an
/// exposure for an unpairable unit.
#[test]
fn the_pairing_handshake_stays_public_so_lan_claiming_still_works() {
    for path in [
        "/api/pairing/info",
        "/api/pairing/code",
        "/api/pairing/claim",
        "/healthz",
    ] {
        assert!(
            is_public(path),
            "{path} must stay reachable to claim a device"
        );
    }
    // The routes that actually command the aircraft are NOT public, so on an
    // unpaired device they fall to the peer filter.
    assert!(!is_public("/api/command"));
    assert!(!is_public("/api/config"));
}

/// A radio-only drone has no LAN pairing, so its claim would hand a
/// relayed caller the master key outright; every pairing mutation and the
/// code a claim needs stay off the relay.
#[test]
fn a_relayed_caller_reaches_no_pairing_route_that_mints_or_clears_a_key() {
    for path in [
        "/api/pairing/claim",
        "/api/pairing/code",
        "/api/pairing/unpair",
        "/api/pairing/accept",
    ] {
        assert!(relay_forbidden(path), "{path}");
    }
    assert!(
        is_public("/api/pairing/claim"),
        "claim stays public on the LAN — a fresh operator holds no key yet"
    );
    assert!(!relay_forbidden("/api/pairing/info"));
}

#[test]
fn credential_issuing_paths_are_refused_over_the_relay() {
    for path in [
        "/api/mcp/tokens",
        "/api/mcp/revoke",
        "/api/dashboard/pin/set",
        "/api/dashboard/pin/clear",
        "/api/plugins/capability-token",
    ] {
        assert!(relay_forbidden(path), "{path} hands out a credential");
    }
}

#[test]
fn trust_root_paths_are_refused_over_the_relay() {
    for path in [
        "/api/wfb/pair/unpair",
        "/api/wfb/pair/local-bind",
        "/api/v1/ground-station/wfb/pair",
        "/api/v1/setup/reset",
        "/api/v1/setup/cloud-choice",
        // `apply` carries a cloud choice and a profile with restart in one
        // body, so it reaches everything `cloud-choice` does.
        "/api/v1/setup/apply",
        "/api/v1/setup/profile",
        "/api/v1/setup/remote-access/cloudflare",
        // The path the ground-station router actually registers
        // (`APIRouter(prefix="/v1/ground-station")` + `@router.post(
        // "/factory-reset")`, mounted under `/api`). The literal used to
        // read `.../ui/factory-reset`, which no router has ever served —
        // so this assertion passed while the real route was reachable
        // over the radio with full on-box authority.
        "/api/v1/ground-station/factory-reset",
    ] {
        assert!(relay_forbidden(path), "{path} must not cross the relay");
    }
}

/// The old literal must stay refused-by-absence: it names nothing, so if it
/// ever comes back as a denylist entry the route-table guard below fails.
/// Asserting it here as well documents that the typo is gone rather than
/// merely moved.
#[test]
fn the_mounted_factory_reset_path_is_the_one_guarded() {
    assert!(relay_forbidden("/api/v1/ground-station/factory-reset"));
    assert!(
        !RELAY_FORBIDDEN_PATHS.contains(&"/api/v1/ground-station/ui/factory-reset"),
        "the unmounted `/ui/` spelling guarded nothing and must not return"
    );
}

/// A relayed caller must be refused the reset outright. This exercises the
/// edge's actual decision — presence of the relay header plus the denylist
/// — rather than the predicate alone, because the predicate being right is
/// worth nothing if the edge stops consulting it.
#[test]
fn factory_reset_is_refused_over_the_relay_at_the_edge() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(RELAYED_HEADER, "1".parse().unwrap());
    let path = "/api/v1/ground-station/factory-reset";
    let is_relayed = headers.contains_key(RELAYED_HEADER);
    assert!(
        is_relayed && relay_forbidden(path),
        "a request carrying {RELAYED_HEADER} must be refused {path}"
    );
    // The same path with no relay header is ordinary on-LAN operation and
    // stays reachable — the refusal is about the lane, not the route.
    let direct = axum::http::HeaderMap::new();
    assert!(!direct.contains_key(RELAYED_HEADER));
}

/// A denylist entry that matches no served path is the defect this guard
/// exists for: it reads as protection while the real route is open. Every
/// literal must resolve against the committed route table, which covers
/// both the native surface and the residual FastAPI one.
#[test]
fn every_denylisted_path_is_a_route_something_actually_serves() {
    let table = include_str!("../../../../docs/api-surface.md");
    let served: std::collections::HashSet<&str> = table
        .lines()
        .filter_map(|line| {
            // Table rows are `| METHOD | `/path` | … |`; the path is the
            // only backticked cell that starts with a slash.
            let mut cells = line.split('|').map(str::trim);
            cells.next()?;
            cells.next()?;
            let path = cells.next()?.trim_matches('`');
            path.starts_with('/').then_some(path)
        })
        .collect();
    assert!(
        served.len() > 100,
        "route table parsed only {} paths — the parser or the table shape drifted",
        served.len()
    );
    for path in RELAY_FORBIDDEN_PATHS {
        assert!(
            served.contains(path),
            "relay_forbidden guards {path}, which no router serves — \
                 either the literal is wrong or the route was removed. \
                 A denylist entry matching nothing is worse than none."
        );
    }
}

#[test]
fn the_predicate_and_the_enumeration_agree() {
    for path in RELAY_FORBIDDEN_PATHS {
        assert!(relay_forbidden(path), "{path} is listed but not matched");
    }
}

/// The operating surface the relay exists to carry stays reachable. A list
/// that quietly grew to cover ordinary operation would break the lane's
/// whole purpose, so pin the paths that must keep working.
#[test]
fn the_ordinary_operating_surface_still_crosses_the_relay() {
    // A ticket bound to the request carries the linked ground station's
    // authority, so plugin, service and reboot operations cross too; the
    // armed interlock, not this list, guards them in flight.
    for path in [
        "/api/status",
        "/api/status/full",
        "/api/telemetry",
        "/api/config",
        "/api/params",
        "/api/services",
        "/api/logs",
        "/api/vision/detections/latest",
        "/api/plugins/install",
        "/api/plugins/install_from_url",
        "/api/plugins/com.example.tool/enable",
        "/api/plugins/com.example.tool/grant",
        "/api/services/ados-mavlink/restart",
        "/api/v1/setup/reboot",
        "/api/v1/setup/shutdown",
        "/api/v1/system/restart-supervisor",
    ] {
        assert!(
            !relay_forbidden(path),
            "{path} is ordinary operation and must still cross"
        );
    }
}

fn write_pairing(dir: &std::path::Path, body: &str) -> PathBuf {
    let path = dir.join("pairing.json");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path
}

#[test]
fn unpaired_opens_every_route() {
    let dir = tempfile::tempdir().unwrap();
    // No pairing file at all → unpaired.
    let state = PairingState::with_path(dir.path().join("absent.json"));
    assert_eq!(state.current(), Pairing::Unpaired);
    assert!(state.authorize("/api/status", None));
    assert!(state.authorize("/api/status", Some("anything")));
}

#[test]
fn paired_requires_the_exact_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "ados_secret"}"#);
    let state = PairingState::with_path(path);
    assert_eq!(state.current(), Pairing::Paired("ados_secret".to_string()));
    assert!(state.authorize("/api/status", Some("ados_secret")));
    assert!(!state.authorize("/api/status", Some("wrong")));
    assert!(!state.authorize("/api/status", None));
}

#[test]
fn the_native_exempt_set_is_open_even_when_paired() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "k"}"#);
    let state = PairingState::with_path(path);
    // The exact public set for this surface.
    assert!(state.authorize("/healthz", None));
    assert!(state.authorize("/api/version", None));
    assert!(state.authorize("/api/pairing/info", None));
    assert!(state.authorize("/api/pairing/code", None));
    assert!(state.authorize("/api/pairing/claim", None));
    // /api/time is NOT exempt, so a paired agent gates it.
    assert!(!state.authorize("/api/time", None));
    // A non-exempt route still needs the key.
    assert!(!state.authorize("/api/status", None));
    assert!(state.authorize("/api/status", Some("k")));
}

#[test]
fn is_public_is_exactly_the_exempt_paths() {
    for p in [
        "/healthz",
        "/api/ping",
        "/api/version",
        "/api/pairing/info",
        "/api/pairing/code",
        "/api/pairing/claim",
        // The dashboard-PIN gate a keyless off-box browser must reach.
        "/api/dashboard/pin/status",
        "/api/dashboard/pin/verify",
        "/api/dashboard/pin/set",
        // The ground-station WebSocket relays: the upgrade bypasses the HTTP
        // key gate, so the handler does its own ticket/header auth.
        "/api/v1/ground-station/ws/uplink",
        "/api/v1/ground-station/pic/events",
        "/api/v1/ground-station/ws/mesh",
        "/api/v1/ground-station/ws/buttons",
    ] {
        assert!(is_public(p), "{p} should be public");
    }
    for p in [
        "/api/time",
        "/api/status",
        "/api/command",
        "/api/pairing/unpair",
        // PIN reset is NOT public — it stays behind the normal on-box/key gate.
        "/api/dashboard/pin/clear",
        "/v1/openapi.json",
    ] {
        assert!(!is_public(p), "{p} should NOT be public");
    }
}

#[test]
fn only_the_exact_tile_shape_skips_the_request_budget() {
    assert!(is_tile_read(
        "/api/v1/ground-station/tiles/field/14/11720/7570.mvt"
    ));
    for p in [
        // The listing is an ordinary charged read.
        "/api/v1/ground-station/tiles",
        "/api/v1/ground-station/tiles/",
        "/api/v1/ground-station/tiles/field/14/11720",
        "/api/v1/ground-station/tiles/field/14/11720/7570.mvt/extra",
        "/api/v1/ground-station/tiles/field//11720/7570.mvt",
        "/api/v1/ground-station/tiles/../../../status",
        "/api/v1/ground-station/tiles/../14/11720/7570.mvt",
        "/api/v1/ground-station/status",
    ] {
        assert!(!is_tile_read(p), "{p} must stay charged");
    }
    // Exempt from the budget is not public: the key is still required.
    assert!(!is_public(
        "/api/v1/ground-station/tiles/field/14/11720/7570.mvt"
    ));
}

#[test]
fn the_operator_ui_is_reachable_while_unpaired() {
    // The shell an operator has to load before they can do anything at all,
    // including read the pairing code that would let them pair.
    for p in [
        "/cockpit",
        "/cockpit/",
        "/cockpit/assets/index-abc123.js",
        "/cockpit/assets/index-abc123.css",
        "/cockpit/brand.svg",
        "/",
        "/index.html",
        "/assets/index-def456.js",
        "/brand.svg",
        "/favicon.ico",
    ] {
        assert!(is_operator_ui(p), "{p} is the operator's own UI");
    }
}

#[test]
fn serving_the_ui_does_not_open_the_data_behind_it() {
    // The whole point: the shell loads, everything it asks for stays shut.
    // A regression here would hand an unpaired node's telemetry, config and
    // command surface to any peer on the network.
    for p in [
        "/api/status",
        "/api/telemetry",
        "/api/config",
        "/api/command",
        "/api/services",
        "/api/v1/ground-station/status",
        // Live video is not UI. It sits outside `/api/` and must not be
        // swept in by a "anything that is not an API path" shortcut.
        "/whep",
        // Neither is the route-surface documentation.
        "/docs",
        "/docs/oauth2-redirect",
    ] {
        assert!(!is_operator_ui(p), "{p} must NOT be served while unpaired");
    }
}

#[test]
fn a_cockpit_lookalike_path_is_not_the_cockpit() {
    // Prefix matching is easy to get wrong in the direction that opens
    // something: `/cockpit` and `/assets` must not vouch for a sibling that
    // merely starts with the same letters. An extension-less sibling is a
    // dashboard client route and gets the shell, but a file under one is
    // not a bundle asset, and a data prefix never vouches for anything.
    for p in [
        "/cockpitfoo/payload.bin",
        "/cockpit-admin/dump.json",
        "/api/cockpit",
        "/assetsfoo/index.js",
        "/whep/cockpit",
        "/docs/cockpit",
    ] {
        assert!(!is_operator_ui(p), "{p} is not the cockpit");
    }
}

#[test]
fn the_unpaired_gate_pins_private_lan_data_and_admits_the_shell() {
    use crate::auth::UnpairedDecision;
    // An ordinary private-LAN browser: trusted for the operator-UI scope,
    // so it is not flatly refused; its DATA calls require a PIN session.
    let lan = CallerClass::OperatorLan;

    // The shell loads, so the operator can see the node and its pairing code.
    for p in [
        "/cockpit/",
        "/cockpit/assets/index-abc.js",
        "/",
        "/brand.svg",
    ] {
        assert_eq!(
            unpaired_decision(&GET, p, true, lan),
            UnpairedDecision::Allow,
            "{p} must load so the operator has a surface at all"
        );
    }
    for p in ["/api/status", "/api/config", "/api/command", "/whep"] {
        assert_eq!(
            unpaired_decision(&GET, p, true, lan),
            UnpairedDecision::RequirePin,
            "{p} must be PIN-gated for a private-LAN peer while unpaired"
        );
    }
    // Claiming the device over its own LAN is the documented local-first
    // flow, so it stays open to this caller.
    assert_eq!(
        unpaired_decision(&POST, "/api/pairing/claim", true, lan),
        UnpairedDecision::Allow
    );
    assert_eq!(
        unpaired_decision(&GET, "/api/pairing/info", true, lan),
        UnpairedDecision::Allow
    );

    // A dashboard client route reloaded or deep-linked loads the shell
    // rather than a JSON refusal; the data it then asks for stays gated.
    for p in ["/settings/network", "/plugins", "/logs/flight"] {
        assert_eq!(
            unpaired_decision(&GET, p, true, lan),
            UnpairedDecision::Allow,
            "{p} is a client route of the dashboard"
        );
        assert_eq!(
            unpaired_decision(&HEAD, p, true, lan),
            UnpairedDecision::Allow,
            "{p}"
        );
        assert_eq!(
            unpaired_decision(&POST, p, true, lan),
            UnpairedDecision::RequirePin,
            "a non-read on {p} is not the shell"
        );
    }
    for p in [
        "/api/status",
        "/whep/abc",
        "/hls/main/index.m3u8",
        "/ws",
        "/ws/telemetry",
        "/healthz/deep",
        "/docs",
        "/settings/dump.json",
    ] {
        assert_eq!(
            unpaired_decision(&GET, p, true, lan),
            UnpairedDecision::RequirePin,
            "{p} is not a client route"
        );
    }
}

/// A remote caller — a public-WAN host, a tunnelled internet request that
/// arrives on loopback, or an unidentifiable peer — reaches neither the
/// data plane nor the claim that would hand it the node's key.
#[test]
fn a_remote_caller_is_refused_data_and_the_claim_while_unpaired() {
    use crate::auth::UnpairedDecision;
    for p in ["/api/status", "/api/command", "/whep", "/api/pairing/claim"] {
        assert_eq!(
            unpaired_decision(&GET, p, true, CallerClass::Remote),
            UnpairedDecision::Refuse,
            "{p} must be refused to a remote caller while unpaired"
        );
    }
    // The shell and the non-issuing public routes are still served, so a
    // browser is never left with nothing to read.
    for p in ["/cockpit/", "/api/pairing/info", "/healthz"] {
        assert_eq!(
            unpaired_decision(&GET, p, true, CallerClass::Remote),
            UnpairedDecision::Allow,
            "{p}"
        );
    }
}

#[test]
fn pairing_the_device_opens_everything_the_gate_was_holding() {
    use crate::auth::UnpairedDecision;
    for caller in [CallerClass::OperatorLan, CallerClass::Remote] {
        for p in ["/api/status", "/api/command", "/whep", "/cockpit/"] {
            assert_eq!(
                unpaired_decision(&GET, p, false, caller),
                UnpairedDecision::Allow,
                "{p} is not this gate's business once paired"
            );
        }
    }
}

#[test]
fn the_reachable_callers_are_the_ones_a_fresh_device_is_reached_from() {
    use crate::auth::UnpairedDecision;
    // The local operator and the first-boot lifelines keep unrestricted
    // unpaired data access (no PIN): these are the surfaces the PIN is first
    // created on, and they may claim the device.
    for caller in [CallerClass::OnBox, CallerClass::Lifeline] {
        for p in ["/api/status", "/api/pairing/claim"] {
            assert_eq!(
                unpaired_decision(&GET, p, true, caller),
                UnpairedDecision::Allow,
                "{caller:?} {p}"
            );
        }
    }
}

#[test]
fn a_paired_state_without_a_key_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    // paired:true with no key is a damaged record, not an unclaimed node.
    let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": ""}"#);
    let state = PairingState::with_path(path);
    assert_eq!(state.current(), Pairing::Unreadable);
    assert!(!state.authorize("/api/status", Some("")));
}

/// A corrupt file must not open the data plane: that is exactly the state
/// in which the next claim would mint a fresh key for whoever asks.
#[test]
fn a_malformed_pairing_file_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pairing(dir.path(), "this is not json");
    let state = PairingState::with_path(path);
    assert_eq!(state.current(), Pairing::Unreadable);
    assert!(!state.authorize("/api/status", None));
    assert!(!state.authorize("/api/status", Some("anything")));
    // The public handshake paths still answer (the handler reports the fault).
    assert!(state.authorize("/api/pairing/info", None));
}

#[test]
fn the_configured_key_and_the_pairing_key_are_both_credentials() {
    let paired = Pairing::Paired("pair-key".into());
    assert!(credential_matches(&paired, "", Some("pair-key")));
    assert!(credential_matches(&paired, "cfg-key", Some("cfg-key")));
    assert!(!credential_matches(&paired, "cfg-key", Some("other")));
    assert!(!credential_matches(&paired, "", Some("")));
    // An unconfigured key never matches an empty presentation.
    assert!(!credential_matches(&Pairing::Unpaired, "", Some("")));
}

#[test]
fn an_unpaired_node_treats_no_arbitrary_string_as_a_credential() {
    // Cloud-posture routes demand a real credential even while unpaired;
    // "any non-empty key" was not one.
    assert!(!credential_matches(&Pairing::Unpaired, "", Some("x")));
    assert!(!credential_matches(&Pairing::Unreadable, "", Some("x")));
    assert!(credential_matches(
        &Pairing::Unpaired,
        "cfg-key",
        Some("cfg-key")
    ));
}

#[test]
fn the_native_edge_accepts_the_configured_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "pair-key"}"#);
    let state = PairingState::with_path(path).with_security(Arc::new(SecurityState::fixed(
        crate::config::ControlSecurityConfig {
            security: crate::config::SecuritySection {
                api: crate::config::ApiSecuritySection {
                    api_key: "cfg-key".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
    )));
    assert!(state.authorize("/api/status", Some("cfg-key")));
    assert!(state.authorize("/api/status", Some("pair-key")));
    assert!(!state.authorize("/api/status", Some("nope")));
    assert!(state.credential_valid(Some("cfg-key")));
}

#[test]
fn constant_time_eq_matches_byte_equality() {
    // Equal slices compare equal; any single-byte or length difference is
    // rejected, exactly as `==` would, only without the early exit.
    assert!(constant_time_eq(b"ados_secret", b"ados_secret"));
    assert!(!constant_time_eq(b"ados_secret", b"ados_secre1"));
    assert!(!constant_time_eq(b"ados_secret", b"xdos_secret"));
    assert!(!constant_time_eq(b"ados_secret", b"ados_secret_longer"));
    assert!(!constant_time_eq(b"ados_secret", b"short"));
    assert!(constant_time_eq(b"", b""));
    assert!(!constant_time_eq(b"", b"x"));
}

fn peer(ip: &str) -> PeerKey {
    PeerKey::of(Some(ip.parse().unwrap()))
}

#[test]
fn rate_limiter_admits_up_to_capacity_then_rejects() {
    let limiter = RateLimiter::new(3, Duration::from_secs(60));
    let a = peer("192.168.1.50");
    assert!(limiter.check(a));
    assert!(limiter.check(a));
    assert!(limiter.check(a));
    // Fourth in the same window is rejected.
    assert!(!limiter.check(a));
}

/// One host exhausting its budget leaves every other caller's untouched: a
/// single shared bucket let any LAN host 429 the paired operator.
#[test]
fn one_caller_exhausting_its_budget_does_not_starve_another() {
    let limiter = RateLimiter::new(2, Duration::from_secs(60));
    let flooder = peer("192.168.1.66");
    while limiter.check(flooder) {}
    assert!(
        limiter.check(peer("192.168.1.50")),
        "the operator is served"
    );
    // Addresses within one IPv6 /64 are one caller.
    let v6 = peer("2001:db8:1:2::5");
    assert!(limiter.check(v6));
    assert!(limiter.check(peer("2001:db8:1:2::6")));
    assert!(!limiter.check(peer("2001:db8:1:2:ffff::1")));
}

#[test]
fn rate_limiter_refills_after_the_window() {
    let limiter = RateLimiter::new(1, Duration::from_millis(20));
    let a = peer("192.168.1.50");
    assert!(limiter.check(a));
    assert!(!limiter.check(a));
    std::thread::sleep(Duration::from_millis(30));
    assert!(limiter.check(a), "the window refilled");
}

/// Link-local and unique-local callers share a /64 with every host on the
/// LAN, so each is its own caller; a global /64 still collapses.
#[test]
fn link_local_and_ula_peers_each_get_their_own_budget() {
    let limiter = RateLimiter::new(60, Duration::from_secs(60));
    for pair in [
        ("fe80::1", "fe80::2"),
        ("febf::1", "febf::2"),
        ("fd12:3456::1", "fd12:3456::2"),
    ] {
        let (a, b) = (peer(pair.0), peer(pair.1));
        assert_ne!(a, b, "{pair:?}");
        while limiter.check(a) {}
        assert!(limiter.check(b), "{} keeps its budget", pair.1);
    }
    assert_eq!(peer("2001:db8:1:2::10"), peer("2001:db8:1:2::20"));
}

#[test]
fn signing_disable_and_credential_issuance_are_refused_over_the_relay() {
    for path in [
        "/api/mavlink/signing/disable-on-fc",
        "/api/plugins/capability-token",
    ] {
        assert!(relay_forbidden(path), "{path} must not cross the relay");
    }
    for path in [
        "/api/services",
        "/api/plugins/com.example.tool",
        "/api/plugins/com.example.tool/disable",
        "/api/mavlink/signing/capability",
    ] {
        assert!(!relay_forbidden(path), "{path} stays reachable");
    }
}

#[test]
fn relayed_config_writes_refuse_trust_root_keys() {
    for key in [
        "security.api.api_key",
        "security",
        "server.cloud.mqtt_broker",
        "server.mode",
        "server.self_hosted.url",
        "pairing.convex_url",
        "remote_access.cloudflare.token_path",
    ] {
        assert!(relay_config_key_forbidden(key), "{key}");
    }
    for key in [
        "video.wfb.fleet_slot",
        "swarm.enabled",
        "mavlink.endpoints",
        "network.hotspot.enabled",
        "serverx",
        "securityx.y",
    ] {
        assert!(!relay_config_key_forbidden(key), "{key}");
    }
}
