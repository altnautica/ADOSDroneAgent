use axum::http::HeaderValue;
use base64::Engine as _;

use super::*;

const NOW: i64 = 1_800_000_000_000;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn key() -> [u8; 32] {
    std::array::from_fn(|i| 0x80 + i as u8)
}

fn approved() -> Vec<phones::ApprovedPhone> {
    vec![phones::ApprovedPhone {
        phone_pub_b64: b64(&[7u8; 32]),
        fingerprint: "AB12-CD34-EF56-7890".into(),
        label: "Ops phone".into(),
        uplink_key_b64: b64(&key()),
        approved_at_ms: NOW,
    }]
}

fn signed(body: &[u8], ts: i64, k: &[u8; 32]) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        sign::PHONE_KEY_HEADER,
        HeaderValue::from_str(&b64(&[7u8; 32])).unwrap(),
    );
    h.insert(
        sign::TS_HEADER,
        HeaderValue::from_str(&ts.to_string()).unwrap(),
    );
    h.insert(
        sign::MAC_HEADER,
        HeaderValue::from_str(&b64(&sign::mac(k, ts, body))).unwrap(),
    );
    h
}

const TAKE: &[u8] = br#"{"op":"take","holder_device_id":"a1b2c3d4e5f6","holder_name":"Ops phone","yield_port":47811}"#;

/// The cross-language vector the phone's signer is tested against. Printed so
/// the phone's test carries the agent's own output.
#[test]
fn the_uplink_mac_vector() {
    let mac = sign::mac(&key(), NOW, TAKE);
    println!("uplink mac vector: {}", hex::encode(mac));
    assert_eq!(
        hex::encode(mac),
        "bb00e64b4ccd8554d9c7f466b08f7c3546bd6ae53dd395fdeeb1f629820ac163",
        "the phone's signer pins this vector"
    );
    // Every input is bound: the key, the time and the body.
    assert_ne!(mac, sign::mac(&[0u8; 32], NOW, TAKE));
    assert_ne!(mac, sign::mac(&key(), NOW + 1, TAKE));
    assert_ne!(mac, sign::mac(&key(), NOW, b"{}"));
}

/// The take-back call's vector, which the phone's verifier pins. Printed so
/// the phone's test carries the agent's own output. The call is signed under
/// its own context, so a phone's own signed take never verifies as one.
#[test]
fn the_yield_mac_vector() {
    let body = sign::yield_body("gs-0a1b2c");
    assert_eq!(body, r#"{"box_device_id":"gs-0a1b2c","op":"yield"}"#);
    let mac = sign::yield_mac(&key(), NOW, body.as_bytes());
    println!("yield mac vector: {}", hex::encode(mac));
    assert_eq!(
        hex::encode(mac),
        "af2aceac943790b74e28d4692ab1c2301c8cf3f8bfa22f47511650cd2b7e9cfc"
    );
    // The same inputs under the phone's context, which a yield verifier refuses.
    assert_eq!(
        hex::encode(sign::mac(&key(), NOW, body.as_bytes())),
        "013cc7565b5e4fce990998ca1b7ee74f21e41d94e5b10944a51c305fd2429331"
    );
    assert_ne!(
        sign::yield_mac(&key(), NOW, TAKE),
        sign::mac(&key(), NOW, TAKE)
    );
}

#[test]
fn an_approved_phone_with_a_good_mac_is_admitted_once() {
    let mut guard = sign::ReplayGuard::default();
    let h = signed(TAKE, NOW, &key());
    assert_eq!(
        admit_phone(&approved(), &h, TAKE, NOW + 500, &mut guard),
        Ok("AB12-CD34-EF56-7890".into())
    );
    // The same request again is a replay.
    assert_eq!(
        admit_phone(&approved(), &h, TAKE, NOW + 600, &mut guard),
        Err(sign::MacError::Replay)
    );
}

/// The guard outlives a restart of this service: a request accepted before
/// it is still a replay after it.
#[test]
fn a_restart_does_not_forget_the_last_accepted_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("uplink-replay.json");
    let h = signed(TAKE, NOW, &key());
    let mut guard = sign::ReplayGuard::load(path.clone());
    assert_eq!(
        admit_phone(&approved(), &h, TAKE, NOW + 500, &mut guard),
        Ok("AB12-CD34-EF56-7890".into())
    );
    drop(guard);

    let mut restarted = sign::ReplayGuard::load(path.clone());
    assert_eq!(
        admit_phone(&approved(), &h, TAKE, NOW + 600, &mut restarted),
        Err(sign::MacError::Replay)
    );
    // The phone's next request goes through.
    assert_eq!(
        admit_phone(
            &approved(),
            &signed(TAKE, NOW + 1, &key()),
            TAKE,
            NOW + 700,
            &mut restarted
        ),
        Ok("AB12-CD34-EF56-7890".into())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "it sits beside the uplink keys");
    }
}

/// A guard that cannot write the timestamp down refuses the request rather
/// than accept one a restart would let through again; the box says the fault
/// is its own.
#[test]
fn a_timestamp_that_cannot_be_recorded_refuses_the_request() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("not-a-dir"), b"").unwrap();
    let mut guard = sign::ReplayGuard::load(dir.path().join("not-a-dir/uplink-replay.json"));
    let e = admit_phone(
        &approved(),
        &signed(TAKE, NOW, &key()),
        TAKE,
        NOW,
        &mut guard,
    );
    assert_eq!(e, Err(sign::MacError::Unrecorded));
    assert_eq!(
        sign::MacError::Unrecorded.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(sign::MacError::Unrecorded.code(), "E_UPLINK_REPLAY_STORE");
    assert_eq!(sign::MacError::Replay.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(sign::MacError::Replay.code(), "E_UPLINK_UNAUTHORIZED");
}

/// The guard keeps the phones heard from most recently, never more than its
/// cap, and a file it cannot parse reads as empty.
#[test]
fn the_replay_guard_keeps_the_most_recent_phones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("uplink-replay.json");
    let mut guard = sign::ReplayGuard::load(path.clone());
    for i in 0..(sign::REPLAY_KEPT as i64 + 6) {
        guard.admit(&format!("FP-{i}"), NOW + i, NOW).unwrap();
    }
    let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let last = file["last"].as_object().unwrap();
    assert_eq!(last.len(), sign::REPLAY_KEPT);
    assert!(!last.contains_key("FP-0") && !last.contains_key("FP-5"));
    assert_eq!(last["FP-6"], json!(NOW + 6));

    std::fs::write(&path, b"{not json").unwrap();
    let mut fresh = sign::ReplayGuard::load(path);
    assert_eq!(fresh.admit("FP-0", NOW, NOW), Ok(()));
}

#[test]
fn every_other_phone_request_is_refused_with_its_reason() {
    let mut guard = sign::ReplayGuard::default();
    let cases = [
        (HeaderMap::new(), TAKE, sign::MacError::Missing),
        (
            signed(TAKE, NOW, &[1u8; 32]),
            TAKE,
            sign::MacError::Mismatch,
        ),
        // A body changed after signing.
        (
            signed(TAKE, NOW, &key()),
            br#"{"op":"take"}"#.as_slice(),
            sign::MacError::Mismatch,
        ),
        (
            signed(TAKE, NOW - 121_000, &key()),
            TAKE,
            sign::MacError::Clock,
        ),
    ];
    for (h, body, want) in cases {
        assert_eq!(
            admit_phone(&approved(), &h, body, NOW, &mut guard),
            Err(want)
        );
    }
    // A phone the box never approved.
    assert_eq!(
        admit_phone(&[], &signed(TAKE, NOW, &key()), TAKE, NOW, &mut guard),
        Err(sign::MacError::UnknownPhone)
    );
    for e in [
        sign::MacError::Missing,
        sign::MacError::UnknownPhone,
        sign::MacError::Clock,
        sign::MacError::Replay,
        sign::MacError::Mismatch,
        sign::MacError::Unrecorded,
    ] {
        assert!(e.detail().ends_with('.'));
    }
}

#[test]
fn a_take_registers_the_phones_own_address_for_the_take_back() {
    let req: UplinkRequest = serde_json::from_slice(TAKE).unwrap();
    let cmd = phone_command(&req, "FP", Some("192.168.1.40".parse().unwrap()));
    assert_eq!(
        cmd,
        json!({
            "op": "uplink_take",
            "holder_device_id": "a1b2c3d4e5f6",
            "holder_name": "Ops phone",
            "phone_key": "FP",
            "yield_url": "http://192.168.1.40:47811/uplink/yield",
        })
    );
    let give: UplinkRequest =
        serde_json::from_value(json!({"op": "give_back", "holder_device_id": "a1b2"})).unwrap();
    assert_eq!(
        phone_command(&give, "FP", None),
        json!({"op": "uplink_give_back", "holder_device_id": "a1b2", "phone_key": "FP"})
    );
}

async fn body(resp: Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn socket_replies_map_to_the_contract() {
    let (s, v) = body(reply_from(Some(
        json!({"ok": true, "state": "standby", "uplink": {"state": "standby"}}),
    )))
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["state"], json!("standby"));

    for (code, want) in [
        ("E_BIND_ACTIVE", StatusCode::CONFLICT),
        ("E_UPLINK_HELD", StatusCode::CONFLICT),
        ("E_NOT_HOLDER", StatusCode::FORBIDDEN),
        ("E_TX_STOP_FAILED", StatusCode::SERVICE_UNAVAILABLE),
        ("E_BAD_HOLDER", StatusCode::BAD_REQUEST),
    ] {
        let (s, v) = body(reply_from(Some(
            json!({"ok": false, "error": code, "detail": "One sentence."}),
        )))
        .await;
        assert_eq!(s, want, "{code}");
        assert_eq!(v, json!({"error": code, "detail": "One sentence."}));
    }

    let (s, v) = body(reply_from(None)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["error"], json!("E_GROUNDLINK_UNAVAILABLE"));
}

#[test]
fn the_uplink_view_is_null_unless_the_radio_is_reporting() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(uplink_view(dir.path()), Value::Null);
    std::fs::write(
        dir.path().join(ados_protocol::paths::WFB_STATS_JSON),
        json!({"state": "active", "uplink": {"state": "standby", "holder_name": "Ops phone"}})
            .to_string(),
    )
    .unwrap();
    assert_eq!(uplink_view(dir.path())["holder_name"], json!("Ops phone"));
}

#[test]
fn approving_a_phone_again_replaces_its_key_and_the_store_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("uplink-phones.json");
    for i in 0..(phones::MAX_PHONES as i64 + 2) {
        phones::remember(
            &path,
            phones::ApprovedPhone {
                phone_pub_b64: b64(&[i as u8; 32]),
                fingerprint: format!("FP-{i}"),
                label: "p".into(),
                uplink_key_b64: b64(&[i as u8; 32]),
                approved_at_ms: NOW + i,
            },
        )
        .unwrap();
    }
    let kept = phones::load(&path);
    assert_eq!(kept.len(), phones::MAX_PHONES);
    assert!(
        phones::by_fingerprint(&kept, "FP-0").is_none(),
        "the oldest goes first"
    );
    let mut again = kept[3].clone();
    again.uplink_key_b64 = b64(&[0xEE; 32]);
    again.approved_at_ms = NOW + 100;
    phones::remember(&path, again.clone()).unwrap();
    let kept = phones::load(&path);
    assert_eq!(kept.len(), phones::MAX_PHONES);
    assert_eq!(
        phones::by_pub(&kept, &again.phone_pub_b64)
            .unwrap()
            .uplink_key(),
        Some([0xEE; 32])
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the uplink keys are secrets");
    }
}

/// Two approvals at once each read the store, add their phone and write it
/// back; without serialising that, one approval's key is lost.
#[test]
fn approvals_of_different_phones_at_once_keep_every_key() {
    for round in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink-phones.json");
        let start = std::sync::Arc::new(std::sync::Barrier::new(phones::MAX_PHONES));
        let approvals: Vec<_> = (0..phones::MAX_PHONES)
            .map(|i| {
                let (path, start) = (path.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    phones::remember(
                        &path,
                        phones::ApprovedPhone {
                            phone_pub_b64: b64(&[i as u8; 32]),
                            fingerprint: format!("FP-{i}"),
                            label: "p".into(),
                            uplink_key_b64: b64(&[i as u8; 32]),
                            approved_at_ms: NOW + i as i64,
                        },
                    )
                    .unwrap();
                })
            })
            .collect();
        for a in approvals {
            a.join().unwrap();
        }
        assert_eq!(
            phones::load(&path).len(),
            phones::MAX_PHONES,
            "round {round} lost an approval"
        );
    }
}

#[tokio::test]
async fn the_take_back_call_is_signed_and_confirmed_by_a_2xx() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = s.read(&mut buf).await.unwrap();
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    });
    let url = sign::yield_url(addr.ip(), addr.port());
    assert!(sign::call_yield(&url, &key(), "gs-0a1b2c", NOW).await);
    let request = server.await.unwrap();
    assert!(request.starts_with("POST /uplink/yield HTTP/1.1\r\n"));
    let (head, body) = request.split_once("\r\n\r\n").unwrap();
    let mac_line = head
        .lines()
        .find_map(|l| l.strip_prefix("x-ados-uplink-mac: "))
        .unwrap();
    assert_eq!(
        mac_line,
        b64(&sign::yield_mac(&key(), NOW, body.as_bytes()))
    );
    assert!(
        !sign::verify(&key(), NOW, body.as_bytes(), mac_line),
        "the take-back call must not verify as a phone's request"
    );
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({"op": "yield", "box_device_id": "gs-0a1b2c"})
    );
    assert!(head.contains(&format!("x-ados-uplink-ts: {NOW}")));
}

/// The phone answers only after its radio has read quiet, which takes it up
/// to five seconds; the box waits that out rather than start on top of it.
#[tokio::test(start_paused = true)]
async fn a_phone_that_answers_after_its_quiet_check_is_waited_for() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _phone = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = s.read(&mut buf).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });
    let url = sign::yield_url(addr.ip(), addr.port());
    assert!(sign::call_yield(&url, &key(), "gs-0a1b2c", NOW).await);
}

#[tokio::test(start_paused = true)]
async fn a_phone_that_does_not_answer_in_the_window_is_not_waited_for() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Accepts and never answers.
    let _hold = tokio::spawn(async move {
        let (_s, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let url = sign::yield_url(addr.ip(), addr.port());
    let started = tokio::time::Instant::now();
    assert!(!sign::call_yield(&url, &key(), "gs", NOW).await);
    assert!(started.elapsed() >= sign::YIELD_TIMEOUT);
    assert_eq!(sign::parse_http_url("ftp://x"), None);
    assert_eq!(sign::parse_http_url("http://phone.local:80/x"), None);
}
