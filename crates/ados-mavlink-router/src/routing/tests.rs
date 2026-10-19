use super::table::tests::{ack_to, heartbeat};
use super::*;
use bytes::Bytes;

/// Recv with a short bound: `None` means nothing was delivered.
async fn next(feed: &mut FrameFeed) -> Option<Bytes> {
    tokio::time::timeout(Duration::from_millis(50), feed.recv())
        .await
        .ok()
        .and_then(Result::ok)
        .map(|f| f.bytes)
}

fn fc(frame: &Bytes) -> RxFrame {
    RxFrame::new(1, frame.clone())
}

#[tokio::test]
async fn route_aware_off_is_flat_fanout() {
    let hub = RouteHub::new(false);
    let (tx, _) = broadcast::channel::<RxFrame>(8);
    let (mut a, ep_a) = hub.client_feed("tcp", tx.subscribe());
    let (mut b, _) = hub.client_feed("tcp", tx.subscribe());
    assert!(ep_a.is_none());
    assert!(matches!(a, FrameFeed::Flat(_)));
    let frame = Bytes::from(ack_to(255, 190));
    tx.send(fc(&frame)).unwrap();
    assert_eq!(next(&mut a).await, Some(frame.clone()));
    assert_eq!(next(&mut b).await, Some(frame));
}

#[tokio::test]
async fn addressed_frame_only_to_owner() {
    let hub = RouteHub::new(true);
    let (tx, _) = broadcast::channel::<RxFrame>(8);
    let (mut a, ep_a) = hub.client_feed("tcp", tx.subscribe());
    let (mut b, ep_b) = hub.client_feed("tcp", tx.subscribe());
    hub.ingress(ep_a.as_ref().unwrap(), &heartbeat(255, 190));
    hub.ingress(ep_b.as_ref().unwrap(), &heartbeat(254, 190));
    // Drain the cross-delivered client heartbeats.
    assert!(next(&mut a).await.is_some());
    assert!(next(&mut b).await.is_some());
    let reply = Bytes::from(ack_to(255, 190));
    hub.publish_fc(fc(&reply));
    assert_eq!(next(&mut a).await, Some(reply));
    assert_eq!(next(&mut b).await, None);
}

#[tokio::test]
async fn broadcast_when_target_zero() {
    let hub = RouteHub::new(true);
    let (tx, _) = broadcast::channel::<RxFrame>(8);
    let (mut a, ep_a) = hub.client_feed("tcp", tx.subscribe());
    let (mut b, _) = hub.client_feed("tcp", tx.subscribe());
    hub.ingress(ep_a.as_ref().unwrap(), &heartbeat(255, 190));
    assert_eq!(next(&mut a).await, None);
    assert!(next(&mut b).await.is_some());
    for frame in [ack_to(0, 0), heartbeat(1, 1)] {
        let frame = Bytes::from(frame);
        hub.publish_fc(fc(&frame));
        assert_eq!(next(&mut a).await, Some(frame.clone()));
        assert_eq!(next(&mut b).await, Some(frame));
    }
}

#[tokio::test]
async fn no_echo_to_source() {
    let hub = RouteHub::new(true);
    let (tx, _) = broadcast::channel::<RxFrame>(8);
    let (mut a, ep_a) = hub.client_feed("tcp", tx.subscribe());
    let (mut b, _) = hub.client_feed("udp", tx.subscribe());
    // The IPC socket also carries the cloud relay: neither hears its own.
    let mut ipc = hub.feed(&["ipc", "cloud"], tx.subscribe());
    let cloud = hub.register("cloud");
    let from_a = heartbeat(255, 190);
    hub.ingress(ep_a.as_ref().unwrap(), &from_a);
    assert_eq!(next(&mut a).await, None);
    assert_eq!(next(&mut b).await.as_deref(), Some(from_a.as_slice()));
    assert_eq!(next(&mut ipc).await.as_deref(), Some(from_a.as_slice()));
    let from_cloud = heartbeat(250, 190);
    hub.ingress(&cloud, &from_cloud);
    assert_eq!(next(&mut ipc).await, None);
    assert_eq!(next(&mut a).await.as_deref(), Some(from_cloud.as_slice()));
}

#[test]
fn sidecar_written_on_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(SIDECAR_NAME);
    let hub = RouteHub::new(true);
    let ipc = hub.register("ipc");
    let mut last = None;
    let none = serde_json::Map::new;
    assert!(write_if_changed(&hub, none(), &path, &mut last));
    assert!(!write_if_changed(&hub, none(), &path, &mut last));
    hub.ingress(&ipc, &heartbeat(255, 190));
    assert!(write_if_changed(&hub, none(), &path, &mut last));
    let body: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(body["version"], 1);
    assert_eq!(body["routes"][0]["sysid"], 255);
    assert_eq!(body["routes"][0]["endpoint"], "ipc");
    assert_eq!(body["per_endpoint"]["ipc"]["rx"], 1);
    assert_eq!(body["sensor_rates_hz"], serde_json::json!({}));
    // A change of the requested sensor rates alone rewrites it.
    let mut rates = serde_json::Map::new();
    rates.insert("fc_imu".into(), serde_json::json!(200.0));
    rates.insert("fc_baro".into(), serde_json::json!(10.0));
    assert!(write_if_changed(&hub, rates.clone(), &path, &mut last));
    assert!(!write_if_changed(&hub, rates, &path, &mut last));
    let body: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        body["sensor_rates_hz"],
        serde_json::json!({"fc_imu": 200.0, "fc_baro": 10.0})
    );
}
