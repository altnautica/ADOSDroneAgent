use super::*;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const DRONE: &str = "40bb1a5a";

fn issuer() -> RelayTicketIssuer {
    RelayTicketIssuer::from_secret(SECRET)
}

const HEX32: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn an_unset_drone_takes_the_first_secret_it_is_offered() {
    // Trust-on-first-use: at pair time there is nothing better available on
    // a headless aircraft.
    assert_eq!(decide_accept(None, HEX32), AcceptDecision::Accept);
}

#[test]
fn a_drone_already_holding_a_secret_refuses_to_be_re_keyed_over_the_air() {
    // The delivery rides the channel the credential protects, and that
    // channel is unauthenticated. Without this, anyone in radio range
    // overwrites the secret and then mints tickets the drone accepts — a
    // credential that reads as protection while granting the access it
    // exists to deny. Deliberate replacement goes through unpair.
    let other = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    assert_eq!(
        decide_accept(Some(HEX32), other),
        AcceptDecision::RefusedWouldOverwrite
    );
}

#[test]
fn restating_the_same_secret_is_a_no_op_not_a_conflict() {
    // The ground station restates it on every reconcile tick, so the
    // steady state must not look like an attack.
    assert_eq!(
        decide_accept(Some(HEX32), HEX32),
        AcceptDecision::AlreadyHeld
    );
    // Case differences in hex are the same secret.
    assert_eq!(
        decide_accept(Some(&HEX32.to_uppercase()), HEX32),
        AcceptDecision::AlreadyHeld
    );
}

#[test]
fn a_malformed_offer_is_refused_before_anything_is_stored() {
    // Storing a short or non-hex value would leave the drone holding
    // something credential-shaped that authenticates nothing, and would
    // then block the real secret via first-write-wins.
    for bad in ["", "abc", "zzzz", &"a".repeat(63), &"a".repeat(65)] {
        assert_eq!(
            decide_accept(None, bad),
            AcceptDecision::RefusedMalformed,
            "{bad:?}"
        );
    }
}

#[test]
fn a_malformed_offer_cannot_displace_a_held_secret() {
    assert_eq!(
        decide_accept(Some(HEX32), "nope"),
        AcceptDecision::RefusedMalformed
    );
}

const NONCE: &str = "00112233445566778899aabbccddeeff";

fn status_get() -> RequestBinding<'static> {
    RequestBinding::new("GET", "/api/status", b"")
}

fn mint(now: i64) -> String {
    issuer().mint_with_nonce(DRONE, &status_get(), now, NONCE)
}

#[test]
fn a_freshly_minted_ticket_verifies_for_its_target_and_request() {
    let t = mint(1_000);
    let v = issuer().verify(&t, DRONE, &status_get(), 1_005).unwrap();
    assert_eq!(v.nonce, NONCE);
    assert_eq!(
        v.valid_until,
        1_000 + TICKET_LIFETIME_SECONDS + CLOCK_SKEW_TOLERANCE_SECONDS
    );
}

#[test]
fn a_ticket_presented_with_another_request_is_a_binding_mismatch() {
    // A ticket lifted off the broadcast uplink must not authorize any
    // request but the one it was minted for.
    let t = mint(1_000);
    for other in [
        RequestBinding::new("POST", "/api/command", b"{}"),
        RequestBinding::new("POST", "/api/status", b""),
        RequestBinding::new("GET", "/api/status?x=1", b""),
        RequestBinding::new("GET", "/api/status", b"x"),
    ] {
        assert_eq!(
            issuer().verify(&t, DRONE, &other, 1_005),
            Err(RelayTicketError::BindingMismatch),
            "{other:?}"
        );
    }
    assert_eq!(
        RelayTicketError::BindingMismatch.reason(),
        "binding_mismatch"
    );
}

#[test]
fn a_ticket_for_one_drone_does_not_verify_at_another() {
    let t = mint(1_000);
    assert_eq!(
        issuer().verify(&t, "f6aa0aa4", &status_get(), 1_005),
        Err(RelayTicketError::BindingMismatch)
    );
}

#[test]
fn a_different_pair_secret_does_not_verify() {
    // A second ground station holding the shared fleet radio key still
    // cannot mint a ticket this drone accepts.
    let t = mint(1_000);
    let other = RelayTicketIssuer::from_secret(b"ffffffffffffffffffffffffffffffff");
    assert_eq!(
        other.verify(&t, DRONE, &status_get(), 1_005),
        Err(RelayTicketError::BadSignature)
    );
}

#[test]
fn a_ws_ticket_key_derivation_does_not_verify_a_relay_ticket() {
    // Domain separation: only the label keeps one credential from being
    // replayed as the other.
    let t = mint(1_000);
    let mut mac = HmacSha256::new_from_slice(SECRET).unwrap();
    mac.update(crate::ws_ticket::TICKET_KEY_LABEL);
    let ws_keyed = RelayTicketIssuer {
        key: mac.finalize().into_bytes().to_vec(),
    };
    assert_eq!(
        ws_keyed.verify(&t, DRONE, &status_get(), 1_005),
        Err(RelayTicketError::BadSignature)
    );
    assert_ne!(RELAY_KEY_LABEL, crate::ws_ticket::TICKET_KEY_LABEL);
}

#[test]
fn clock_skew_inside_the_tolerance_is_accepted_and_beyond_it_is_refused() {
    let now = 10_000;
    // Minter's clock 200 s ahead of ours: inside the tolerance.
    let ahead = mint(now + 200);
    assert!(issuer().verify(&ahead, DRONE, &status_get(), now).is_ok());
    // 400 s ahead: refused, and said why.
    let far_ahead = mint(now + 400);
    assert_eq!(
        issuer().verify(&far_ahead, DRONE, &status_get(), now),
        Err(RelayTicketError::ClockSkew)
    );
    // Minter's clock behind ours: accepted until the expiry is more than
    // the tolerance in the past.
    let behind = mint(now - 300);
    assert!(issuer().verify(&behind, DRONE, &status_get(), now).is_ok());
    let stale = mint(now - 400);
    assert_eq!(
        issuer().verify(&stale, DRONE, &status_get(), now),
        Err(RelayTicketError::Expired)
    );
}

#[test]
fn a_ticket_is_spent_once() {
    let cache = new_replay_cache();
    let t = mint(1_000);
    let v = issuer().verify(&t, DRONE, &status_get(), 1_001).unwrap();
    assert_eq!(v.spend(&cache, 1_001), Ok(()));
    let again = issuer().verify(&t, DRONE, &status_get(), 1_002).unwrap();
    assert_eq!(again.spend(&cache, 1_002), Err(RelayTicketError::Replayed));
    assert_eq!(RelayTicketError::Replayed.reason(), "replayed");
}

#[test]
fn every_mint_carries_a_fresh_nonce() {
    let a = issuer().mint(DRONE, &status_get(), 1_000).unwrap();
    let b = issuer().mint(DRONE, &status_get(), 1_000).unwrap();
    assert_ne!(a, b);
}

#[test]
fn a_tampered_field_is_refused_before_anything_else_is_read() {
    let t = mint(1_000);
    // Push the expiry out. Without an HMAC over the exact substring this
    // would simply extend the ticket's life.
    let forged = t.replacen("|1030|", "|99999999999|", 1);
    assert_ne!(forged, t);
    assert_eq!(
        issuer().verify(&forged, DRONE, &status_get(), 1_005),
        Err(RelayTicketError::BadSignature)
    );
}

#[test]
fn a_path_containing_a_pipe_still_binds_exactly() {
    let req = RequestBinding::new("GET", "/api/logs?q=a|b", b"");
    let t = issuer().mint_with_nonce(DRONE, &req, 1_000, NONCE);
    assert!(issuer().verify(&t, DRONE, &req, 1_001).is_ok());
    let other = RequestBinding::new("GET", "/api/logs?q=a", b"");
    assert_eq!(
        issuer().verify(&t, DRONE, &other, 1_001),
        Err(RelayTicketError::BindingMismatch)
    );
}

#[test]
fn a_malformed_token_is_refused_rather_than_panicking() {
    for bad in [
        "",
        "v2",
        "v1|relay.http|d|1|2|ff",
        "not-a-ticket",
        "v2|relay.http|d|1|2|short|GET|/x|ab|ff",
        "v2|relay.http|d|x|2|00112233445566778899aabbccddeeff|GET|/x|ab|ff",
    ] {
        assert!(
            issuer().verify(bad, DRONE, &status_get(), 1_000).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn a_refusal_body_names_the_code_and_reason() {
    let body: serde_json::Value = serde_json::from_slice(&refusal_body("clock_skew")).unwrap();
    assert_eq!(body["error"], RELAY_REFUSAL_CODE);
    assert_eq!(body["reason"], "clock_skew");
}

#[test]
fn the_tunnel_binding_ignores_the_ticket_and_key_order() {
    let a: serde_json::Value =
        serde_json::from_str(r#"{"op":"put","key":"video.bitrate","value":4}"#).unwrap();
    let b: serde_json::Value =
        serde_json::from_str(r#"{"value":4,"ticket":"anything","key":"video.bitrate","op":"put"}"#)
            .unwrap();
    assert_eq!(tunnel_binding_body(&a), tunnel_binding_body(&b));
    let c: serde_json::Value =
        serde_json::from_str(r#"{"op":"put","key":"video.bitrate","value":5}"#).unwrap();
    assert_ne!(tunnel_binding_body(&a), tunnel_binding_body(&c));
}

// Checked at compile time: a shorter secret would weaken every ticket,
// and a runtime assertion on a constant is not a test.
const _: () = assert!(RELAY_SECRET_LEN >= 32);

#[test]
fn applying_an_offer_to_an_unset_drone_stores_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets/relay-peer-secret");

    assert_eq!(
        apply_offered_secret(&path, HEX32).unwrap(),
        AcceptDecision::Accept
    );
    assert_eq!(load_secret_at(&path).as_deref(), Some(HEX32));
}

#[test]
fn applying_a_restated_secret_leaves_the_stored_value_alone() {
    // The ground station restates on every reconcile tick, so the ordinary
    // steady state must not read as a refusal.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets/relay-peer-secret");
    apply_offered_secret(&path, HEX32).unwrap();

    assert_eq!(
        apply_offered_secret(&path, HEX32).unwrap(),
        AcceptDecision::AlreadyHeld
    );
}

#[test]
fn applying_a_stranger_offer_leaves_the_stored_secret_intact() {
    // The attack the first-write-wins rule exists to stop: anyone in radio
    // range replacing the credential with their own would leave something
    // that reads as protection while granting exactly what it denies.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets/relay-peer-secret");
    apply_offered_secret(&path, HEX32).unwrap();

    let attacker = "f".repeat(64);
    assert_eq!(
        apply_offered_secret(&path, &attacker).unwrap(),
        AcceptDecision::RefusedWouldOverwrite
    );
    assert_eq!(
        load_secret_at(&path).as_deref(),
        Some(HEX32),
        "the held secret is untouched"
    );
}

#[test]
fn applying_a_malformed_offer_reaches_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets/relay-peer-secret");

    assert_eq!(
        apply_offered_secret(&path, "nonsense").unwrap(),
        AcceptDecision::RefusedMalformed
    );
    assert!(load_secret_at(&path).is_none());
}

#[cfg(unix)]
#[test]
fn an_empty_leftover_file_is_replaced_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-peer-secret");
    // What a crash between truncate and write used to leave behind, with
    // the looser mode an earlier build could have set.
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(
        apply_offered_secret(&path, HEX32).unwrap(),
        AcceptDecision::Accept
    );
    assert_eq!(load_secret_at(&path).as_deref(), Some(HEX32));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "got {mode:o}");
}

#[test]
fn concurrent_offers_accept_exactly_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets/relay-peer-secret");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let offers = [HEX32.to_string(), "f".repeat(64)];
    let handles: Vec<_> = offers
        .iter()
        .cloned()
        .map(|offer| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (offer.clone(), apply_offered_secret(&path, &offer).unwrap())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let winners: Vec<_> = results
        .iter()
        .filter(|(_, d)| *d == AcceptDecision::Accept)
        .collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    assert!(results
        .iter()
        .any(|(_, d)| *d == AcceptDecision::RefusedWouldOverwrite));
    assert_eq!(
        load_secret_at(&path).as_deref(),
        Some(winners[0].0.as_str())
    );
    // No temp sibling is left behind.
    let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
    assert_eq!(leftovers, 1);
}

#[test]
fn an_absent_secret_reads_as_none_rather_than_an_error() {
    // Which is what keeps the drone inert until one is delivered.
    let dir = tempfile::tempdir().unwrap();
    assert!(load_secret_at(&dir.path().join("nope")).is_none());
}
