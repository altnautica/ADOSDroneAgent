//! End-to-end Unix-socket round trips through the plugin host with a `RealHost`.
//!
//! Where `round_trip.rs` proves the wire and gate against the unwired
//! [`NoopHost`], this proves the real facades end to end: a client speaking
//! Contract C handshakes and exercises telemetry, config, camera, MAVLink, and
//! process-spawn against a live [`PluginIpcServer`] backed by [`RealHost`]. The
//! responses are the exact Python wire shapes and error bodies.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ados_plugin_host::frame_link::{FrameLink, LINK_DEPTH};
use ados_plugin_host::realhost::RealHost;
use ados_plugin_host::{EventBus, PluginIpcServer};
use ados_protocol::frame::{
    decode_len, encode_frame, HEADER_SIZE, MAVLINK_MAX_FRAME, PLUGIN_MAX_FRAME,
};
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::mavlink::{serialize_v1, serialize_v2, MavHeader, MavMessage, Message};
use ados_protocol::plugin::{Envelope, TokenIssuer, PROTOCOL_VERSION};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const PLUGIN_A: &str = "com.example.alpha";

fn caps(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// A whole v1 frame of the dialect message `msgid`, default fields, with a
/// real checksum. The pose-inject gate refuses anything a flight controller
/// would not consume whole, so these fixtures have to be genuine frames.
fn v1_frame(msgid: u8) -> Vec<u8> {
    let msg = MavMessage::default_message_from_id(u32::from(msgid)).expect("dialect id");
    serialize_v1(MavHeader::default(), &msg).expect("serialize")
}

/// A whole v2 frame of the dialect message `msgid`, default fields.
fn v2_frame(msgid: u32) -> Vec<u8> {
    let msg = MavMessage::default_message_from_id(msgid).expect("dialect id");
    serialize_v2(MavHeader::default(), &msg).expect("serialize")
}

fn map(entries: &[(&str, Value)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(k, v)| (Value::from(*k), v.clone()))
            .collect(),
    )
}

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

async fn send(stream: &mut UnixStream, env: &Envelope) {
    let frame = env.encode_frame().expect("encode frame");
    stream.write_all(&frame).await.expect("write frame");
    stream.flush().await.expect("flush");
}

async fn recv(stream: &mut UnixStream) -> Envelope {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await.expect("read header");
    let len = decode_len(header, PLUGIN_MAX_FRAME, true).expect("decode len");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.expect("read body");
    Envelope::from_msgpack(&body).expect("decode envelope")
}

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

/// A server backed by a shared `Arc<RealHost>`, with one or more plugin sockets
/// bound.
struct Harness {
    issuer: Arc<TokenIssuer>,
    paths: Vec<(String, PathBuf)>,
    _accepts: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

fn harness(host: RealHost, plugins: &[&str]) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let issuer = Arc::new(TokenIssuer::new(b"real-host-secret".to_vec()));
    let bus = Arc::new(EventBus::new());
    let server = PluginIpcServer::new(dir.path(), issuer.clone(), bus, Arc::new(host));
    let mut paths = Vec::new();
    let mut accepts = Vec::new();
    for p in plugins {
        let (path, accept) = server.serve_plugin(p).expect("bind plugin socket");
        paths.push((p.to_string(), path));
        accepts.push(accept);
    }
    Harness {
        issuer,
        paths,
        _accepts: accepts,
        _dir: dir,
    }
}

fn path_for<'a>(h: &'a Harness, plugin: &str) -> &'a std::path::Path {
    h.paths
        .iter()
        .find(|(p, _)| p == plugin)
        .map(|(_, path)| path.as_path())
        .expect("plugin socket bound")
}

async fn connect(path: &std::path::Path) -> UnixStream {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(path).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to {path:?}");
}

/// Handshake a plugin connection with the given caps and drain the ready frame.
async fn hello(h: &Harness, plugin: &str, granted: &[&str]) -> (UnixStream, String) {
    let token = h.issuer.mint(plugin, &caps(granted), 600).to_token_string();
    let mut client = connect(path_for(h, plugin)).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let ready = recv(&mut client).await;
    assert_eq!(args_bool(&ready, "ready"), Some(true));
    (client, token)
}

#[tokio::test]
async fn telemetry_extend_merges_and_lands_in_the_snapshot() {
    let h = harness(RealHost::new(), &[PLUGIN_A]);
    let (mut client, token) = hello(&h, PLUGIN_A, &["telemetry.extend"]).await;

    let args = map(&[
        ("channel", Value::from("metrics")),
        ("payload", map(&[("rssi", Value::Integer((-42).into()))])),
    ]);
    send(&mut client, &request("telemetry.extend", &token, args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    assert_eq!(args_bool(&resp, "merged"), Some(true));
    assert_eq!(args_str(&resp, "channel"), Some("metrics"));

    // The channel is also in the plugin's state sidecar as `telemetry.<channel>`,
    // which the native front serves to the GCS for the plugin's own UI.
    let sidecar = ados_plugin_host::state_sidecar::sidecar_path(h._dir.path(), PLUGIN_A);
    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar).expect("sidecar written")).unwrap();
    let text = doc.to_string();
    assert!(text.contains("telemetry.metrics"), "sidecar: {text}");
    assert!(text.contains("-42"), "sidecar: {text}");
}

#[tokio::test]
async fn config_set_then_get_round_trips() {
    let h = harness(
        RealHost::new().with_agent_id_lookup(Box::new(|_pid| "agent-1".to_string())),
        &[PLUGIN_A],
    );
    // config.* is ungated at the dispatch level, so no caps needed.
    let (mut client, token) = hello(&h, PLUGIN_A, &[]).await;

    let set_args = map(&[
        ("key", Value::from("threshold")),
        ("value", Value::Integer(7.into())),
    ]);
    send(&mut client, &request("config.set", &token, set_args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    assert_eq!(args_bool(&resp, "set"), Some(true));
    assert_eq!(args_str(&resp, "scope"), Some("drone"));

    let get_args = map(&[("key", Value::from("threshold"))]);
    send(&mut client, &request("config.get", &token, get_args)).await;
    let resp = recv(&mut client).await;
    let value = match &resp.args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some("value"))
            .and_then(|(_, v)| v.as_i64()),
        _ => None,
    };
    assert_eq!(value, Some(7));
}

#[tokio::test]
async fn camera_claim_answers_not_implemented_without_a_capture_pipeline() {
    // Nothing feeds this host camera buffers, so a claim is never acknowledged.
    let h = harness(RealHost::new(), &[PLUGIN_A]);
    let (mut a, token_a) = hello(&h, PLUGIN_A, &["sensor.camera.register"]).await;
    let claim = map(&[("device_path", Value::from("/dev/video0"))]);
    send(&mut a, &request("camera.claim", &token_a, claim)).await;
    let resp = recv(&mut a).await;
    assert_eq!(args_bool(&resp, "claimed"), None);
    assert_eq!(args_str(&resp, "error"), Some("not_implemented"));
}

#[tokio::test]
async fn mavlink_send_with_no_router_returns_not_available() {
    let h = harness(RealHost::new(), &[PLUGIN_A]);
    let (mut client, token) = hello(&h, PLUGIN_A, &["mavlink.write"]).await;

    let args = map(&[("msg_bytes", Value::Binary(v1_frame(0)))]);
    send(&mut client, &request("mavlink.send", &token, args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    assert_eq!(args_str(&resp, "error"), Some("not_available"));
    assert_eq!(args_str(&resp, "method"), Some("mavlink.send"));
}

#[tokio::test]
async fn process_spawn_allowlist_hit_and_miss() {
    let host = RealHost::new().with_runtime_lookup(Box::new(|_pid| {
        let mut allow = BTreeSet::new();
        allow.insert("ffmpeg".to_string());
        Some((PathBuf::from("/opt/ados/plugins/alpha"), allow))
    }));
    let h = harness(host, &[PLUGIN_A]);
    let (mut client, token) = hello(&h, PLUGIN_A, &["process.spawn"]).await;

    // Hit: authorized shape with the install dir.
    let hit = map(&[("basename", Value::from("ffmpeg"))]);
    send(&mut client, &request("process.spawn", &token, hit)).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    assert_eq!(args_bool(&resp, "authorized"), Some(true));
    assert_eq!(
        args_str(&resp, "install_dir"),
        Some("/opt/ados/plugins/alpha")
    );

    // Miss: the exact allowlist_violation body.
    let miss = map(&[("basename", Value::from("rm"))]);
    send(&mut client, &request("process.spawn", &token, miss)).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error.as_deref(), Some("allowlist_violation: rm"));
}

#[tokio::test]
async fn pose_inject_send_is_denied_without_the_estimator_cap() {
    // The pose-inject gate runs inside the handler (after msg_bytes validation),
    // so a granted mavlink.write but missing estimator.pose.inject is denied.
    let h = harness(RealHost::new(), &[PLUGIN_A]);
    let (mut client, token) = hello(&h, PLUGIN_A, &["mavlink.write"]).await;

    // A whole v2 ODOMETRY (331) frame.
    let args = map(&[("msg_bytes", Value::Binary(v2_frame(331)))]);
    send(&mut client, &request("mavlink.send", &token, args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(
        resp.error.as_deref(),
        Some("capability_denied: estimator.pose.inject")
    );
}

#[tokio::test]
async fn a_batched_send_cannot_smuggle_pose_frames_past_the_gate() {
    // Proven end to end through the server, because this is the shape a real
    // plugin would use: one benign frame followed by pose frames in a single
    // send. The router forwards a buffer whole and never parses it, so reading
    // only the first message id let every pose frame through to the flight
    // controller's state estimator on a plugin that held mavlink.write alone.
    let h = harness(RealHost::new(), &[PLUGIN_A]);
    let (mut client, token) = hello(&h, PLUGIN_A, &["mavlink.write"]).await;

    let mut buf = v2_frame(0); // HEARTBEAT
    buf.extend_from_slice(&v2_frame(331)); // ODOMETRY
    let args = map(&[("msg_bytes", Value::Binary(buf))]);
    send(&mut client, &request("mavlink.send", &token, args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(
        resp.error.as_deref(),
        Some("capability_denied: estimator.pose.inject")
    );
}

#[tokio::test]
async fn mavlink_send_validates_msg_bytes_before_the_capability_gate() {
    // Ordering parity: a non-bytes msg_bytes AND a VIO component_id without the
    // mavlink.component.vio cap must fail validation FIRST (msg_bytes must be
    // bytes), not on the capability gate. Proven end to end through the server.
    let h = harness(RealHost::new(), &[PLUGIN_A]);
    let (mut client, token) = hello(&h, PLUGIN_A, &["mavlink.write"]).await;

    let args = map(&[
        ("msg_bytes", Value::Integer(7.into())),
        ("component_id", Value::Integer(197.into())),
    ]);
    send(&mut client, &request("mavlink.send", &token, args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error.as_deref(), Some("msg_bytes must be bytes"));
}

/// A router-style socket plus a host whose MAVLink link is connected to it.
async fn router_and_host(tag: &str) -> (IpcBroadcast, RealHost) {
    let mut router_path = std::env::temp_dir();
    router_path.push(format!("ados-rh-mav-{}-{tag}.sock", std::process::id()));
    let _ = std::fs::remove_file(&router_path);
    let (router, _inbound) = IpcBroadcast::bind(&router_path, LINK_DEPTH, false, Some(256))
        .await
        .expect("bind router socket");
    let link = Arc::new(FrameLink::spawn(&router_path, Box::new(Vec::new)));
    assert!(link.connected_within(Duration::from_secs(2)).await);
    // Let the router register the connection before broadcasting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (router, RealHost::new().with_mavlink(link))
}

#[tokio::test]
async fn mavlink_subscribe_delivers_only_the_subscribed_message() {
    let (router, host) = router_and_host("filter").await;
    let h = harness(host, &[PLUGIN_A]);
    let (mut plugin, token) = hello(&h, PLUGIN_A, &["mavlink.read"]).await;

    // Subscribe to HEARTBEAT; the response carries {subscribed, msg_name}.
    let sub = map(&[("msg_name", Value::from("HEARTBEAT"))]);
    send(&mut plugin, &request("mavlink.subscribe", &token, sub)).await;
    let resp = recv(&mut plugin).await;
    assert_eq!(args_bool(&resp, "subscribed"), Some(true));
    assert_eq!(args_str(&resp, "msg_name"), Some("HEARTBEAT"));
    // Give the forwarder task a moment to arm.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The router fans an ATTITUDE out first, then a HEARTBEAT. Only the
    // HEARTBEAT may reach a HEARTBEAT subscription; delivering the ATTITUDE
    // under that label would hand the plugin another message's bytes.
    let attitude = v2_frame(30);
    let heartbeat = v2_frame(0);
    for frame in [&attitude, &heartbeat] {
        router
            .broadcast(encode_frame(frame, MAVLINK_MAX_FRAME).unwrap().into())
            .await;
    }

    let push = tokio::time::timeout(Duration::from_secs(2), recv(&mut plugin))
        .await
        .expect("push within timeout");
    assert_eq!(push.kind, "event");
    assert_eq!(push.method, "mavlink.deliver");
    assert_eq!(push.capability, "mavlink.read");
    assert_eq!(args_str(&push, "msg_name"), Some("HEARTBEAT"));
    let frame_field = match &push.args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some("frame"))
            .and_then(|(_, v)| match v {
                Value::Binary(b) => Some(b.clone()),
                _ => None,
            }),
        _ => None,
    };
    assert_eq!(frame_field.as_deref(), Some(&heartbeat[..]));
}

#[tokio::test]
async fn mavlink_subscribe_refuses_a_name_the_dialect_does_not_define() {
    let (_router, host) = router_and_host("unknown").await;
    let h = harness(host, &[PLUGIN_A]);
    let (mut plugin, token) = hello(&h, PLUGIN_A, &["mavlink.read"]).await;
    let sub = map(&[("msg_name", Value::from("NOT_A_MESSAGE"))]);
    send(&mut plugin, &request("mavlink.subscribe", &token, sub)).await;
    let resp = recv(&mut plugin).await;
    assert_eq!(
        resp.error.as_deref(),
        Some("unknown MAVLink message name: NOT_A_MESSAGE")
    );
}

#[tokio::test]
async fn mavlink_send_reports_a_router_that_is_down() {
    // A link with no router behind it: the send must say it was not sent.
    let mut path = std::env::temp_dir();
    path.push(format!("ados-rh-mav-{}-down.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let host = RealHost::new().with_mavlink(Arc::new(FrameLink::spawn(&path, Box::new(Vec::new))));
    let h = harness(host, &[PLUGIN_A]);
    let (mut client, token) = hello(&h, PLUGIN_A, &["mavlink.write"]).await;
    let args = map(&[("msg_bytes", Value::Binary(v1_frame(0)))]);
    send(&mut client, &request("mavlink.send", &token, args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    assert_eq!(args_bool(&resp, "sent"), Some(false));
    assert_eq!(args_str(&resp, "reason"), Some("disconnected"));
}

#[tokio::test]
async fn config_persists_across_a_reconnect() {
    // release_plugin does not clear the config store, so a value set on one
    // connection is still readable after the plugin reconnects.
    let h = harness(RealHost::new(), &[PLUGIN_A]);

    {
        let (mut client, token) = hello(&h, PLUGIN_A, &[]).await;
        let set_args = map(&[("key", Value::from("k")), ("value", Value::from("v"))]);
        send(&mut client, &request("config.set", &token, set_args)).await;
        let _ = recv(&mut client).await;
        // Drop the connection; the accept task runs release_plugin.
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut client, token) = hello(&h, PLUGIN_A, &[]).await;
    let get_args = map(&[("key", Value::from("k"))]);
    send(&mut client, &request("config.get", &token, get_args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(args_str(&resp, "value"), Some("v"));
}
