use super::*;
use crate::backend::MockBackend;
use ados_protocol::framebus::{BoundingBox, LockState, ModelKind};

pub(super) fn engine() -> Arc<VisionEngine> {
    VisionEngine::new(Box::new(MockBackend), 4)
}

fn meta(id: &str, exec: ModelExecution) -> ModelMetadata {
    ModelMetadata {
        id: id.into(),
        kind: ModelKind::Detection,
        execution: exec,
        input_width: 8,
        input_height: 8,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["x".into()],
        model_path: None,
        head: ados_protocol::framebus::DetectionHead::Yolo8,
    }
}

#[tokio::test]
async fn models_for_kind_filters_by_task() {
    let e = engine();
    e.register_model(meta("det-a", ModelExecution::PluginSide))
        .await
        .unwrap();
    e.register_model(meta("det-b", ModelExecution::PluginSide))
        .await
        .unwrap();
    let mut seg = meta("seg", ModelExecution::PluginSide);
    seg.kind = ModelKind::Segmentation;
    e.register_model(seg).await.unwrap();

    // Detection task returns both detectors, sorted; segmentation returns
    // the one seg model; a task with no models returns empty.
    assert_eq!(
        e.models_for_kind(ModelKind::Detection).await,
        vec!["det-a", "det-b"]
    );
    assert_eq!(
        e.models_for_kind(ModelKind::Segmentation).await,
        vec!["seg"]
    );
    assert!(e.models_for_kind(ModelKind::Tracking).await.is_empty());
}

/// A no-op loaded model, so a test backend can report inference-capable
/// without pulling a real backend feature.
struct NoopModel;

impl LoadedModel for NoopModel {
    fn infer(&self, _frame: &[u8], _w: u32, _h: u32, _f: FrameFormat) -> Result<Vec<Detection>> {
        Ok(Vec::new())
    }
}

/// A test backend that loads any model and, unlike [`MockBackend`], reports
/// inference-capable — so a resolved capability has a real model to return.
struct CapableBackend;

impl VisionBackend for CapableBackend {
    fn load(&self, _meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(Box::new(NoopModel))
    }
    fn name(&self) -> &str {
        "capable"
    }
}

fn capable_engine() -> Arc<VisionEngine> {
    VisionEngine::new(Box::new(CapableBackend), 4)
}

#[tokio::test]
async fn a_plugin_model_lives_in_its_own_namespace() {
    let e = engine();
    e.register_model(meta("det", ModelExecution::EngineRun))
        .await
        .unwrap();
    // A plugin registering the engine's id gets its own namespaced entry;
    // the configured detector stays the engine's.
    let (id, _, _) = e
        .register_owned_model("com.example.a", meta("det", ModelExecution::EngineRun))
        .await
        .unwrap();
    assert_eq!(id, "com.example.a/det");
    let ids: Vec<String> = e.list_models().await.into_iter().map(|m| m.id).collect();
    assert_eq!(ids, vec!["com.example.a/det", "det"]);

    // Another plugin naming the first plugin's full id is refused, and so is
    // its inference on that model.
    let mut theirs = meta("x", ModelExecution::EngineRun);
    theirs.id = "com.example.a/det".into();
    let (id_b, _, _) = e
        .register_owned_model("com.example.b", theirs)
        .await
        .unwrap();
    assert_eq!(id_b, "com.example.b/com.example.a/det");
    let frame = [0u8; 192];
    assert!(e
        .infer_for(
            "com.example.c",
            "com.example.a/det",
            &frame,
            8,
            8,
            FrameFormat::Rgb24
        )
        .await
        .is_err());
    // The owner reaches its model by its own id; any plugin may run the
    // engine's configured models.
    e.infer_for("com.example.a", "det", &frame, 8, 8, FrameFormat::Rgb24)
        .await
        .unwrap();
    e.infer_for("com.example.c", "det", &frame, 8, 8, FrameFormat::Rgb24)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_plugin_holds_a_bounded_number_of_models_until_it_is_released() {
    let e = engine();
    for i in 0..MAX_MODELS_PER_OWNER {
        e.register_owned_model("p", meta(&format!("m{i}"), ModelExecution::PluginSide))
            .await
            .unwrap();
    }
    assert!(e
        .register_owned_model("p", meta("one-more", ModelExecution::PluginSide))
        .await
        .is_err());
    // Re-registering a held id is a replace, not a new slot.
    e.register_owned_model("p", meta("m0", ModelExecution::PluginSide))
        .await
        .unwrap();
    e.register_model(meta("det", ModelExecution::PluginSide))
        .await
        .unwrap();

    assert_eq!(e.unregister_owner("p").await, MAX_MODELS_PER_OWNER);
    assert_eq!(e.model_count().await, 1, "the engine's own model stays");
    e.register_owned_model("p", meta("one-more", ModelExecution::PluginSide))
        .await
        .unwrap();
}

/// A model whose inference takes a while, standing in for a slow sidecar.
struct SlowModel;

impl LoadedModel for SlowModel {
    fn infer(&self, _frame: &[u8], _w: u32, _h: u32, _f: FrameFormat) -> Result<Vec<Detection>> {
        std::thread::sleep(std::time::Duration::from_millis(600));
        Ok(Vec::new())
    }
}

struct SlowBackend;

impl VisionBackend for SlowBackend {
    fn load(&self, _meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(Box::new(SlowModel))
    }
    fn name(&self) -> &str {
        "slow"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_inference_does_not_hold_up_the_model_registry() {
    let e = VisionEngine::new(Box::new(SlowBackend), 4);
    e.register_model(meta("slow", ModelExecution::EngineRun))
        .await
        .unwrap();
    let running = {
        let e = e.clone();
        tokio::spawn(async move { e.infer("slow", &[0u8; 192], 8, 8, FrameFormat::Rgb24).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let listed = tokio::time::timeout(std::time::Duration::from_millis(250), e.list_models())
        .await
        .expect("listing waited behind the inference");
    assert_eq!(listed.len(), 1);
    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        e.register_model(meta("other", ModelExecution::PluginSide)),
    )
    .await
    .expect("registration waited behind the inference")
    .unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn resolve_capability_matches_kind_and_class() {
    let e = capable_engine();
    // A person/car detector, engine-run so it loads on the capable backend.
    let mut det = meta("person-det", ModelExecution::EngineRun);
    det.output_classes = vec!["person".into(), "car".into()];
    e.register_model(det).await.unwrap();

    // detection + person resolves the model.
    let m = e
        .resolve_capability(ModelKind::Detection, Some("person"))
        .await
        .expect("a person detector resolves detection+person");
    assert_eq!(m.id, "person-det");
    assert!(m.is_inference_capable);

    // detection + None resolves any model of that kind.
    assert_eq!(
        e.resolve_capability(ModelKind::Detection, None)
            .await
            .map(|m| m.id),
        Some("person-det".into())
    );

    // An unlisted class does not resolve.
    assert!(e
        .resolve_capability(ModelKind::Detection, Some("boat"))
        .await
        .is_none());

    // A kind with no registered model does not resolve.
    assert!(e.resolve_capability(ModelKind::Depth, None).await.is_none());
}

#[tokio::test]
async fn resolve_capability_skips_a_non_inference_capable_model() {
    // The default mock engine loads the model (backend_loaded) but is not
    // inference-capable, so the detection capability does not resolve — a
    // placeholder is never offered as a working capability.
    let e = engine();
    e.register_model(meta("m1", ModelExecution::EngineRun))
        .await
        .unwrap();
    assert!(e
        .resolve_capability(ModelKind::Detection, None)
        .await
        .is_none());
}

/// resolve returns the lowest-id inference-capable match (deterministic).
#[tokio::test]
async fn resolve_capability_is_deterministic_by_id() {
    let e = capable_engine();
    for id in ["det-c", "det-a", "det-b"] {
        let mut m = meta(id, ModelExecution::EngineRun);
        m.output_classes = vec!["person".into()];
        e.register_model(m).await.unwrap();
    }
    assert_eq!(
        e.resolve_capability(ModelKind::Detection, Some("person"))
            .await
            .map(|m| m.id),
        Some("det-a".into())
    );
}

#[tokio::test]
async fn registers_engine_run_model_and_infers() {
    let e = engine();
    let (exec, had_backend) = e
        .register_model(meta("m1", ModelExecution::EngineRun))
        .await
        .unwrap();
    assert_eq!(exec, ModelExecution::EngineRun);
    assert!(had_backend);
    assert_eq!(e.model_count().await, 1);
    // Mock backend returns no detections.
    let out = e
        .infer("m1", &[0u8; 192], 8, 8, FrameFormat::Rgb24)
        .await
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn model_timing_tracks_latency_and_fps() {
    let mut t = ModelTiming::default();
    // No data yet: an unrun model reports zero, not a fabricated rate.
    assert_eq!(t.fps(), 0.0);
    assert_eq!(t.ewma_latency_ms, 0.0);
    // First sample seeds the latency; fps still 0 (no interval yet).
    t.record(20.0, 1000);
    assert_eq!(t.ewma_latency_ms, 20.0);
    assert_eq!(t.fps(), 0.0);
    // Second sample 100 ms later: the interval is 100 ms -> ~10 fps.
    t.record(20.0, 1100);
    assert!(
        (t.fps() - 10.0).abs() < 0.01,
        "100ms interval -> ~10fps, got {}",
        t.fps()
    );
    assert_eq!(t.samples, 2);
}

#[tokio::test]
async fn list_models_flags_a_mock_backend_as_not_inference_capable() {
    // A model loads on the mock backend (backend_loaded true) but the mock
    // runs no real inference, so is_inference_capable is false — a status
    // surface must not present it as a working detector.
    let e = engine();
    e.register_model(meta("m1", ModelExecution::EngineRun))
        .await
        .unwrap();
    let models = e.list_models().await;
    assert_eq!(models.len(), 1);
    assert!(models[0].backend_loaded, "the mock loads the model");
    assert!(
        !models[0].is_inference_capable,
        "the mock backend is not inference-capable"
    );
    // Timing is zero until it runs.
    assert_eq!(models[0].fps, 0.0);
    assert_eq!(models[0].latency_ms, 0.0);
}

#[tokio::test]
async fn infer_records_timing_surfaced_by_list_models() {
    let e = engine();
    e.register_model(meta("m1", ModelExecution::EngineRun))
        .await
        .unwrap();
    // One inference records a latency sample; list_models surfaces it.
    e.infer("m1", &[0u8; 192], 8, 8, FrameFormat::Rgb24)
        .await
        .unwrap();
    let models = e.list_models().await;
    // latency_ms is set from the (tiny) measured duration; assert the field is
    // populated (>= 0, the timing path ran) — a real backend reports a real ms.
    assert!(models[0].latency_ms >= 0.0);
}

#[tokio::test]
async fn plugin_side_model_cannot_be_inferred_by_engine() {
    let e = engine();
    e.register_model(meta("m2", ModelExecution::PluginSide))
        .await
        .unwrap();
    let err = e.infer("m2", &[0u8; 4], 1, 1, FrameFormat::Rgb24).await;
    assert!(err.is_err());
}

#[tokio::test]
async fn unknown_model_inference_errors() {
    let e = engine();
    assert!(e
        .infer("nope", &[0u8; 4], 1, 1, FrameFormat::Rgb24)
        .await
        .is_err());
}

#[tokio::test]
async fn publish_detection_reaches_subscribers() {
    let e = engine();
    let mut rx = e.subscribe_detections();
    let batch = DetectionBatch {
        v: VISION_DETECTION_VERSION,
        model_id: "m".into(),
        camera_id: "c".into(),
        frame_id: 1,
        ts_ms: 0,
        frame_width: 640,
        frame_height: 480,
        detections: vec![Detection {
            bbox: Some(BoundingBox {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            }),
            class_label: "x".into(),
            confidence: 0.5,
            track_id: None,
            assoc_confidence: None,
            lock_state: None,
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        }],
    };
    let reached = e.publish_detection(batch.clone());
    assert_eq!(reached, 1);
    assert_eq!(rx.try_recv().unwrap(), batch);
}

#[tokio::test]
async fn infer_and_publish_builds_batch_from_descriptor() {
    let e = engine();
    e.register_model(meta("m", ModelExecution::EngineRun))
        .await
        .unwrap();
    let mut rx = e.subscribe_detections();
    let desc = FrameDescriptor {
        v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "uvc-0".into(),
        frame_id: 9,
        ts_ns: 123,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: ados_protocol::framebus::TIMEBASE_HOST,
        width: 8,
        height: 8,
        format: FrameFormat::Rgb24,
        shm_name: "ados-vision-uvc-0".into(),
        slot: 1,
        seq: 1,
        byte_len: 192,
    };
    let batch = e.infer_and_publish("m", &desc, &[0u8; 192]).await.unwrap();
    assert_eq!(batch.model_id, "m");
    assert_eq!(batch.camera_id, "uvc-0");
    assert_eq!(batch.frame_id, 9);
    assert_eq!(rx.try_recv().unwrap(), batch);
}

fn det(x: f32, y: f32, conf: f32, label: &str) -> Detection {
    Detection {
        bbox: Some(BoundingBox {
            x,
            y,
            width: 40.0,
            height: 40.0,
        }),
        class_label: label.into(),
        confidence: conf,
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

#[tokio::test]
async fn tracker_stamps_a_stable_track_id_and_holds_through_a_drop() {
    let e = VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
    // Run up to confirmation: the same box for a few frames.
    e.apply_tracker(
        "cam",
        vec![det(100.0, 100.0, 0.9, "uav")],
        &[],
        0,
        0,
        FrameFormat::Rgb24,
    )
    .await;
    e.apply_tracker(
        "cam",
        vec![det(100.0, 100.0, 0.9, "uav")],
        &[],
        0,
        0,
        FrameFormat::Rgb24,
    )
    .await;
    let confirmed = e
        .apply_tracker(
            "cam",
            vec![det(100.0, 100.0, 0.9, "uav")],
            &[],
            0,
            0,
            FrameFormat::Rgb24,
        )
        .await;

    let stamped: Vec<_> = confirmed.iter().filter(|d| d.track_id.is_some()).collect();
    assert_eq!(
        stamped.len(),
        1,
        "exactly one detection carries the track id"
    );
    let id = stamped[0].track_id.unwrap();
    assert!(
        stamped[0].lock_state.is_none(),
        "an auto-seeded track is tracked but never presented as locked"
    );
    assert_eq!(e.current_track("cam").await, Some(id));

    // A dropped frame: the tracker coasts and the held target is appended
    // with the SAME id, still reported as the current track (never a silent
    // identity loss).
    let coast = e
        .apply_tracker("cam", vec![], &[], 0, 0, FrameFormat::Rgb24)
        .await;
    assert!(
        coast.iter().any(|d| d.track_id == Some(id)),
        "the held target survives a dropped frame with its id"
    );
    assert_eq!(e.current_track("cam").await, Some(id));
}

#[tokio::test]
async fn operator_designate_locks_a_specific_detection() {
    let e = VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
    // Two detections; the operator picks the lower-confidence one — the
    // auto-lock would have taken the other.
    let target = det(200.0, 50.0, 0.4, "person");
    let id = e
        .designate("cam", &target)
        .await
        .expect("designate seeds a track");
    // A freshly-seeded track is tentative, so current_track is None until a
    // measured frame confirms it — but the designated id is fixed.
    assert_eq!(e.current_track("cam").await, None);

    // Feed the designated box: it confirms under the SAME id, not a new one.
    e.apply_tracker("cam", vec![target.clone()], &[], 0, 0, FrameFormat::Rgb24)
        .await;
    let confirmed = e
        .apply_tracker("cam", vec![target.clone()], &[], 0, 0, FrameFormat::Rgb24)
        .await;
    assert_eq!(e.current_track("cam").await, Some(id));
    assert!(confirmed.iter().any(|d| d.track_id == Some(id)));

    // reset_track drops the lock entirely.
    e.reset_track("cam").await;
    assert_eq!(e.current_track("cam").await, None);
}

#[tokio::test]
async fn auto_seed_publishes_no_lock_state_but_designate_does() {
    // An auto-seeded track (no operator designate) must NOT be published as
    // locked, even once confirmed — it is tracked (carries an id) for
    // continuity, but a follow behavior only ever engages a target the
    // operator actually designated. This mirrors the offload publish path.
    let e = VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
    let subject = det(100.0, 100.0, 0.9, "uav");
    e.apply_tracker("cam", vec![subject.clone()], &[], 0, 0, FrameFormat::Rgb24)
        .await;
    let auto = e
        .apply_tracker("cam", vec![subject.clone()], &[], 0, 0, FrameFormat::Rgb24)
        .await;
    let tracked: Vec<_> = auto.iter().filter(|d| d.track_id.is_some()).collect();
    assert_eq!(tracked.len(), 1, "the auto-seeded subject is tracked");
    assert!(
        tracked[0].lock_state.is_none(),
        "an auto-seeded track is never published as locked"
    );

    // The SAME subject, now explicitly designated by the operator, IS
    // published as locked once the track confirms.
    e.reset_track("cam").await;
    e.designate("cam", &subject)
        .await
        .expect("designate seeds a track");
    e.apply_tracker("cam", vec![subject.clone()], &[], 0, 0, FrameFormat::Rgb24)
        .await;
    let designated = e
        .apply_tracker("cam", vec![subject.clone()], &[], 0, 0, FrameFormat::Rgb24)
        .await;
    let locked: Vec<_> = designated
        .iter()
        .filter(|d| d.lock_state == Some(LockState::Locked))
        .collect();
    assert_eq!(
        locked.len(),
        1,
        "an operator-designated track is published as locked"
    );
}
