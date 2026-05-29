//! End-to-end Unix-socket round trips through the plugin host.
//!
//! A client built only from `ados-protocol` types (Envelope + frame) handshakes
//! and dispatches against a live [`PluginIpcServer`]. This proves a client
//! speaking Contract C round-trips against the Rust host with no wire
//! re-implementation on either side.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use ados_plugin_host::{EventBus, NoopHost, PluginIpcServer};
use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{Envelope, TokenIssuer, PROTOCOL_VERSION};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const PLUGIN_ID: &str = "com.example.demo";

fn caps(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Build a request envelope with the given method, args, and token string.
fn request(method: &str, token: &str, args: Value) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "request".to_string(),
        method: method.to_string(),
        capability: String::new(),
        args,
        request_id: format!("req-{method}"),
        token: token.to_string(),
        error: None,
    }
}

/// Write one envelope to the stream as a Contract C frame.
async fn send(stream: &mut UnixStream, env: &Envelope) {
    let frame = env.encode_frame().expect("encode frame");
    stream.write_all(&frame).await.expect("write frame");
    stream.flush().await.expect("flush");
}

/// Read one Contract C frame from the stream into an envelope.
async fn recv(stream: &mut UnixStream) -> Envelope {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await.expect("read header");
    let len = decode_len(header, PLUGIN_MAX_FRAME, true).expect("decode len");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.expect("read body");
    Envelope::from_msgpack(&body).expect("decode envelope")
}

/// Stand up a server with a fresh issuer + bus, bind the plugin socket, and
/// return the issuer, the bound path, and the kept-alive accept task.
struct Harness {
    issuer: Arc<TokenIssuer>,
    path: std::path::PathBuf,
    _accept: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let issuer = Arc::new(TokenIssuer::new(b"round-trip-secret".to_vec()));
    let bus = Arc::new(EventBus::new());
    let host = Arc::new(NoopHost);
    let server = PluginIpcServer::new(dir.path(), issuer.clone(), bus, host);
    let (path, accept) = server.serve_plugin(PLUGIN_ID).expect("bind plugin socket");
    Harness {
        issuer,
        path,
        _accept: accept,
        _dir: dir,
    }
}

async fn connect(path: &std::path::Path) -> UnixStream {
    // Bounded retry: the accept loop binds before serve_plugin returns, so a
    // single connect normally succeeds; retry a few times for slow CI.
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(path).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to {path:?}");
}

/// Read a string field from a response args map.
fn args_str<'a>(env: &'a Envelope, key: &str) -> Option<&'a str> {
    match &env.args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_str()),
        _ => None,
    }
}

fn args_bool(env: &Envelope, key: &str) -> Option<bool> {
    match &env.args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_bool()),
        _ => None,
    }
}

#[tokio::test]
async fn valid_token_handshakes_and_pings() {
    let h = harness();
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["event.publish"]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;

    // hello -> {ready: true}
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.kind, "response");
    assert_eq!(resp.error, None);
    assert_eq!(args_bool(&resp, "ready"), Some(true));

    // ping -> {pong: true, plugin_id: <id>}
    send(&mut client, &request("ping", &token, Value::Map(vec![]))).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    assert_eq!(args_bool(&resp, "pong"), Some(true));
    assert_eq!(args_str(&resp, "plugin_id"), Some(PLUGIN_ID));
}

#[tokio::test]
async fn wrong_plugin_id_token_is_rejected() {
    let h = harness();
    // Token minted for a different plugin id than the socket serves.
    let token = h
        .issuer
        .mint("com.example.other", &caps(&[]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let resp = recv(&mut client).await;
    let err = resp.error.expect("expected an error");
    assert!(err.contains("does not match socket"), "got: {err}");
}

#[tokio::test]
async fn expired_token_is_rejected_at_handshake() {
    let h = harness();
    // Mint a token that expired in the past (issued at 0, ttl 1 -> exp 1).
    let token = h
        .issuer
        .mint_at(PLUGIN_ID, &caps(&[]), 1, 0, "sess")
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let resp = recv(&mut client).await;
    let err = resp.error.expect("expected an error");
    assert!(err.contains("capability token invalid"), "got: {err}");
    assert!(err.contains("expired"), "got: {err}");
}

#[tokio::test]
async fn bad_hmac_token_is_rejected_at_handshake() {
    let h = harness();
    // Mint with a different secret so the HMAC will not verify against the
    // server's issuer.
    let other = TokenIssuer::new(b"a-different-secret".to_vec());
    let token = other.mint(PLUGIN_ID, &caps(&[]), 600).to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let resp = recv(&mut client).await;
    let err = resp.error.expect("expected an error");
    assert!(err.contains("capability token invalid"), "got: {err}");
    assert!(err.contains("HMAC"), "got: {err}");
}

#[tokio::test]
async fn granted_cap_runs_the_handler() {
    let h = harness();
    // mission.read requires the mission.read cap; granted -> the NoopHost runs
    // and returns the not_implemented shape (handler ran, host is unwired).
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["mission.read"]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    send(
        &mut client,
        &request("mission.read", &token, Value::Map(vec![])),
    )
    .await;
    let resp = recv(&mut client).await;
    assert_eq!(
        resp.error, None,
        "granted cap must not produce a gate error"
    );
    assert_eq!(args_str(&resp, "error"), Some("not_implemented"));
    assert_eq!(args_str(&resp, "method"), Some("mission.read"));
}

#[tokio::test]
async fn ungranted_cap_is_denied_with_exact_string() {
    let h = harness();
    // No caps granted; mission.read must be denied with the exact body.
    let token = h.issuer.mint(PLUGIN_ID, &caps(&[]), 600).to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    send(
        &mut client,
        &request("mission.read", &token, Value::Map(vec![])),
    )
    .await;
    let resp = recv(&mut client).await;
    assert_eq!(
        resp.error.as_deref(),
        Some("capability_denied: mission.read")
    );
}

#[tokio::test]
async fn unknown_method_is_rejected_with_exact_string() {
    let h = harness();
    let token = h.issuer.mint(PLUGIN_ID, &caps(&[]), 600).to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    send(
        &mut client,
        &request("does.not.exist", &token, Value::Map(vec![])),
    )
    .await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error.as_deref(), Some("unknown method does.not.exist"));
}

#[tokio::test]
async fn event_publish_and_subscribe_round_trip() {
    let h = harness();
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["event.publish", "event.subscribe"]), 600)
        .to_token_string();

    // One client subscribes to its own namespace; another publishes to it.
    let mut sub = connect(&h.path).await;
    send(&mut sub, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut sub).await; // ready

    let subscribe_args = Value::Map(vec![(
        Value::from("topic"),
        Value::from(format!("plugin.{PLUGIN_ID}.*").as_str()),
    )]);
    send(
        &mut sub,
        &request("event.subscribe", &token, subscribe_args),
    )
    .await;
    let resp = recv(&mut sub).await;
    assert_eq!(args_bool(&resp, "subscribed"), Some(true));

    // Publish on the same connection (same plugin id namespace).
    let publish_args = Value::Map(vec![
        (
            Value::from("topic"),
            Value::from(format!("plugin.{PLUGIN_ID}.metric").as_str()),
        ),
        (
            Value::from("payload"),
            Value::Map(vec![(Value::from("v"), Value::Integer(7.into()))]),
        ),
    ]);
    send(&mut sub, &request("event.publish", &token, publish_args)).await;

    // The subscriber receives, in some order, the publish response and the
    // event.deliver push. Drain a couple of frames and assert we saw the
    // delivered event for our topic.
    let mut saw_deliver = false;
    for _ in 0..3 {
        let frame = tokio::time::timeout(Duration::from_secs(2), recv(&mut sub))
            .await
            .expect("frame within timeout");
        if frame.kind == "event" && frame.method == "event.deliver" {
            assert_eq!(
                args_str(&frame, "topic"),
                Some(format!("plugin.{PLUGIN_ID}.metric").as_str())
            );
            assert_eq!(args_str(&frame, "publisher"), Some(PLUGIN_ID));
            saw_deliver = true;
            break;
        }
    }
    assert!(
        saw_deliver,
        "expected an event.deliver push for the subscribed topic"
    );
}

#[tokio::test]
async fn publish_to_reserved_namespace_is_denied() {
    let h = harness();
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["event.publish"]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    let publish_args = Value::Map(vec![(Value::from("topic"), Value::from("mavlink.x"))]);
    send(&mut client, &request("event.publish", &token, publish_args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(
        resp.error.as_deref(),
        Some("publish not permitted on topic mavlink.x")
    );
}
