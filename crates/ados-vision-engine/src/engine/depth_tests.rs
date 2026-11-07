use super::*;
use crate::backend::MockBackend;
use crate::ring::FrameTiming;
use ados_protocol::framebus::{DetectionHead, ModelKind};
use ados_protocol::vision_depth::{DepthRecord, ScaleSource};
use ados_protocol::vision_rpc::DemandKind;
use std::sync::atomic::Ordering;

fn depth_meta() -> ModelMetadata {
    ModelMetadata {
        id: "depth_midas_v21_small".into(),
        kind: ModelKind::Depth,
        execution: ModelExecution::EngineRun,
        input_width: 256,
        input_height: 256,
        input_format: FrameFormat::Rgb24,
        output_classes: Vec::new(),
        model_path: None,
        head: DetectionHead::Yolo8,
    }
}

async fn engine_with_depth() -> Arc<VisionEngine> {
    let e = VisionEngine::new(Box::new(MockBackend), 4);
    let (_, loaded) = e.register_model(depth_meta()).await.unwrap();
    assert!(loaded);
    e.configure_depth(DepthSetup {
        model_id: Some("depth_midas_v21_small".into()),
        unavailable: None,
        interval_ms: 500,
        geometries: HashMap::new(),
    });
    e
}

async fn frame_on(e: &VisionEngine, camera: &str) -> (FrameDescriptor, Vec<u8>) {
    let (w, h) = (64u32, 36u32);
    let data = vec![128u8; (w * h * 3) as usize];
    let desc = e
        .publish_frame(
            camera,
            1,
            FrameTiming::host(42),
            w,
            h,
            FrameFormat::Rgb24,
            &data,
        )
        .await
        .unwrap();
    (desc, data)
}

#[test]
fn depth_and_surfaces_leases_are_their_own_and_run_only_with_a_model() {
    let e = VisionEngine::new(Box::new(MockBackend), 4);
    let reply = e.request_demand(DemandKind::Depth, "main", "operator", 6_000, 1_000);
    assert_eq!(reply.kind, DemandKind::Depth);
    assert_eq!(reply.until_ms, Some(7_000));
    // Leased, but no depth model: not running.
    assert!(!reply.detecting);
    assert_eq!(e.depth_wanted("main", 2_000), (true, false));
    assert!(!e.detection_wanted("main", 2_000));
    e.request_demand(DemandKind::Surfaces, "main", "operator", 6_000, 1_000);
    assert_eq!(e.depth_wanted("main", 2_000), (true, true));
    // Releasing depth leaves surfaces.
    e.request_demand(DemandKind::Depth, "main", "operator", 0, 2_000);
    assert_eq!(e.depth_wanted("main", 2_000), (false, true));
    let status = e.depth_status(&[], 2_000);
    assert_eq!(status.state, "no_model");
    let main = &status.cameras["main"];
    assert!(!main.depth && main.surfaces);
    assert_eq!(main.leases[0].owner, "surfaces:operator");
    // The detector status never lists a depth-only camera.
    assert!(e.detection_status(&[], 2_000).cameras.is_empty());
}

#[tokio::test]
async fn a_leased_camera_gets_a_grid_and_zones_and_an_unleased_one_nothing() {
    let e = engine_with_depth().await;
    let (desc, data) = frame_on(&e, "depthtest-grid").await;
    let now = 10_000;
    // No lease: nothing runs.
    assert!(e.run_depth(&desc, &data, now).await.unwrap().is_none());
    let reply = e.request_demand(DemandKind::Depth, "depthtest-grid", "operator", 6_000, now);
    assert!(reply.detecting);
    e.request_demand(
        DemandKind::Surfaces,
        "depthtest-grid",
        "operator",
        6_000,
        now,
    );
    let mut rx = e.subscribe_depth();
    let out = e
        .run_depth(&desc, &data, now)
        .await
        .unwrap()
        .expect("a run");
    // No ranging on a test host: relative bands.
    assert_eq!(out.grid.scale_source, ScaleSource::None);
    assert_eq!(out.grid.mono_ns, 42);
    match rx.recv().await.unwrap() {
        DepthRecord::Depth(g) => assert_eq!(g.camera_id, "depthtest-grid"),
        other => panic!("expected the grid first, got {other:?}"),
    }
    match rx.recv().await.unwrap() {
        DepthRecord::Surfaces(s) => assert!(s.zones.is_empty()),
        other => panic!("expected surfaces, got {other:?}"),
    }
    let status = e.depth_status(&["depthtest-grid".into()], now);
    assert_eq!(status.state, "ready");
    assert_eq!(status.interval_ms, 500);
    let cam = &status.cameras["depthtest-grid"];
    assert_eq!(cam.runs, 1);
    assert_eq!(cam.scale_source.as_deref(), Some("none"));
}

#[tokio::test]
async fn a_depth_run_stands_aside_while_other_inference_waits() {
    let e = engine_with_depth().await;
    let (desc, data) = frame_on(&e, "depthtest-prio").await;
    e.request_demand(DemandKind::Depth, "depthtest-prio", "operator", 6_000, 0);
    // A detector queued for the accelerator for longer than the depth wait.
    e.priority_waiters.fetch_add(1, Ordering::AcqRel);
    let t0 = std::time::Instant::now();
    assert!(e.run_depth(&desc, &data, 1).await.unwrap().is_none());
    assert!(t0.elapsed() >= DEPTH_PERMIT_WAIT);
    e.priority_waiters.fetch_sub(1, Ordering::AcqRel);
    // Nothing waiting: it runs.
    assert!(e.run_depth(&desc, &data, 2).await.unwrap().is_some());
}
