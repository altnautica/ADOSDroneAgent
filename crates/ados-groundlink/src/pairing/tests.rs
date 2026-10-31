//! Pairing service tests.
//!
//! The golden fixtures under `testdata/` are the wire replies, snapshots and
//! pair-event journal lines of the reference implementation for one fixed op
//! sequence (synthetic ids). Volatile fields are normalised before comparing,
//! on both sides, by [`norm`]: every `*_ms` timestamp and every approval time
//! becomes `"<ms>"`, a six-digit `code` becomes `"<code>"` and
//! `invite_blob_hex` becomes `"<hex>"`. The values the placeholders hide are
//! checked separately: `closes_at_ms - opened_at_ms` equals the duration, the
//! code is the one the window returned, and the invite blob opens under it with
//! the relay's key. A malformed-JSON reply is compared by its
//! `"malformed request: "` prefix, since the parser's own message differs.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UdpSocket, UnixStream};

use super::client::request_join;
use super::crypto::{decrypt_invite, generate_keypair};
use super::relay_rekey::{self, RelayRekeyReceiver, RelayRekeyState};
use super::window::{new_invite_code, parse_join};
use super::*;

const MS_KEYS: &[&str] = &[
    "opened_at_ms",
    "closes_at_ms",
    "received_at_ms",
    "issued_at_ms",
    "expires_at_ms",
    "timestamp_ms",
];

fn norm(v: &Value, key: Option<&str>) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let x = if key == Some("approvals") {
                        json!("<ms>")
                    } else {
                        norm(x, Some(k))
                    };
                    (k.clone(), x)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| norm(x, None)).collect()),
        Value::Number(_) if key.is_some_and(|k| MS_KEYS.contains(&k)) => json!("<ms>"),
        Value::String(s)
            if key == Some("code") && s.len() == 6 && s.bytes().all(|b| b.is_ascii_digit()) =>
        {
            json!("<code>")
        }
        Value::String(_) if key == Some("invite_blob_hex") => json!("<hex>"),
        other => other.clone(),
    }
}

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/pairing/testdata")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A receiver layout in `root` with a config pointing the PSK into it.
fn test_env(root: &Path) -> PairingEnv {
    let mut env = PairingEnv::rooted(root);
    env.udp_port = 0;
    env.systemctl = "true".to_string();
    let psk = root.join("mesh/psk.key");
    std::fs::write(
        &env.config_path,
        format!(
            "agent:\n  device_id: gs-synthetic-01\nground_station:\n  mesh:\n    shared_key_path: {}\n",
            psk.display()
        ),
    )
    .unwrap();
    env
}

fn init_mesh_identity(env: &PairingEnv) {
    std::fs::create_dir_all(&env.mesh_dir).unwrap();
    std::fs::write(&env.mesh_id_path, "ados-synthetic\n").unwrap();
    std::fs::write(env.mesh_dir.join("psk.key"), [7u8; 32]).unwrap();
}

async fn rpc_raw(sock: &Path, line: &str) -> Value {
    let mut stream = UnixStream::connect(sock).await.unwrap();
    stream
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).await.unwrap();
    serde_json::from_str(&reply).unwrap()
}

async fn wait_for_pending(service: &PairingService) {
    for _ in 0..100 {
        let snap = service.manager().snapshot().await;
        if snap["pending"].as_array().is_some_and(|p| !p.is_empty()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("join request never reached the pending list");
}

#[tokio::test]
async fn socket_round_trip_matches_golden_replies() {
    let dir = tempfile::tempdir().unwrap();
    let env = test_env(dir.path());
    let events = env.pair_events_path.clone();
    let service = PairingService::new(env.clone());
    let sock = dir.path().join("run/pairing.sock");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn({
        let service = Arc::clone(&service);
        let sock = sock.clone();
        async move {
            serve(&sock, service, async {
                let _ = stop_rx.await;
            })
            .await
        }
    });
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let cases = fixture("replies.json").as_array().unwrap().clone();
    let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let relay_keys = generate_keypair();
    let mut code = String::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        match name {
            "approve_mesh_not_initialized" => {}
            "open_window" => init_mesh_identity(&env),
            "snapshot_pending" => {
                let port = service.manager().bound_udp_port().await.unwrap();
                let join = json!({
                    "type": "join",
                    "device_id": "relay-synthetic-01",
                    "pubkey_hex": hex::encode(relay_keys.public),
                });
                relay
                    .send_to(join.to_string().as_bytes(), ("127.0.0.1", port))
                    .await
                    .unwrap();
                wait_for_pending(&service).await;
            }
            _ => {}
        }
        let reply = match case.get("raw") {
            Some(raw) => rpc_raw(&sock, raw.as_str().unwrap()).await,
            None => rpc_raw(&sock, &case["request"].to_string()).await,
        };
        if name == "malformed_json" {
            assert_eq!(reply["ok"], json!(false));
            assert!(reply["error"]
                .as_str()
                .unwrap()
                .starts_with("malformed request: "));
            continue;
        }
        assert_eq!(norm(&reply, None), case["reply"], "case {name}");
        match name {
            "open_window" => {
                let r = &reply["result"];
                let span =
                    r["closes_at_ms"].as_i64().unwrap() - r["opened_at_ms"].as_i64().unwrap();
                assert_eq!(span, 30_000);
                code = r["code"].as_str().unwrap().to_string();
            }
            "snapshot_open" => {
                assert_eq!(norm(&reply["result"], None), fixture("snapshot_open.json"));
                assert_eq!(reply["result"]["code"], json!(code));
            }
            "snapshot_closed" => {
                assert_eq!(
                    norm(&reply["result"], None),
                    fixture("snapshot_closed.json")
                );
            }
            "snapshot_pending" => {
                assert_eq!(
                    norm(&reply["result"], None),
                    fixture("snapshot_pending.json")
                );
            }
            "snapshot_approved" => {
                assert_eq!(
                    norm(&reply["result"], None),
                    fixture("snapshot_approved.json")
                );
            }
            "approve_ok" => {
                let blob =
                    hex::decode(reply["result"]["invite_blob_hex"].as_str().unwrap()).unwrap();
                let bundle = decrypt_invite(&blob, &relay_keys.secret, &code).unwrap();
                assert_eq!(bundle.mesh_id, "ados-synthetic");
                assert_eq!(bundle.mesh_psk, vec![7u8; 32]);
                // The same invite also went to the relay over UDP.
                let mut buf = vec![0u8; 4096];
                let (n, _) =
                    tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut buf))
                        .await
                        .unwrap()
                        .unwrap();
                assert!(decrypt_invite(&buf[..n], &relay_keys.secret, &code).is_ok());
            }
            "revoke_new" => {
                let revoked = revocations::load_from(&env.revocations_path());
                assert!(revoked.contains("relay-synthetic-02"));
                let id = std::fs::read_to_string(&env.mesh_id_path).unwrap();
                assert_ne!(id.trim(), "ados-synthetic", "the mesh id was rotated");
            }
            _ => {}
        }
    }

    let lines: Vec<Value> = std::fs::read_to_string(&events)
        .unwrap()
        .lines()
        .map(|l| norm(&serde_json::from_str(l).unwrap(), None))
        .collect();
    assert_eq!(Value::Array(lines), fixture("pair_events.json"));

    stop_tx.send(()).unwrap();
    server.await.unwrap().unwrap();
    assert!(!sock.exists());
}

#[tokio::test]
async fn relay_join_round_trip_persists_the_invite() {
    let rx_dir = tempfile::tempdir().unwrap();
    let rx_env = test_env(rx_dir.path());
    init_mesh_identity(&rx_env);
    let receiver = PairingService::new(rx_env.clone());
    let window = receiver.manager().open_window(30).await.unwrap();
    let port = receiver.manager().bound_udp_port().await.unwrap();

    let relay_dir = tempfile::tempdir().unwrap();
    let relay_env = test_env(relay_dir.path());
    let code = window.code.clone();
    let join = tokio::spawn(async move {
        request_join(
            &relay_env,
            &code,
            Some("127.0.0.1"),
            Some(port),
            Duration::from_secs(10),
        )
        .await
        .map(|j| (j, relay_env))
    });
    wait_for_pending(&receiver).await;
    let reply = receiver
        .handle_op(
            "approve",
            json!({"device_id": "gs-synthetic-01"}).as_object().unwrap(),
        )
        .await;
    assert_eq!(reply["ok"], json!(true), "{reply}");

    let (joined, relay_env) = join.await.unwrap().unwrap();
    assert_eq!(joined.mesh_id, "ados-synthetic");
    assert_eq!(joined.rekey.code, window.code);
    assert_eq!(
        std::fs::read_to_string(&relay_env.mesh_id_path).unwrap(),
        "ados-synthetic\n"
    );
    assert_eq!(
        std::fs::read(&relay_env.mesh_psk_path).unwrap(),
        vec![7u8; 32]
    );
    let relays = super::rekey::load_paired_relays(&rx_env.mesh_dir);
    assert_eq!(relays.len(), 1);
    assert_eq!(relays[0].port, joined.rekey.port);
}

#[tokio::test]
async fn join_with_a_bad_code_reports_its_error_code() {
    let dir = tempfile::tempdir().unwrap();
    let service = PairingService::new(test_env(dir.path()));
    let reply = service
        .handle_op("join", json!({"code": "1234567"}).as_object().unwrap())
        .await;
    assert_eq!(
        reply,
        json!({"ok": false, "error": "the join code is six digits", "error_code": "E_BAD_CODE"})
    );
}

#[tokio::test]
async fn relay_applies_only_newer_rekeyed_bundles() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(test_env(dir.path()));
    let relay = generate_keypair();
    let state = RelayRekeyState {
        code: "123456".to_string(),
        issued_at_ms: 1_000,
        port: 0,
        priv_hex: hex::encode(relay.secret.to_bytes()),
    };
    relay_rekey::save_state(&env.mesh_dir, &state).unwrap();
    assert_eq!(relay_rekey::load_state(&env.mesh_dir), Some(state.clone()));
    let mut receiver = RelayRekeyReceiver::new(Arc::clone(&env), state).unwrap();

    let sender = generate_keypair();
    let now = crate::mesh_events::now_ms();
    let mut bundle = InviteBundle {
        mesh_id: "ados-rotated".to_string(),
        mesh_psk: vec![9u8; 32],
        drone_channel: 149,
        wfb_rx_key: Vec::new(),
        receiver_mdns_host: "gs-synthetic".to_string(),
        receiver_mdns_port: 5800,
        issued_at_ms: now,
        expires_at_ms: now + 120_000,
    };
    let blob = encrypt_invite(&bundle, &sender.secret, &relay.public, "123456").unwrap();
    assert!(receiver.handle(&blob, "127.0.0.1").await);
    assert!(
        !receiver.handle(&blob, "127.0.0.1").await,
        "a resend is not newer"
    );
    assert_eq!(receiver.state().issued_at_ms, now);
    assert_eq!(
        std::fs::read_to_string(&env.mesh_id_path).unwrap(),
        "ados-rotated\n"
    );
    bundle.issued_at_ms = now + 1;
    let wrong_code = encrypt_invite(&bundle, &sender.secret, &relay.public, "654321").unwrap();
    assert!(!receiver.handle(&wrong_code, "127.0.0.1").await);
}

#[test]
fn window_code_is_six_digits() {
    for _ in 0..50 {
        let code = new_invite_code();
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
    }
}

#[test]
fn join_datagram_parsing_rejects_malformed_requests() {
    let key = hex::encode([1u8; 32]);
    let ok = json!({"type": "join", "device_id": "relay-synthetic-01", "pubkey_hex": key});
    assert_eq!(
        parse_join(ok.to_string().as_bytes()),
        Some(("relay-synthetic-01".to_string(), [1u8; 32]))
    );
    for bad in [
        json!({"type": "hello", "device_id": "r", "pubkey_hex": key}),
        json!({"type": "join", "device_id": "", "pubkey_hex": key}),
        json!({"type": "join", "device_id": "r", "pubkey_hex": "abcd"}),
        json!({"type": "join", "device_id": "r"}),
    ] {
        assert_eq!(parse_join(bad.to_string().as_bytes()), None, "{bad}");
    }
    assert_eq!(parse_join(b"not json"), None);
}

#[test]
fn arp_lookup_takes_only_complete_entries_on_the_mesh() {
    let dir = tempfile::tempdir().unwrap();
    let arp = dir.path().join("arp");
    std::fs::write(
        &arp,
        "IP address       HW type     Flags       HW address            Mask     Device\n\
         10.0.0.2         0x1         0x2         AA:BB:CC:00:00:02     *        bat0\n\
         10.0.0.3         0x1         0x0         aa:bb:cc:00:00:03     *        bat0\n\
         10.0.0.4         0x1         0x2         aa:bb:cc:00:00:04     *        eth0\n",
    )
    .unwrap();
    use super::rekey::arp_mac;
    assert_eq!(
        arp_mac(&arp, "10.0.0.2", "bat0").as_deref(),
        Some("aa:bb:cc:00:00:02")
    );
    assert_eq!(arp_mac(&arp, "10.0.0.3", "bat0"), None);
    assert_eq!(arp_mac(&arp, "10.0.0.4", "bat0"), None);
}

#[test]
fn malformed_args_are_refused_before_dispatch() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let service = PairingService::new(test_env(dir.path()));
    let reply = rt.block_on(server::reply_for_line(
        &service,
        br#"{"op":"snapshot","args":[1]}"#,
    ));
    assert_eq!(
        reply,
        json!({"ok": false, "error": "malformed request: args must be object"})
    );
    let reply = rt.block_on(service.handle_op("snapshot", &Map::new()));
    assert_eq!(reply, json!({"ok": true, "result": {"open": false}}));
}
