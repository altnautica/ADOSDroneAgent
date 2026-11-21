use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::follow_beacon::{FOLLOW_BEACON_READ_CAP, FOLLOW_BEACON_TOPIC};
use ados_protocol::ipc::PeerClass;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::Instant;

use super::publish::*;
use super::*;
use crate::handlers::{is_subscribe_allowed, may_deliver, SharedTopics};

const HOST: PublishPeer = PublishPeer {
    allowed: true,
    uid: 0,
};

fn beacon_line(payload: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(
        &json!({"v": 1, "op": "publish", "topic": FOLLOW_BEACON_TOPIC, "payload": payload}),
    )
    .unwrap();
    line.push(b'\n');
    line
}

fn caps(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|c| c.to_string()).collect()
}

/// Serve one connection as `peer` over a socket pair; returns the client end
/// and the serving task.
fn serve_as(
    bus: &Arc<EventBus>,
    gate: &Arc<PublishGate>,
    peer: PublishPeer,
) -> (UnixStream, tokio::task::JoinHandle<()>) {
    let (client, server) = UnixStream::pair().unwrap();
    let subscribers = Arc::new(tokio::sync::Semaphore::new(1));
    let task = tokio::spawn(serve_connection(
        bus.clone(),
        gate.clone(),
        subscribers,
        peer,
        server,
    ));
    (client, task)
}

async fn next_line(reader: &mut tokio::io::Lines<BufReader<UnixStream>>) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(2), reader.next_line())
        .await
        .expect("a line or a close within the deadline")
        .unwrap()
}

#[test]
fn only_a_host_service_off_the_plugin_plane_may_publish() {
    let (legacy, plugins) = (Some(995), Some(980));
    assert!(publish_peer_allowed(
        PeerClass::Service,
        0,
        &[0],
        legacy,
        plugins
    ));
    // A per-plugin uid is never a service peer.
    assert!(!publish_peer_allowed(
        PeerClass::Operator,
        1201,
        &[980],
        legacy,
        plugins
    ));
    assert!(!publish_peer_allowed(
        PeerClass::Reader,
        1201,
        &[1201],
        legacy,
        plugins
    ));
    // A service-class peer on the plugin plane is refused too.
    assert!(!publish_peer_allowed(
        PeerClass::Service,
        995,
        &[995],
        legacy,
        plugins
    ));
    assert!(!publish_peer_allowed(
        PeerClass::Service,
        0,
        &[980],
        legacy,
        plugins
    ));
    // The plugin group as a supplementary group counts as much as a primary one.
    assert!(!publish_peer_allowed(
        PeerClass::Service,
        0,
        &[0, 27, 980],
        legacy,
        plugins
    ));
}

#[tokio::test]
async fn the_connected_peer_is_read_with_its_supplementary_groups() {
    let (a, _b) = UnixStream::pair().unwrap();
    let me = nix::unistd::geteuid().as_raw();
    let groups: Vec<u32> = nix::unistd::getgroups()
        .unwrap()
        .into_iter()
        .map(|g| g.as_raw())
        .collect();
    let peer = publish_peer(&a, PeerClass::Service, PluginPlane::default());
    assert!(peer.allowed);
    assert_eq!(peer.uid, me);
    // With one of this process's supplementary groups taken as the plugin group,
    // the same peer is on the plugin plane.
    if let Some(&g) = groups
        .iter()
        .find(|&&g| g != nix::unistd::getegid().as_raw())
    {
        let plane = PluginPlane {
            user_uid: None,
            gid: Some(g),
        };
        assert!(!publish_peer(&a, PeerClass::Service, plane).allowed);
    }
}

#[tokio::test]
async fn subscribers_at_their_cap_leave_a_slot_for_the_publisher() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let (path, task) = serve_event_export(bus.clone(), dir.path().join("plugin-host")).unwrap();

    let mut held = Vec::new();
    for _ in 0..8 {
        let mut s = UnixStream::connect(&path).await.unwrap();
        s.write_all(b"{\"v\":1,\"op\":\"subscribe\",\"topics\":[\"a\"]}\n")
            .await
            .unwrap();
        let mut lines = BufReader::new(s).lines();
        assert!(next_line(&mut lines).await.unwrap().contains("\"ok\""));
        held.push(lines);
    }
    let mut extra = UnixStream::connect(&path).await.unwrap();
    extra
        .write_all(b"{\"v\":1,\"op\":\"subscribe\",\"topics\":[\"a\"]}\n")
        .await
        .unwrap();
    let mut extra = BufReader::new(extra).lines();
    assert!(next_line(&mut extra)
        .await
        .unwrap()
        .contains("subscriber cap"));
    assert_eq!(next_line(&mut extra).await, None, "refused and closed");
    drop(extra);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut publisher = UnixStream::connect(&path).await.unwrap();
    publisher
        .write_all(&beacon_line(json!({"lat": 1.0})))
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.topic, FOLLOW_BEACON_TOPIC);

    task.abort();
}

#[tokio::test]
async fn a_plugin_uid_publish_is_refused_counted_and_disconnected() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let gate = Arc::new(PublishGate::default());
    let plugin = PublishPeer {
        allowed: false,
        uid: 1201,
    };
    let (mut client, task) = serve_as(&bus, &gate, plugin);
    client
        .write_all(&beacon_line(json!({"lat": 1.0})))
        .await
        .unwrap();
    let mut lines = BufReader::new(client).lines();
    let reply: serde_json::Value =
        serde_json::from_str(&next_line(&mut lines).await.unwrap()).unwrap();
    assert!(reply["error"].as_str().unwrap().contains("peer"));
    assert_eq!(
        next_line(&mut lines).await,
        None,
        "the host closes a refused peer"
    );
    task.await.unwrap();
    assert_eq!(gate.refused_peer.load(Ordering::Relaxed), 1);
    assert_eq!(gate.published.load(Ordering::Relaxed), 0);
    assert!(rx.try_recv().is_err(), "nothing reached the bus");
}

#[test]
fn an_unknown_topic_is_refused() {
    let gate = PublishGate::default();
    let line = json!({"v": 1, "op": "publish", "topic": "plugin.navigation.state", "payload": {}});
    let got = gate.admit(HOST, line.to_string().as_bytes(), Instant::now());
    assert_eq!(got.unwrap_err(), Refusal::Topic);
    let vehicle = json!({"v": 1, "op": "publish", "topic": "vehicle.armed", "payload": {}});
    let got = gate.admit(HOST, vehicle.to_string().as_bytes(), Instant::now());
    assert_eq!(got.unwrap_err(), Refusal::Topic);
}

#[test]
fn a_malformed_line_is_refused() {
    let gate = PublishGate::default();
    let now = Instant::now();
    for bad in [
        json!({"v": 2, "op": "publish", "topic": FOLLOW_BEACON_TOPIC, "payload": {}}),
        json!({"v": 1, "op": "publish", "topic": FOLLOW_BEACON_TOPIC, "payload": [1, 2]}),
        json!({"v": 1, "op": "publish", "topic": FOLLOW_BEACON_TOPIC}),
    ] {
        let got = gate.admit(HOST, bad.to_string().as_bytes(), now);
        assert_eq!(got.unwrap_err(), Refusal::Malformed, "{bad}");
    }
}

#[tokio::test]
async fn an_oversize_payload_is_refused_and_an_over_long_line_closes() {
    let gate = PublishGate::default();
    let big = "x".repeat(crate::handlers::EVENT_PAYLOAD_MAX_BYTES);
    let line = beacon_line(json!({ "pad": big }));
    assert!(
        line.len() < PUBLISH_LINE_MAX,
        "under the line cap, over the payload cap"
    );
    let got = gate.admit(HOST, &line[..line.len() - 1], Instant::now());
    assert_eq!(got.unwrap_err(), Refusal::Oversize);

    let bus = Arc::new(EventBus::new());
    let gate = Arc::new(PublishGate::default());
    let (mut client, task) = serve_as(&bus, &gate, HOST);
    client
        .write_all(&beacon_line(json!({"lat": 1.0})))
        .await
        .unwrap();
    let huge = beacon_line(json!({ "pad": "y".repeat(PUBLISH_LINE_MAX) }));
    client.write_all(&huge).await.unwrap();
    let mut lines = BufReader::new(client).lines();
    let reply: serde_json::Value =
        serde_json::from_str(&next_line(&mut lines).await.unwrap()).unwrap();
    assert!(reply["error"].as_str().unwrap().contains("size"));
    assert_eq!(next_line(&mut lines).await, None);
    task.await.unwrap();
    assert_eq!(gate.published.load(Ordering::Relaxed), 1);
    assert_eq!(gate.refused_oversize.load(Ordering::Relaxed), 1);
}

#[test]
fn the_rate_is_per_publisher_uid() {
    let gate = PublishGate::default();
    let line = beacon_line(json!({"lat": 1.0}));
    let line = &line[..line.len() - 1];
    let t0 = Instant::now();
    for _ in 0..PUBLISH_BURST as usize {
        assert!(gate.admit(HOST, line, t0).is_ok());
    }
    assert_eq!(gate.admit(HOST, line, t0).unwrap_err(), Refusal::Rate);
    // Another service uid has its own bucket.
    let other = PublishPeer {
        allowed: true,
        uid: 7,
    };
    assert!(gate.admit(other, line, t0).is_ok());
    // A tenth of a second refills two events at 20/s.
    let t1 = t0 + Duration::from_millis(100);
    assert!(gate.admit(HOST, line, t1).is_ok());
    assert!(gate.admit(HOST, line, t1).is_ok());
    assert_eq!(gate.admit(HOST, line, t1).unwrap_err(), Refusal::Rate);
}

#[tokio::test]
async fn a_host_publish_reaches_a_subscriber_holding_the_read_capability_only() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let gate = Arc::new(PublishGate::default());
    let (path, task) =
        serve_event_export_with(bus.clone(), dir.path().join("plugin-host"), gate.clone()).unwrap();

    // The test process is the serving uid: a host-service peer.
    let mut stream = UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(&beacon_line(
            json!({"lat": 47.4, "lon": 8.5, "horiz_acc_m": 3.0}),
        ))
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.topic, FOLLOW_BEACON_TOPIC);
    assert_eq!(
        event.publisher_plugin_id,
        crate::vehicle_events::HOST_PUBLISHER
    );
    let payload = crate::state_sidecar::msgpack_to_json(&event.payload);
    assert_eq!(payload["lat"], 47.4);
    assert_eq!(gate.published.load(Ordering::Relaxed), 1);

    let shared = SharedTopics::default();
    let reader = "com.example.follow";
    let granted = caps(&["event.subscribe", FOLLOW_BEACON_READ_CAP]);
    assert!(is_subscribe_allowed(
        reader,
        FOLLOW_BEACON_TOPIC,
        &granted,
        &shared
    ));
    assert!(may_deliver(reader, &event, &shared));
    let without = caps(&["event.subscribe", "telemetry.read"]);
    assert!(!is_subscribe_allowed(
        reader,
        FOLLOW_BEACON_TOPIC,
        &without,
        &shared
    ));
    assert!(!is_subscribe_allowed(
        reader,
        "plugin.follow.*",
        &granted,
        &shared
    ));
    let no_bus = caps(&[FOLLOW_BEACON_READ_CAP]);
    assert!(!is_subscribe_allowed(
        reader,
        FOLLOW_BEACON_TOPIC,
        &no_bus,
        &shared
    ));

    task.abort();
}
