use super::demand::drop_oversized_boxes;
use super::*;
use ados_protocol::framebus::{BoundingBox, DetectionHead, ModelKind};

fn engine_with(policy: DetectionPolicy) -> Arc<VisionEngine> {
    VisionEngine::with_tracker_reid(
        Box::new(BoxesBackend),
        4,
        false,
        TrackerConfig::default(),
        false,
        None,
        policy,
    )
}

fn on_demand() -> Arc<VisionEngine> {
    engine_with(DetectionPolicy::default())
}

#[test]
fn a_lease_runs_detection_until_it_expires() {
    let e = on_demand();
    assert!(!e.detection_wanted("belly", 1_000));
    let reply = e.request_detection("belly", "operator", 6_000, 1_000);
    assert!(reply.detecting);
    assert_eq!(reply.until_ms, Some(7_000));
    assert!(e.detection_wanted("belly", 6_999));
    // Only the leased camera.
    assert!(!e.detection_wanted("main", 2_000));
    // Expired: off, and the lease is gone from the status.
    assert!(!e.detection_wanted("belly", 7_000));
    assert!(
        e.detection_status(&["belly".into()], 7_000).cameras["belly"]
            .leases
            .is_empty()
    );
}

#[test]
fn repeating_a_request_renews_the_lease() {
    let e = on_demand();
    e.request_detection("main", "operator", 6_000, 0);
    let renewed = e.request_detection("main", "operator", 6_000, 4_000);
    assert_eq!(renewed.until_ms, Some(10_000));
    assert!(e.detection_wanted("main", 9_000));
    assert!(!e.detection_wanted("main", 10_000));
}

#[test]
fn a_zero_hold_releases_only_that_owners_lease() {
    let e = on_demand();
    e.request_detection("main", "operator", 6_000, 0);
    e.request_detection("main", "com.example.follow", 3_000, 0);
    let after = e.request_detection("main", "operator", 0, 1_000);
    // The follow lease still holds the camera on.
    assert!(after.detecting);
    assert_eq!(after.until_ms, Some(3_000));
    let released = e.request_detection("main", "com.example.follow", 0, 1_000);
    assert!(!released.detecting);
    assert_eq!(released.until_ms, None);
    assert!(!e.detection_wanted("main", 1_000));
}

#[test]
fn always_mode_detects_on_every_camera_without_a_lease() {
    let e = engine_with(DetectionPolicy {
        mode: DetectionDemandMode::Always,
        ..DetectionPolicy::default()
    });
    assert!(e.detection_wanted("main", 0));
    assert!(e.detection_wanted("belly", u64::MAX / 2));
    let reply = e.detection_demand("belly", 0);
    assert!(reply.detecting);
    assert_eq!(reply.until_ms, None);
    let status = e.detection_status(&["main".into()], 0);
    assert_eq!(status.mode, "always");
    assert!(status.cameras["main"].detecting);
}

#[tokio::test]
async fn a_designation_holds_detection_on_its_camera() {
    let e = engine_with(DetectionPolicy {
        designate_hold_ms: 30_000,
        ..DetectionPolicy::default()
    });
    let now = ados_protocol::framebus::mono_ns() / 1_000_000;
    assert!(!e.detection_wanted("main", now));
    e.designate("main", &the_box(10.0, 10.0, 20.0, 40.0)).await;
    assert!(e.detection_wanted("main", now + 1));
    let status = e.detection_status(&[], now + 1);
    let leases = &status.cameras["main"].leases;
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].owner, DESIGNATE_OWNER);
    assert!(!e.detection_wanted("main", now + 31_000));
}

#[test]
fn the_status_lists_every_camera_with_its_leases() {
    let e = on_demand();
    e.request_detection("belly", "operator", 6_000, 1_000);
    let status = e.detection_status(&["main".into(), "belly".into()], 2_000);
    assert_eq!(status.mode, "on_demand");
    assert!(!status.cameras["main"].detecting);
    assert!(status.cameras["main"].leases.is_empty());
    let belly = &status.cameras["belly"];
    assert!(belly.detecting);
    assert_eq!(belly.leases[0].owner, "operator");
    assert_eq!(belly.leases[0].remaining_ms, 5_000);
}

fn the_box(x: f32, y: f32, width: f32, height: f32) -> Detection {
    Detection {
        bbox: Some(BoundingBox {
            x,
            y,
            width,
            height,
        }),
        class_label: "person".into(),
        confidence: 0.6,
        track_id: None,
        assoc_confidence: None,
        lock_state: None,
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    }
}

#[test]
fn boxes_over_the_area_cutoff_are_dropped() {
    // 640x480 frame: a near-full-frame box, a quarter-frame box, a box half
    // outside the frame (clipped to a quarter), and a box-less detection.
    let mut dets = vec![
        the_box(10.0, 10.0, 600.0, 450.0),
        the_box(0.0, 0.0, 320.0, 240.0),
        the_box(480.0, 0.0, 320.0, 480.0),
        Detection {
            bbox: None,
            ..the_box(0.0, 0.0, 1.0, 1.0)
        },
    ];
    drop_oversized_boxes(&mut dets, 640, 480, 0.5);
    assert_eq!(dets.len(), 3);
    assert!(dets.iter().all(|d| d.bbox.is_none_or(|b| b.width < 600.0)));

    // A cutoff of 1 keeps everything; an unknown frame size filters nothing.
    let mut all = vec![the_box(0.0, 0.0, 640.0, 480.0)];
    drop_oversized_boxes(&mut all, 640, 480, 1.0);
    drop_oversized_boxes(&mut all, 0, 0, 0.5);
    assert_eq!(all.len(), 1);
}

/// A detector that reports one near-full-frame box and one person-sized box.
struct BoxesBackend;
struct BoxesDetector;

impl LoadedModel for BoxesDetector {
    fn infer(&self, _f: &[u8], w: u32, h: u32, _fmt: FrameFormat) -> Result<Vec<Detection>> {
        let (w, h) = (w as f32, h as f32);
        Ok(vec![
            the_box(0.02 * w, 0.05 * h, 0.95 * w, 0.92 * h),
            the_box(0.4 * w, 0.3 * h, 0.1 * w, 0.3 * h),
        ])
    }
}

impl VisionBackend for BoxesBackend {
    fn load(&self, _meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(Box::new(BoxesDetector))
    }
    fn name(&self) -> &str {
        "boxes"
    }
}

#[tokio::test]
async fn the_engine_drops_oversized_boxes_before_publishing() {
    let e = on_demand();
    e.register_model(ModelMetadata {
        id: "det".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 64,
        input_height: 48,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["person".into()],
        model_path: Some("/boxes".into()),
        head: DetectionHead::Yolo8,
    })
    .await
    .unwrap();
    let desc = FrameDescriptor {
        v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "belly".into(),
        frame_id: 1,
        ts_ns: 1,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: ados_protocol::framebus::TIMEBASE_HOST,
        width: 640,
        height: 480,
        format: FrameFormat::Rgb24,
        seq: 1,
        slot: 0,
        shm_name: String::new(),
        byte_len: 640 * 480 * 3,
    };
    let batch = e
        .infer_and_publish("det", &desc, &vec![0u8; 640 * 480 * 3])
        .await
        .unwrap();
    assert_eq!(batch.detections.len(), 1);
    assert!((batch.detections[0].bbox.unwrap().width - 64.0).abs() < 1e-3);
}
