//! The MAVLink router link of a `RealHost`: subscribe delivery through the
//! router socket's v2 records, and the down-router send report.

use super::*;
use ados_plugin_host::frame_link::{FrameLink, Wire, LINK_DEPTH};
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::mavlink_socket::encode_record;

/// A router-style socket plus a host whose MAVLink link is connected to it.
async fn router_and_host(tag: &str) -> (IpcBroadcast, RealHost) {
    let mut router_path = std::env::temp_dir();
    router_path.push(format!("ados-rh-mav-{}-{tag}.sock", std::process::id()));
    let _ = std::fs::remove_file(&router_path);
    let (router, _inbound) = IpcBroadcast::bind(&router_path, LINK_DEPTH, false, Some(256))
        .await
        .expect("bind router socket");
    let link = Arc::new(FrameLink::spawn(
        &router_path,
        Wire::MavlinkSocket,
        Box::new(Vec::new),
    ));
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

    // The router fans an ATTITUDE out first, then a HEARTBEAT, each as a v2
    // socket record (receive-time header + frame). Only the HEARTBEAT may
    // reach a HEARTBEAT subscription; delivering the ATTITUDE under that label
    // would hand the plugin another message's bytes.
    let attitude = v2_frame(30);
    let heartbeat = v2_frame(0);
    for (i, frame) in [&attitude, &heartbeat].into_iter().enumerate() {
        router
            .broadcast(encode_record(1_000 + i as u64, frame).unwrap().into())
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
    let host = RealHost::new().with_mavlink(Arc::new(FrameLink::spawn(
        &path,
        Wire::MavlinkSocket,
        Box::new(Vec::new),
    )));
    let h = harness(host, &[PLUGIN_A]);
    let (mut client, token) = hello(&h, PLUGIN_A, &["mavlink.write"]).await;
    let args = map(&[("msg_bytes", Value::Binary(v1_frame(0)))]);
    send(&mut client, &request("mavlink.send", &token, args)).await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    assert_eq!(args_bool(&resp, "sent"), Some(false));
    assert_eq!(args_str(&resp, "reason"), Some("disconnected"));
}
