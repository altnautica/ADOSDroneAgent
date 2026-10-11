use super::*;
use ados_groundlink::pairing::{generate_keypair, open_from_peer, seal_to_peer_with};

const T0: i64 = 1_800_000_000_000;

async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn material() -> InviteMaterial {
    InviteMaterial {
        rx_key: (0u8..64).map(|i| i.wrapping_mul(7)).collect(),
        hop_key: ados_radio::hop::derive_pair_key(Some(&[0x5Au8; 64])),
        fleet_id: 7,
        slots: vec![(1, "drone-a1b2c3".into()), (2, "drone-d4e5f6".into())],
        channel: 149,
        rendezvous_channel: 149,
        mcs: 1,
        video_fec: (8, 12),
        aux_fec: (1, 2),
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn create(store: &mut InviteStore, phone_pub: &[u8], id: &str, now: i64) -> Response {
    let req = CreateInviteRequest {
        phone_pub_b64: b64(phone_pub),
        label: Some("Pilot phone".into()),
    };
    create_with(true, store, &req, id.into(), true, now)
}

fn b64d(v: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(v.as_str().unwrap())
        .unwrap()
}

#[tokio::test]
async fn create_approve_fetch_once_then_gone() {
    let phone = generate_keypair();
    let mut store = InviteStore::new();
    let id = "00112233445566778899aabbccddeeff";

    let resp = create(&mut store, &phone.public, id, T0);
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["invite_id"], id);
    assert_eq!(body["expires_at_ms"], T0 + INVITE_TTL_MS);
    assert_eq!(body["phone_fingerprint"], phone_fingerprint(&phone.public));

    let list = body_json(list_with(true, &mut store, T0 + 1)).await;
    assert_eq!(list["pending"].as_array().unwrap().len(), 1);
    assert_eq!(list["pending"][0]["label"], "Pilot phone");

    let resp = fetch_with(true, &mut store, id, T0 + 2);
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(resp).await, json!({"state": "pending"}));

    let resp = approve_with(true, &mut store, id, || Ok(material()), T0 + 3);
    assert_eq!(resp.status(), StatusCode::OK);
    // Approved invites leave the pending list.
    let list = body_json(list_with(true, &mut store, T0 + 4)).await;
    assert!(list["pending"].as_array().unwrap().is_empty());

    let resp = fetch_with(true, &mut store, id, T0 + 5);
    assert_eq!(resp.status(), StatusCode::OK);
    let sealed = body_json(resp).await;
    let plaintext = open_from_peer(
        &phone.secret.to_bytes(),
        &b64d(&sealed["gs_eph_pub_b64"]),
        &b64d(&sealed["nonce_b64"]),
        &b64d(&sealed["ciphertext_b64"]),
        &invite_context(id),
    )
    .unwrap();
    assert_eq!(plaintext, bundle_plaintext(&material()));
    let bundle: Value = serde_json::from_slice(&plaintext).unwrap();
    assert_eq!(bundle["v"], 1);
    assert_eq!(bundle["fleet_id"], 7);
    assert_eq!(bundle["bandwidth_mhz"], 20);
    assert_eq!(
        bundle["ports"],
        json!({"video": 0, "control": 1, "aux_down": 2})
    );
    assert_eq!(
        bundle["slots"][1],
        json!({"slot": 2, "device_id": "drone-d4e5f6"})
    );
    assert_eq!(b64d(&bundle["rx_key_b64"]), material().rx_key);
    // The hop key is the derived HMAC key, never the raw shared key.
    assert_eq!(b64d(&bundle["hop_key_b64"]), material().hop_key.to_vec());
    assert_ne!(b64d(&bundle["hop_key_b64"]), vec![0x5Au8; 64]);

    let resp = fetch_with(true, &mut store, id, T0 + 6);
    assert_eq!(resp.status(), StatusCode::GONE);
    assert_eq!(body_json(resp).await, json!({"state": "expired"}));
}

#[tokio::test]
async fn reject_answers_403_and_cannot_be_approved() {
    let phone = generate_keypair();
    let mut store = InviteStore::new();
    let id = "aa";
    create(&mut store, &phone.public, id, T0);
    assert_eq!(
        reject_with(true, &mut store, id, T0 + 1).status(),
        StatusCode::OK
    );
    let resp = fetch_with(true, &mut store, id, T0 + 2);
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await, json!({"state": "rejected"}));
    let resp = approve_with(true, &mut store, id, || Ok(material()), T0 + 3);
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn pending_and_uncollected_invites_expire_to_410() {
    let phone = generate_keypair();
    let mut store = InviteStore::new();
    create(&mut store, &phone.public, "p", T0);
    create(&mut store, &phone.public, "a", T0);
    approve_with(true, &mut store, "a", || Ok(material()), T0 + 60_000);

    // The pending one lapses at T0 + TTL; the approved one has its own window.
    let resp = fetch_with(true, &mut store, "p", T0 + INVITE_TTL_MS);
    assert_eq!(resp.status(), StatusCode::GONE);
    let resp = approve_with(true, &mut store, "p", || Ok(material()), T0 + INVITE_TTL_MS);
    assert_eq!(resp.status(), StatusCode::GONE);
    let resp = fetch_with(true, &mut store, "a", T0 + 60_000 + INVITE_TTL_MS);
    assert_eq!(resp.status(), StatusCode::GONE);

    // Long after, the tombstones are forgotten.
    let resp = fetch_with(
        true,
        &mut store,
        "p",
        T0 + 10 * INVITE_TTL_MS + TOMBSTONE_MS,
    );
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_fifth_pending_invite_is_refused() {
    let phone = generate_keypair();
    let mut store = InviteStore::new();
    for i in 0..MAX_PENDING {
        let resp = create(&mut store, &phone.public, &format!("id{i}"), T0);
        assert_eq!(resp.status(), StatusCode::CREATED);
    }
    let resp = create(&mut store, &phone.public, "id5", T0);
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        "E_INVITE_LIMIT"
    );
    // Deciding one frees a place.
    reject_with(true, &mut store, "id0", T0 + 1);
    assert_eq!(
        create(&mut store, &phone.public, "id5", T0 + 2).status(),
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn bad_phone_key_and_unpaired_station_are_refused() {
    let mut store = InviteStore::new();
    let resp = create(&mut store, &[1u8; 31], "x", T0);
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Low-order points (u = 0, u = 1) are refused before anything is stored.
    let mut one = [0u8; 32];
    one[0] = 1;
    for low_order in [[0u8; 32], one] {
        let resp = create(&mut store, &low_order, "lo", T0);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            "E_PHONE_PUB_INVALID"
        );
    }
    assert!(store.invites.is_empty());
    let req = CreateInviteRequest {
        phone_pub_b64: b64(&[9u8; 32]),
        label: None,
    };
    let resp = create_with(true, &mut store, &req, "y".into(), false, T0);
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        "E_GS_NOT_PAIRED"
    );

    let phone = generate_keypair();
    create(&mut store, &phone.public, "z", T0);
    let resp = approve_with(
        true,
        &mut store,
        "z",
        || Err(MaterialError::NotPaired),
        T0 + 1,
    );
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let resp = approve_with(
        true,
        &mut store,
        "z",
        || Err(MaterialError::HopKeyUnavailable),
        T0 + 1,
    );
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    // A failed approval leaves the invite pending.
    assert_eq!(
        fetch_with(true, &mut store, "z", T0 + 2).status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        fetch_with(true, &mut store, "nope", T0 + 2).status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn drone_profile_gets_404_on_every_route() {
    let mut store = InviteStore::new();
    let req = CreateInviteRequest {
        phone_pub_b64: b64(&generate_keypair().public),
        label: None,
    };
    for resp in [
        create_with(false, &mut store, &req, "d".into(), true, T0),
        list_with(false, &mut store, T0),
        approve_with(false, &mut store, "d", || Ok(material()), T0),
        reject_with(false, &mut store, "d", T0),
        fetch_with(false, &mut store, "d", T0),
    ] {
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(resp).await,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }
    assert!(store.invites.is_empty());
}

#[test]
fn every_invite_route_is_relay_forbidden() {
    for path in [
        "/api/v1/ground-station/wfb/invite",
        "/api/v1/ground-station/wfb/invite/0011/approve",
        "/api/v1/ground-station/wfb/invite/0011/reject",
        "/api/v1/ground-station/wfb/invite/0011",
    ] {
        assert!(
            crate::auth::relay_forbidden(path),
            "{path} must be relay-forbidden"
        );
    }
}

#[test]
fn fingerprint_is_four_groups_of_uppercase_hex() {
    let fp = phone_fingerprint(&[0u8; 32]);
    // sha256 of 32 zero bytes begins 66687aadf862bd77.
    assert_eq!(fp, "6668-7AAD-F862-BD77");
}

/// The shared cross-language vector: a deterministic seal of the fixed
/// material. Set `ADOS_WRITE_PHONE_INVITE_VECTOR=1` to (re)write the file;
/// otherwise the committed file must match what the sealing code produces.
#[test]
fn phone_invite_vector_matches_the_committed_fixture() {
    let phone_secret: [u8; 32] = std::array::from_fn(|i| i as u8 + 1);
    let phone_public = x25519_public(phone_secret);
    let eph_secret: [u8; 32] = std::array::from_fn(|i| 0x40 + i as u8);
    let nonce: [u8; 12] = std::array::from_fn(|i| 0xA0 + i as u8);
    let invite_id = "0123456789abcdef0123456789abcdef";
    let context = invite_context(invite_id);
    let plaintext = bundle_plaintext(&material());
    let sealed = seal_to_peer_with(eph_secret, nonce, &phone_public, &context, &plaintext).unwrap();
    let opened = open_from_peer(
        &phone_secret,
        &sealed.eph_pub,
        &sealed.nonce,
        &sealed.ciphertext,
        &context,
    )
    .unwrap();
    assert_eq!(opened, plaintext);

    let vector = json!({
        "invite_id": invite_id,
        "phone_secret_key_hex": hex::encode(phone_secret),
        "phone_public_key_hex": hex::encode(phone_public),
        "gs_ephemeral_public_hex": hex::encode(sealed.eph_pub),
        "nonce_hex": hex::encode(sealed.nonce),
        "context_hex": hex::encode(&context),
        "ciphertext_hex": hex::encode(&sealed.ciphertext),
        "phone_fingerprint": phone_fingerprint(&phone_public),
        "expected_plaintext_json": String::from_utf8(plaintext).unwrap(),
    });
    let rendered = format!("{}\n", serde_json::to_string_pretty(&vector).unwrap());
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/phone-invite-vector.json");
    if std::env::var_os("ADOS_WRITE_PHONE_INVITE_VECTOR").is_some() {
        std::fs::write(&path, &rendered).unwrap();
    }
    let committed = std::fs::read_to_string(&path).unwrap();
    assert_eq!(committed.replace("\r\n", "\n"), rendered);
}

fn x25519_public(secret: [u8; 32]) -> [u8; 32] {
    // Derive through the same sealing primitive: sealing from `secret` to a
    // fixed peer reports `secret`'s public half as `eph_pub`.
    let peer = generate_keypair().public;
    seal_to_peer_with(secret, [0u8; 12], &peer, b"", b"")
        .unwrap()
        .eph_pub
}
