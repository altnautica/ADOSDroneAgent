use std::collections::BTreeSet;

use ados_protocol::frame::encode_frame;
use ados_protocol::framebus::{BoundingBox, Detection, DetectionBatch, Keypoint, TrackState};
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::vision_depth_frame::{DepthFit, FRAME_COLS, FRAME_ROWS, VISION_DEPTH_READ_CAP};
use ados_protocol::vision_subject::{SubjectPrediction, SubjectReleased, SubjectState};
use ados_protocol::vision_subject::{SubjectSources, PREDICTION_HORIZONS_S};
use tokio::sync::broadcast::error::TryRecvError;

use super::*;
use crate::handlers::{
    is_publish_allowed, is_subscribe_allowed, SharedTopics, EVENT_QUEUE_DEPTH,
    VISION_DETECTION_READ_CAP, VISION_SUBJECT_READ_CAP,
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
        (
            VISION_DETECTION_TOPIC,
            VISION_DETECTION_READ_CAP,
            VISION_SUBJECT_READ_CAP,
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
        VISION_DETECTION_READ_CAP,
    ]);
    for pattern in [
        "vision.*",
        "vision.depth.*",
        "vision.subject.*",
        "vision.detection.*",
    ] {
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

#[test]
fn a_release_is_forwarded_and_a_stale_state_is_not() {
    let released = SubjectRecord::Released(SubjectReleased {
        camera_id: "main".into(),
        id: 7,
        stamp_ns: 9,
    });
    let p = payload(&SUBJECT, &released.to_msgpack()).expect("a release");
    assert_eq!(SubjectRecord::from_msgpack(&encoded(&p)).unwrap(), released);
    let edge = SubjectState {
        fix_age_ms: SUBJECT_MAX_FIX_AGE_MS,
        ..subject(9)
    };
    assert!(payload(&SUBJECT, &edge.to_msgpack()).is_some());
    let stale = SubjectState {
        fix_age_ms: SUBJECT_MAX_FIX_AGE_MS + 1,
        ..subject(9)
    };
    assert!(payload(&SUBJECT, &stale.to_msgpack()).is_none());
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

#[tokio::test]
async fn every_receiver_shares_one_event() {
    let bus = EventBus::new();
    let (mut a, mut b) = (bus.subscribe(), bus.subscribe());
    let p = payload(&DEPTH_FRAMES, &frame(1).to_msgpack()).unwrap();
    assert_eq!(publish_host_event(&bus, DEPTH_FRAMES.topic, p), 2);
    let (ea, eb) = (a.recv().await.unwrap(), b.recv().await.unwrap());
    assert!(
        Arc::ptr_eq(&ea, &eb),
        "the 48 KiB frame is not copied per receiver"
    );
}

#[test]
fn a_frame_with_bad_intrinsics_is_not_forwarded() {
    let mut f = frame(1);
    f.fx = f32::NAN;
    assert!(payload(&DEPTH_FRAMES, &f.to_msgpack()).is_none());
}

/// A tracker batch as the engine publishes it in a run, its frame taken
/// now: `people` confirmed person tracks, each with its 512-value appearance
/// embedding, and a mask, keypoints and an appearance model on the first.
fn tracker_batch(people: usize) -> DetectionBatch {
    let embedding = rmpv::Value::Array(vec![rmpv::Value::F32(0.04); 512]);
    let detections = (0..people)
        .map(|i| Detection {
            bbox: Some(BoundingBox {
                x: 626.8 + 40.0 * i as f32,
                y: 233.7,
                width: 34.4,
                height: 81.0,
            }),
            class_label: "person".into(),
            confidence: 0.58,
            track_id: Some(8 + i as u64),
            assoc_confidence: Some(0.86),
            lock_state: None,
            attributes: Some(
                [
                    ("embedding".to_owned(), embedding.clone()),
                    ("embedding_model".to_owned(), "reid_osnet_x0_25_v1".into()),
                ]
                .into(),
            ),
            mask: (i == 0).then(|| vec![[626.8, 233.7], [661.2, 233.7], [644.0, 314.7]]),
            keypoints: (i == 0).then(|| {
                vec![Keypoint {
                    x: 644.0,
                    y: 240.0,
                    confidence: 0.9,
                }]
            }),
            depth: None,
            world_pos: None,
            track_state: Some(TrackState::Confirmed),
            appearance_id: (i == 0).then_some(3),
        })
        .collect();
    DetectionBatch {
        v: VISION_DETECTION_VERSION,
        model_id: "person_yolox_tiny_v1".into(),
        camera_id: "main".into(),
        frame_id: 3558,
        ts_ms: (mono_ns() / 1_000_000) as i64,
        ts_sensor_ms: Some(122_567),
        hfov_deg: Some(87.0),
        mount_pitch_deg: Some(-15.0),
        frame_width: 1280,
        frame_height: 720,
        crop_window: None,
        detections,
    }
}

#[test]
fn a_tracker_batch_is_forwarded_with_its_geometry_and_without_its_embeddings() {
    let p = payload(&DETECTIONS, &tracker_batch(1).to_msgpack().unwrap()).expect("a batch");
    let json = crate::state_sidecar::msgpack_to_json(&p);
    // What the pilot places a person from.
    assert_eq!(json["camera_id"], "main");
    assert_eq!(json["ts_sensor_ms"], 122_567);
    assert_eq!(json["hfov_deg"], 87.0);
    assert_eq!(json["mount_pitch_deg"], -15.0);
    assert_eq!(json["frame_width"], 1280);
    assert_eq!(json["frame_height"], 720);
    let d = &json["detections"][0];
    assert_eq!(d["track_id"], 8);
    assert_eq!(d["track_state"], "confirmed");
    assert_eq!(d["class_label"], "person");
    assert!(d["bbox"]["height"].as_f64().unwrap() > 80.0);
    for dropped in DETECTION_DROPPED {
        assert!(d.get(dropped).is_none(), "{dropped}");
    }
    // Still a batch under the contract.
    let back = DetectionBatch::from_msgpack(&encoded(&p)).unwrap();
    assert_eq!(back.detections[0].bbox, tracker_batch(1).detections[0].bbox);
}

#[test]
fn a_crowd_fits_one_event_once_its_embeddings_are_off() {
    let body = tracker_batch(40).to_msgpack().unwrap();
    assert!(body.len() > EVENT_PAYLOAD_MAX_BYTES, "{}", body.len());
    let p = payload(&DETECTIONS, &body).expect("a crowd's batch");
    assert!(encoded(&p).len() <= EVENT_PAYLOAD_MAX_BYTES);
    assert_eq!(
        DetectionBatch::from_msgpack(&encoded(&p))
            .unwrap()
            .detections
            .len(),
        40
    );
    // Another version, or another stream's record, is not a batch.
    let other = DetectionBatch {
        v: VISION_DETECTION_VERSION + 1,
        ..tracker_batch(1)
    };
    assert!(payload(&DETECTIONS, &other.to_msgpack().unwrap()).is_none());
    assert!(payload(&DETECTIONS, &subject(9).to_msgpack()).is_none());
    assert!(payload(&DETECTIONS, &frame(7).to_msgpack()).is_none());
}

#[test]
fn a_plugins_batch_is_never_forwarded() {
    // What the host makes of a batch a plugin publishes, however it named
    // its model, track ids and all (a plugin may designate).
    for named in ["person_yolox_tiny_v1", "com.example.det/person"] {
        let mut b = tracker_batch(3);
        b.model_id = ados_protocol::vision_rpc::owned_model_id("com.example.det", named);
        assert!(
            payload(&DETECTIONS, &b.to_msgpack().unwrap()).is_none(),
            "{named}"
        );
    }
    // A batch naming no model is not a batch.
    let body = tracker_batch(1).to_msgpack().unwrap();
    let mut v = rmpv::decode::read_value(&mut &body[..]).unwrap();
    if let rmpv::Value::Map(m) = &mut v {
        m.retain(|(k, _)| k.as_str() != Some("model_id"));
    }
    assert!(payload(&DETECTIONS, &encoded(&v)).is_none());
}

#[tokio::test]
async fn the_last_tracker_batch_reaches_a_late_reader() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(DETECTIONS.socket);
    // Last-state, as the engine binds it.
    let (server, _inbound) = IpcBroadcast::bind(&sock, 8, true, None).await.unwrap();
    let body = tracker_batch(1).to_msgpack().unwrap();
    server
        .broadcast(encode_frame(&body, STATE_V2_MAX_FRAME).unwrap().into())
        .await;
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let task = spawn(bus.clone(), dir.path(), DETECTIONS);
    let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a batch event")
        .unwrap();
    assert_eq!(ev.topic, "vision.detection");
    assert_eq!(ev.publisher_plugin_id, HOST_PUBLISHER);
    let got = DetectionBatch::from_msgpack(&encoded(&ev.payload)).unwrap();
    assert_eq!(got.ts_sensor_ms, Some(122_567));
    assert!(got.detections[0].attributes.is_none());
    task.abort();
}

#[tokio::test]
async fn a_stale_batch_replayed_to_a_late_reader_is_not_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(DETECTIONS.socket);
    let (server, _inbound) = IpcBroadcast::bind(&sock, 8, true, None).await.unwrap();
    let stale = DetectionBatch {
        ts_ms: (mono_ns() / 1_000_000) as i64 - DETECTION_MAX_AGE_MS as i64 - 1,
        frame_id: 1,
        ..tracker_batch(1)
    };
    let framed = |b: &DetectionBatch| encode_frame(&b.to_msgpack().unwrap(), STATE_V2_MAX_FRAME);
    server.broadcast(framed(&stale).unwrap().into()).await;
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let task = spawn(bus.clone(), dir.path(), DETECTIONS);
    // The reader connects at once and is handed the stale batch, which it
    // forwards nothing of; the next batch, fresh, is the first on the bus.
    tokio::time::sleep(Duration::from_millis(300)).await;
    server
        .broadcast(framed(&tracker_batch(1)).unwrap().into())
        .await;
    let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the fresh batch")
        .unwrap();
    let got = DetectionBatch::from_msgpack(&encoded(&ev.payload)).unwrap();
    assert_eq!(got.frame_id, 3558, "the stale batch was forwarded");
    task.abort();
}
