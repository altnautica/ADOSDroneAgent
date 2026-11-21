use std::collections::BTreeSet;

use ados_protocol::frame::encode_frame;
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::vision_depth_frame::{DepthFit, FRAME_COLS, FRAME_ROWS, VISION_DEPTH_READ_CAP};
use tokio::sync::broadcast::error::TryRecvError;

use super::*;
use crate::handlers::{is_subscribe_allowed, SharedTopics, EVENT_QUEUE_DEPTH};
use crate::vehicle_events::HOST_PUBLISHER;

fn frame(stamp_ns: u64) -> DepthFrame {
    let n = usize::from(FRAME_COLS) * usize::from(FRAME_ROWS);
    DepthFrame {
        camera_id: "main".into(),
        stamp_ns,
        w: FRAME_COLS,
        h: FRAME_ROWS,
        fx: 110.0,
        fy: 83.0,
        cx: 64.0,
        cy: 48.0,
        pitch_down_deg: 15.0,
        depth_mm: vec![4000; n],
        sigma_mm: vec![200; n],
        fit: DepthFit {
            scale: 0.2,
            shift: 0.05,
            inliers: 12,
            residual_m: 0.3,
            held_ms: 0,
        },
    }
}

fn caps(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|c| c.to_string()).collect()
}

#[test]
fn the_topic_is_read_only_with_its_capability() {
    let shared = SharedTopics::default();
    let id = "com.example.mapper";
    let topic = VISION_DEPTH_FRAME_TOPIC;
    assert!(!is_subscribe_allowed(
        id,
        topic,
        &caps(&["event.subscribe"]),
        &shared
    ));
    assert!(!is_subscribe_allowed(
        id,
        topic,
        &caps(&[VISION_DEPTH_READ_CAP]),
        &shared
    ));
    assert!(is_subscribe_allowed(
        id,
        topic,
        &caps(&["event.subscribe", VISION_DEPTH_READ_CAP]),
        &shared
    ));
    // A pattern reaching the topic grants nothing.
    let all = caps(&["event.subscribe", VISION_DEPTH_READ_CAP]);
    assert!(!is_subscribe_allowed(id, "vision.*", &all, &shared));
    assert!(!is_subscribe_allowed(id, "vision.depth.*", &all, &shared));
    // And no plugin may publish it.
    assert!(!crate::handlers::is_publish_allowed(
        id,
        topic,
        &caps(&["event.publish", VISION_DEPTH_READ_CAP]),
        &shared
    ));
}

#[test]
fn only_valid_frames_become_payloads() {
    let body = frame(7).to_msgpack();
    let payload = frame_payload(&body).expect("a frame");
    let mut back = Vec::new();
    rmpv::encode::write_value(&mut back, &payload).unwrap();
    assert_eq!(DepthFrame::from_msgpack(&back).unwrap(), frame(7));
    assert!(frame_payload(b"\x81\xa1v\x01").is_none());
    assert!(frame_payload(&[0xc1]).is_none());
}

#[tokio::test]
async fn engine_frames_reach_the_bus_and_a_slow_subscriber_blocks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(SOCKET_NAME);
    let (server, _inbound) = IpcBroadcast::bind(sock.to_str().unwrap(), 16, false, None)
        .await
        .unwrap();
    let bus = Arc::new(EventBus::new());
    // Subscribed before the reader starts; never drained.
    let mut slow = bus.subscribe();
    let task = spawn(bus.clone(), sock.clone());
    let mut live = bus.subscribe();

    // Publish more frames than the bus holds, draining only `live`.
    let total = EVENT_QUEUE_DEPTH as u64 + 8;
    let mut seen = 0u64;
    for i in 0..total {
        let bytes = encode_frame(&frame(i).to_msgpack(), FRAME_MAX_BYTES).unwrap();
        // The reader may still be connecting: the first frame is resent
        // until one arrives.
        'send: loop {
            server.broadcast(bytes.clone().into()).await;
            loop {
                match tokio::time::timeout(Duration::from_millis(500), live.recv()).await {
                    Ok(Ok(ev)) => {
                        assert_eq!(ev.topic, VISION_DEPTH_FRAME_TOPIC);
                        assert_eq!(ev.publisher_plugin_id, HOST_PUBLISHER);
                        let mut raw = Vec::new();
                        rmpv::encode::write_value(&mut raw, &ev.payload).unwrap();
                        if DepthFrame::from_msgpack(&raw).unwrap().stamp_ns == i {
                            seen += 1;
                            break 'send;
                        }
                    }
                    Ok(Err(e)) => panic!("live subscriber: {e}"),
                    Err(_) if i == 0 => continue 'send,
                    Err(_) => panic!("frame {i} never reached the bus"),
                }
            }
        }
    }
    assert_eq!(seen, total);
    // The undrained subscriber was lagged, not waited on: it lost the oldest
    // frames and still holds the newest.
    assert!(matches!(slow.try_recv(), Err(TryRecvError::Lagged(_))));
    let mut newest = None;
    while let Ok(ev) = slow.try_recv() {
        newest = Some(ev);
    }
    let mut raw = Vec::new();
    rmpv::encode::write_value(&mut raw, &newest.expect("recent frames").payload).unwrap();
    assert_eq!(DepthFrame::from_msgpack(&raw).unwrap().stamp_ns, total - 1);
    task.abort();
}
