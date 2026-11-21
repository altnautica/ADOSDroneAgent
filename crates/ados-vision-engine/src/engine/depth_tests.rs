use super::*;
use crate::backend::{synthetic_depth, DenseMap, MockBackend};
use crate::depth::feed::{Ranging, Scan};
use crate::depth::{geometry::CameraGeometry, DepthProcessor};
use crate::ring::FrameTiming;
use ados_protocol::framebus::{DetectionHead, ModelKind};
use ados_protocol::vision_depth::{decode_range_code, DepthRecord, ScaleSource};
use ados_protocol::vision_depth_frame::{FRAME_COLS, FRAME_ROWS};
use ados_protocol::vision_rpc::DemandKind;
use std::sync::atomic::Ordering;

const MODEL_ID: &str = "depth_anything_v2_small";

fn depth_meta() -> ModelMetadata {
    ModelMetadata {
        id: MODEL_ID.into(),
        kind: ModelKind::Depth,
        execution: ModelExecution::EngineRun,
        input_width: 252,
        input_height: 196,
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
        model_id: Some(MODEL_ID.into()),
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

/// A planar scan that agrees with `map` (a synthetic scene) under
/// `1 / Z = x`, seen by a default camera on 16:9 frames: the post 1.25 m
/// ahead-left, ground and background far.
fn scan_for(map: &DenseMap) -> Scan {
    let proj = CameraGeometry::default().projector(16.0 / 9.0);
    let ranges_m = (0..360)
        .map(|k| {
            let a = (k as f32).to_radians();
            match proj.project([a.cos(), a.sin(), 0.0]) {
                Some((_, u, v)) if (0.0..1.0).contains(&u) => {
                    let ray = proj.ray(u, v);
                    ray[0].hypot(ray[1]) / map.at(u, v)
                }
                _ => 0.0,
            }
        })
        .collect();
    Scan {
        angle_start_rad: 0.0,
        angle_increment_rad: 1f32.to_radians(),
        ranges_m,
    }
}

#[tokio::test]
async fn a_ranged_depth_lease_gets_paced_metric_frames_that_hold_their_fit_with_growing_sigma() {
    let e = engine_with_depth().await;
    let (desc, data) = frame_on(&e, "depthtest-metric").await;
    let mut rx = e.subscribe_depth_frames();
    e.request_demand(DemandKind::Depth, "depthtest-metric", "nav", 60_000, 0);
    // No ranging yet: a grid, but no metric frame.
    e.run_depth(&desc, &data, 1_000)
        .await
        .unwrap()
        .expect("a run");
    assert!(rx.try_recv().is_err());
    // The mock model's map is the synthetic scene at 64x64.
    e.range_feed()
        .set_scan(scan_for(&synthetic_depth(64, 64)), 2_000);
    e.run_depth(&desc, &data, 2_000).await.unwrap();
    let fresh = rx.try_recv().expect("a metric frame");
    assert_eq!(fresh.camera_id, "depthtest-metric");
    assert_eq!(fresh.stamp_ns, desc.ts_ns);
    assert_eq!((fresh.w, fresh.h), (FRAME_COLS, FRAME_ROWS));
    assert_eq!(fresh.fit.held_ms, 0);
    assert!(fresh.fit.inliers >= 6, "{:?}", fresh.fit);
    // The post, a quarter across and mid-height, is 1.25 m deep.
    let at = |f: &ados_protocol::vision_depth_frame::DepthFrame, u: f32, v: f32| {
        let i = (v * f32::from(f.h)) as usize * usize::from(f.w) + (u * f32::from(f.w)) as usize;
        (f.depth_mm[i], f.sigma_mm[i])
    };
    let (post_mm, post_sigma) = at(&fresh, 0.25, 0.4);
    assert!((1_200..=1_300).contains(&post_mm), "{post_mm}");
    // 5 Hz at most per camera.
    e.run_depth(&desc, &data, 2_100).await.unwrap();
    assert!(rx.try_recv().is_err());
    // The scan goes stale: the last fit is held, less certain.
    e.run_depth(&desc, &data, 3_000).await.unwrap();
    let held = rx.try_recv().expect("a held frame");
    assert_eq!(held.fit.held_ms, 1_000);
    let (held_mm, held_sigma) = at(&held, 0.25, 0.4);
    assert_eq!(held_mm, post_mm);
    assert!(held_sigma >= post_sigma, "{held_sigma} {post_sigma}");
    // Ground 1.7 m ahead-right: the held fit's sigma has grown (the 50 m
    // background's sigma saturates either way).
    let (_, mid_fresh) = at(&fresh, 0.75, 0.8);
    let (_, mid_held) = at(&held, 0.75, 0.8);
    assert!(mid_held > mid_fresh, "{mid_held} {mid_fresh}");
    // Past the hold: nothing.
    e.run_depth(&desc, &data, 9_000).await.unwrap();
    assert!(rx.try_recv().is_err());
}

/// One grid of `map` (a 16:9 frame, default camera) with and without a scan
/// that agrees with `truth`.
fn grids(map: &DenseMap, truth: &DenseMap) -> [ados_protocol::vision_depth::DepthGrid; 2] {
    let ranged = Ranging {
        scan: Some(scan_for(truth)),
        down_m: None,
        attitude: None,
    };
    [ranged, Ranging::default()].map(|r| {
        DepthProcessor::default()
            .process("main", map, 16.0 / 9.0, 1, 0, &r, false)
            .grid
    })
}

#[test]
fn the_grid_reads_the_same_from_the_new_model_size_and_output_range() {
    // The scene as the previous model saw it (256x256) and as the current
    // one does (252x196); and the current one's map on another output scale
    // and offset. The 32x18 grid is pooled and normalised per frame.
    let old = synthetic_depth(256, 256);
    let new = synthetic_depth(252, 196);
    let mut rescaled = new.clone();
    for v in &mut rescaled.data {
        *v = 0.004 * *v + 0.3;
    }
    // The output range does not matter at all.
    assert_eq!(grids(&new, &new), grids(&rescaled, &new));
    // Nor does the input size: each cell agrees within one code step, or in
    // inverse range within about one model row of the scene's ground
    // gradient (the pooling samples different rows at the two sizes), except
    // where the post's edges fall across a cell boundary at one size and not
    // the other (columns 6-9).
    let (old_grids, new_grids) = (grids(&old, &old), grids(&new, &new));
    for (a, b) in old_grids.iter().zip(new_grids.iter()) {
        assert_eq!((a.cols, a.rows), (32, 18));
        assert_eq!(a.scale_source, b.scale_source);
        assert_eq!(a.measured, b.measured);
        for (i, (x, y)) in a.near.iter().zip(&b.near).enumerate() {
            let inv = |c: u8| 1.0 / decode_range_code(c).unwrap();
            let post_edge = (6..=9).contains(&(i % 32));
            assert!(
                post_edge || x.abs_diff(*y) <= 1 || (inv(*x) - inv(*y)).abs() <= 0.015,
                "cell {i}: {x} vs {y}"
            );
        }
    }
    assert_eq!(old_grids[0].scale_source, ScaleSource::Lidar);
    assert_eq!(old_grids[1].scale_source, ScaleSource::None);
}

#[tokio::test]
async fn a_metric_frame_reaches_a_frames_socket_subscriber() {
    use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
    use ados_protocol::vision_depth_frame::{DepthFrame, FRAME_MAX_BYTES};
    use std::time::Duration;

    let e = engine_with_depth().await;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("vision-depth-frames.sock");
    let sock_str = sock.to_str().unwrap().to_string();
    let cancel = ados_protocol::shutdown::Shutdown::new();
    let server = {
        let (e, cancel, sock_str) = (e.clone(), cancel.clone(), sock_str.clone());
        tokio::spawn(async move { crate::depth_bus::serve_frames(e, &sock_str, cancel).await })
    };
    let mut client = connect_with_retry(&sock_str, 50, Duration::from_millis(20))
        .await
        .unwrap();
    // Let the accept loop register the client before publishing.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (desc, data) = frame_on(&e, "depthtest-sock").await;
    e.request_demand(DemandKind::Depth, "depthtest-sock", "nav", 60_000, 0);
    e.range_feed()
        .set_scan(scan_for(&synthetic_depth(64, 64)), 1_000);
    e.run_depth(&desc, &data, 1_000).await.unwrap();
    let body = tokio::time::timeout(
        Duration::from_secs(5),
        read_length_prefixed(&mut client, FRAME_MAX_BYTES, true),
    )
    .await
    .unwrap()
    .unwrap()
    .expect("a frame");
    assert!(body.len() <= FRAME_MAX_BYTES);
    let frame = DepthFrame::from_msgpack(&body).unwrap();
    assert_eq!(frame.camera_id, "depthtest-sock");
    cancel.trigger();
    server.await.unwrap().unwrap();
}
