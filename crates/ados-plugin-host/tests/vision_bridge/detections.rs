//! Detection-batch subscription through the vision bridge: the capability
//! gate, in-order delivery, and the per-camera duplicate-subscribe report.

use super::*;
use ados_protocol::framebus::{BoundingBox, Detection, VISION_DETECTION_VERSION};

fn sample_batch(frame_id: u64) -> DetectionBatch {
    DetectionBatch {
        v: VISION_DETECTION_VERSION,
        model_id: "com.example.det".into(),
        camera_id: "uvc-0".into(),
        frame_id,
        ts_ms: 1_700_000_000_000 + frame_id as i64,
        frame_width: 640,
        frame_height: 480,
        detections: vec![Detection {
            bbox: Some(BoundingBox {
                x: frame_id as f32,
                y: 2.0,
                width: 3.0,
                height: 4.0,
            }),
            class_label: "person".into(),
            confidence: 0.9,
            track_id: Some(frame_id),
            assoc_confidence: None,
            lock_state: None,
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        }],
    }
}

#[tokio::test]
async fn subscribe_detections_is_denied_without_the_subscribe_cap() {
    let h = harness();
    let token = h.issuer.mint(PLUGIN_ID, &caps(&[]), 600).to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    send(
        &mut client,
        &request(methods::SUBSCRIBE_DETECTIONS, &token, Value::Map(vec![])),
    )
    .await;
    let resp = recv(&mut client).await;
    assert_eq!(
        resp.error.as_deref(),
        Some("capability_denied: vision.detection.subscribe")
    );
}

#[tokio::test]
async fn subscribe_detections_delivers_n_batches_in_order() {
    let h = harness();
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["vision.detection.subscribe"]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    send(
        &mut client,
        &request(methods::SUBSCRIBE_DETECTIONS, &token, Value::Map(vec![])),
    )
    .await;
    let resp = recv(&mut client).await;
    assert_eq!(resp.error, None);
    assert_eq!(args_bool(&resp, "subscribed"), Some(true));

    // Let the per-subscription forwarder arm before the first publish.
    tokio::time::sleep(Duration::from_millis(50)).await;

    const N: u64 = 5;
    for i in 0..N {
        h.host.push_detection(&sample_batch(i));
    }

    // The engine fans batches in publish order; the host preserves that order on
    // the single connection writer, so the plugin sees them 0..N in sequence.
    for i in 0..N {
        let frame = tokio::time::timeout(Duration::from_secs(2), recv(&mut client))
            .await
            .expect("vision.deliver_detection within timeout");
        assert_eq!(frame.kind, "event");
        assert_eq!(frame.method, methods::DELIVER_DETECTION);
        assert_eq!(frame.capability, "vision.detection.subscribe");
        let got = args_bytes(&frame, "batch").expect("batch bytes present");
        let batch = DetectionBatch::from_msgpack(&got).unwrap();
        assert_eq!(batch.frame_id, i, "batches must arrive in publish order");
        assert_eq!(batch, sample_batch(i));
    }
}

#[tokio::test]
async fn second_subscribe_detections_to_same_camera_reports_already_subscribed() {
    let h = harness();
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(&["vision.detection.subscribe"]), 600)
        .to_token_string();
    let mut client = connect(&h.path).await;
    send(&mut client, &request("hello", &token, Value::Map(vec![]))).await;
    let _ = recv(&mut client).await; // ready

    let args = Value::Map(vec![(Value::from("camera_id"), Value::from("uvc-0"))]);
    send(
        &mut client,
        &request(methods::SUBSCRIBE_DETECTIONS, &token, args.clone()),
    )
    .await;
    let first = recv(&mut client).await;
    assert_eq!(args_bool(&first, "subscribed"), Some(true));

    send(
        &mut client,
        &request(methods::SUBSCRIBE_DETECTIONS, &token, args),
    )
    .await;
    let second = recv(&mut client).await;
    assert_eq!(args_bool(&second, "already_subscribed"), Some(true));
}
