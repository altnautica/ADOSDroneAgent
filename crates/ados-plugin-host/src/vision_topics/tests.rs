use std::collections::BTreeSet;

use ados_protocol::frame::encode_frame;
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::vision_depth_frame::{DepthFit, FRAME_COLS, FRAME_ROWS, VISION_DEPTH_READ_CAP};
use ados_protocol::vision_subject::SubjectPrediction;
use ados_protocol::vision_subject::{SubjectSources, PREDICTION_HORIZONS_S};
use tokio::sync::broadcast::error::TryRecvError;

use super::*;
use crate::handlers::{
    is_publish_allowed, is_subscribe_allowed, SharedTopics, EVENT_QUEUE_DEPTH,
    VISION_SUBJECT_READ_CAP,
};
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

fn subject(stamp_ns: u64) -> SubjectState {
    SubjectState {
        camera_id: "main".into(),
        id: 7,
        stamp_ns,
        fix_age_ms: 0,
        pos_ned: [8.0, -2.5, 0.0],
        cov: [0.2, 0.0, 0.0, 0.0, 0.2, 0.0, 0.0, 0.0, 0.05],
        vel_ned: [1.4, 0.0, 0.0],
        mode_probs: [0.7, 0.2, 0.1],
        height_m: 1.74,
        predictions: PREDICTION_HORIZONS_S
            .iter()
            .map(|&dt_s| SubjectPrediction {
                dt_s,
                pos_ned: [8.0 + 1.4 * dt_s, -2.5, 0.0],
                cov: [dt_s; 9],
            })
            .collect(),
        sources: SubjectSources {
            box_height: true,
            ..Default::default()
        },
    }
}

fn caps(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|c| c.to_string()).collect()
}

fn encoded(payload: &rmpv::Value) -> Vec<u8> {
    let mut raw = Vec::new();
    rmpv::encode::write_value(&mut raw, payload).unwrap();
    raw
}

#[test]
fn each_topic_is_read_only_with_its_own_capability() {
    let shared = SharedTopics::default();
    let id = "com.example.mapper";
    for (topic, cap, other) in [
        (
            VISION_DEPTH_FRAME_TOPIC,
            VISION_DEPTH_READ_CAP,
            VISION_SUBJECT_READ_CAP,
        ),
        (
            VISION_SUBJECT_TOPIC,
            VISION_SUBJECT_READ_CAP,
            VISION_DEPTH_READ_CAP,
        ),
    ] {
        let allowed = |c: &[&str]| is_subscribe_allowed(id, topic, &caps(c), &shared);
        assert!(!allowed(&["event.subscribe"]), "{topic}");
        assert!(!allowed(&[cap]), "{topic}");
        assert!(!allowed(&["event.subscribe", other]), "{topic}");
        assert!(allowed(&["event.subscribe", cap]), "{topic}");
        // No plugin may publish it.
        assert!(!is_publish_allowed(
            id,
            topic,
            &caps(&["event.publish", cap]),
            &shared
        ));
    }
    // A pattern reaching a topic grants nothing.
    let all = caps(&[
        "event.subscribe",
        VISION_DEPTH_READ_CAP,
        VISION_SUBJECT_READ_CAP,
    ]);
    for pattern in ["vision.*", "vision.depth.*", "vision.subject.*"] {
        assert!(!is_subscribe_allowed(id, pattern, &all, &shared));
    }
}

#[test]
fn only_valid_records_become_payloads() {
    let p = payload(&DEPTH_FRAMES, &frame(7).to_msgpack()).expect("a frame");
    assert_eq!(DepthFrame::from_msgpack(&encoded(&p)).unwrap(), frame(7));
    let p = payload(&SUBJECT, &subject(9).to_msgpack()).expect("a subject");
    assert_eq!(
        SubjectState::from_msgpack(&encoded(&p)).unwrap(),
        subject(9)
    );
    // A record of one stream is not valid on the other.
    assert!(payload(&SUBJECT, &frame(7).to_msgpack()).is_none());
    assert!(payload(&DEPTH_FRAMES, &subject(9).to_msgpack()).is_none());
    assert!(payload(&DEPTH_FRAMES, &[0xc1]).is_none());
}

#[tokio::test]
async fn engine_frames_reach_the_bus_and_a_slow_subscriber_blocks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(DEPTH_FRAMES.socket);
    let (server, _inbound) = IpcBroadcast::bind(&sock, 16, false, None).await.unwrap();
    let bus = Arc::new(EventBus::new());
    // Subscribed before the reader starts; never drained.
    let mut slow = bus.subscribe();
    let task = spawn(bus.clone(), dir.path(), DEPTH_FRAMES);
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
                        let got = DepthFrame::from_msgpack(&encoded(&ev.payload)).unwrap();
                        if got.stamp_ns == i {
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
    let newest = newest.expect("recent frames");
    assert_eq!(
        DepthFrame::from_msgpack(&encoded(&newest.payload))
            .unwrap()
            .stamp_ns,
        total - 1
    );
    task.abort();
}

#[tokio::test]
async fn the_last_subject_state_reaches_a_late_reader() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(SUBJECT.socket);
    // Last-state, as the engine binds it.
    let (server, _inbound) = IpcBroadcast::bind(&sock, 8, true, None).await.unwrap();
    let bytes = encode_frame(&subject(5).to_msgpack(), SUBJECT_MAX_BYTES).unwrap();
    server.broadcast(bytes.into()).await;
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let task = spawn(bus.clone(), dir.path(), SUBJECT);
    let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a subject event")
        .unwrap();
    assert_eq!(ev.topic, VISION_SUBJECT_TOPIC);
    assert_eq!(ev.publisher_plugin_id, HOST_PUBLISHER);
    assert_eq!(
        SubjectState::from_msgpack(&encoded(&ev.payload)).unwrap(),
        subject(5)
    );
    task.abort();
}
