//! `ados-vision` entry point.
//!
//! Resolves the `vision:` config, gates on enablement + the agent profile (a
//! ground-station node or a disabled engine exits cleanly with no work), picks
//! an inference backend for the board, then starts:
//!
//! - one capture task per engine camera (the owned `capture: true` entries of
//!   `cameras[]` and the video pipeline's tap), each opening its frame source
//!   and publishing normalized frames into the engine's ring,
//! - the `/run/ados/vision.sock` request/response server the plugin host
//!   connects to.
//!
//! It runs until SIGTERM / SIGINT. The accelerator runtime is never linked
//! here: NPU inference is reached through the Python sidecar, ONNX is an opt-in
//! cargo feature, and the default build runs the mock backend.

use std::path::Path;
use std::sync::Arc;

use ados_protocol::framebus::mono_ns;
use ados_protocol::framebus::{FrameDescriptor, ModelKind};
use ados_vision_engine::backend::{select_backend, BackendPrefs};
use ados_vision_engine::cameras::{camera_specs, needs_triggers, CameraSpec, SourceFactory};
use ados_vision_engine::config::{DepthPlan, VisionConfig};
use ados_vision_engine::daemon_support::{
    backoff, board_soc, init_tracing, sd_ready, wait_for_shutdown,
};
use ados_vision_engine::depth::feed;
use ados_vision_engine::depth_bus;
use ados_vision_engine::detection_bus;
use ados_vision_engine::engine::{CameraMotionSetup, DepthSetup, VisionEngine};
use ados_vision_engine::frame_bus;
use ados_vision_engine::perception_scheduler::PerceptionScheduler;
use ados_vision_engine::source::{AnySource, FrameSource, SimClock};
use ados_vision_engine::visionsock;
use ados_vision_engine::work_status::{self, InputLiveness, WorkStatus, INPUT_SILENCE_TIMEOUT};

// A crate root resolves `mod` next to itself, where Cargo would take a file
// for a binary of its own; the module lives in the binary's own directory.
#[path = "ados-vision/camera.rs"]
mod camera;
use camera::run_camera;

/// Canonical agent config file.
const CONFIG_YAML: &str = "/etc/ados/config.yaml";

#[tokio::main]
async fn main() {
    init_tracing();

    let config = VisionConfig::load_from(Path::new(CONFIG_YAML));
    tracing::info!(
        enabled = config.enabled,
        profile = ?config.profile,
        backend = %config.backend,
        cameras = config.cameras.len(),
        "ados-vision resolved config"
    );

    // GATE A: a ground-station node has no air-side cameras.
    if config.is_ground_station() {
        tracing::info!("ados-vision idle: ground-station profile has no vision engine");
        return;
    }
    // GATE B: the engine is opt-in, except that it is the only owner of the
    // `capture: true` cameras, so any such camera runs it.
    if !config.should_run() {
        tracing::info!("ados-vision idle: vision is not enabled and no camera is captured");
        return;
    }

    let soc = board_soc();
    let backend = select_backend(
        &soc,
        &BackendPrefs {
            preference: &config.backend,
            rknn_socket_path: config.rknn_socket_path(),
            hailo_socket_path: config.hailo_socket_path(),
            mock_target: config.mock_target,
            onnx_threads: config.detector_threads() as usize,
            depth_threads: config.depth_threads() as usize,
        },
    );
    tracing::info!(soc = %soc, backend = backend.name(), "vision backend selected");

    // Reclaim any ring a previous run stranded before opening new ones. `Drop`
    // unlinks on a clean exit but does not run on SIGKILL or an OOM-kill, and a
    // camera that re-enumerates under a new /dev/videoN mints a ring under a
    // new name — either way the old mapping holds tmpfs RAM until it is
    // unlinked. The unit's ExecStartPre does this too; doing it here as well
    // means the guarantee does not depend on how the engine was started.
    ados_vision_engine::ring::sweep_stale_rings();

    let sizing = ados_vision_engine::engine::RingSizing::new(
        config.effective_slot_count(),
        config.effective_shm_budget_bytes(),
    );
    // The tracker is off unless the config opts in. When on, the published
    // detection batch carries a stable track_id + lock_state on the locked
    // object; when off, the engine publishes raw detections. When re-id is on,
    // the tracker associates on appearance embeddings: the configured re-id
    // model's (registered below), else the model-free colour histogram.
    let reid_model_id = config.reid.as_ref().map(|r| r.model_id.clone());
    let engine = VisionEngine::with_tracker_reid(
        backend,
        sizing,
        config.tracker_enabled,
        ados_vision::tracker::TrackerConfig::default(),
        config.reid_enabled,
        reid_model_id,
        config.detection_policy(),
    );
    tracing::info!(
        demand = config.detection_demand.as_str(),
        designate_hold_s = config.designate_hold_s,
        max_box_area_frac = config.max_box_area_frac,
        "detection demand policy"
    );
    if config.tracker_enabled {
        tracing::info!(reid = config.reid_enabled, "vision tracker enabled");
    }

    // Register the configured detector as an engine-run model. With it, each
    // camera's capture loop drives inference and the engine publishes a
    // detection stream (consumed by follow / designate plugins). Absent ⇒ the
    // engine only runs models a plugin registers over vision.sock.
    // Registration side-effects (the model lands in the engine registry, read
    // back below to build the schedule); the returned id itself is unused now
    // that the schedule drives inference.
    let _detector_id: Option<String> = if let Some(det) = &config.detector {
        let meta = det.to_metadata();
        let id = meta.id.clone();
        match engine.register_model(meta).await {
            Ok((_, loaded)) => {
                tracing::info!(
                    model = %id, backend_loaded = loaded, path = %det.model_path,
                    "detector registered"
                );
                if !loaded {
                    tracing::warn!(
                        model = %id,
                        "detector backend not loaded; no detections until the model file/sidecar is reachable"
                    );
                }
                Some(id)
            }
            Err(e) => {
                tracing::error!(model = %id, error = %e, "detector registration failed");
                None
            }
        }
    } else {
        None
    };

    // Register the appearance (re-id) model whenever one is configured. It is
    // loaded as a second engine-run model whose `embed` produces the embeddings
    // the re-id tracker associates on and that a subscriber can request on the
    // detection wire. A failed load (missing file / sidecar) falls back to the
    // colour histogram descriptor.
    match &config.reid {
        Some(reid) => {
            let meta = reid.to_metadata();
            let id = meta.id.clone();
            match engine.register_model(meta).await {
                Ok((_, loaded)) => tracing::info!(
                    model = %id, backend_loaded = loaded, path = %reid.model_path,
                    "re-id model registered"
                ),
                Err(e) => tracing::error!(model = %id, error = %e, "re-id registration failed"),
            }
        }
        None if config.reid_enabled => tracing::warn!(
            "reid_enabled but no vision.reid model configured; the tracker associates on the colour histogram"
        ),
        None => {}
    }

    // The depth model behind the depth and surfaces layers. It loads when its
    // file is installed (or the mock is the backend); without one, depth and
    // surfaces leases are accepted but report not running, and the status
    // says why.
    let depth_interval_ms = {
        let plan = config
            .depth
            .plan(engine.backend_name(), &config.models_dir, |p| {
                Path::new(p).is_file()
            });
        let (model_id, unavailable, interval_ms) = match plan {
            DepthPlan::Load { meta, interval_ms } => {
                let id = meta.id.clone();
                match engine.register_model(meta).await {
                    Ok((_, true)) => {
                        tracing::info!(model = %id, interval_ms, "depth model registered");
                        (Some(id), None, interval_ms)
                    }
                    Ok((_, false)) => {
                        tracing::warn!(model = %id, "depth model backend not loaded");
                        (
                            Some(id),
                            Some("the depth model did not load".to_string()),
                            interval_ms,
                        )
                    }
                    Err(e) => {
                        tracing::error!(model = %id, error = %e, "depth model registration failed");
                        (
                            None,
                            Some(format!("depth model registration failed: {e:#}")),
                            0,
                        )
                    }
                }
            }
            DepthPlan::None(reason) => {
                tracing::info!(reason = %reason, "no depth model");
                (None, None, 0)
            }
        };
        engine.configure_depth(DepthSetup {
            model_id,
            unavailable,
            interval_ms,
            geometries: config.camera_geometry.clone(),
        });
        interval_ms
    };
    // The tracker moves each configured camera's track with the vehicle's
    // turn between batches. Under simulation the autopilot runs on the
    // simulator's clock, the one the simulated cameras stamp exposures with.
    engine.configure_camera_motion(CameraMotionSetup {
        geometries: config.camera_geometry.clone(),
        fc_on_sim_clock: config.simulation,
    });

    // Build the per-camera schedule input: every registered engine-run detection
    // model, each with a target rate + priority so the perception scheduler paces
    // them over the ONE accelerator. Sourced from the model registry (task-typed)
    // so an added detection model auto-schedules. The configured detector runs
    // at `detector.rate_hz` (an explicit 0 is unpaced); a plugin-registered
    // detection model is unpaced.
    let paces: Vec<ModelPace> = {
        let ids = engine.models_for_kind(ModelKind::Detection).await;
        let default_priority = 100u8;
        ids.into_iter()
            .map(|id| {
                let (rate_hz, priority) = match &config.detector {
                    Some(det) if det.model_id == id => {
                        (det.rate_hz, det.priority.unwrap_or(default_priority))
                    }
                    _ => (0.0, default_priority),
                };
                ModelPace {
                    id,
                    rate_hz,
                    priority,
                }
            })
            .collect()
    };
    tracing::info!(models = paces.len(), "perception schedule built");

    let cancel = ados_protocol::shutdown::Shutdown::new();

    // The engine's cameras: the owned `capture: true` entries of `cameras[]`
    // and, when the video pipeline taps its flight camera, that tap.
    let cameras = camera_specs(&config);
    let camera_ids: Vec<String> = cameras.iter().map(|c| c.id().to_string()).collect();
    engine.set_capture_cameras(&camera_ids);
    tracing::info!(count = cameras.len(), "vision cameras resolved");

    let mut tasks = Vec::new();

    // Triggered cameras join their frames to the sensor bus trigger records;
    // on a simulated node each frame's simulator time is a clock pair. Each
    // opened source registers its control route on the engine's table; the
    // entries the engine does not own are refused as non-capture cameras.
    let mut factory = SourceFactory {
        controls: engine.camera_controls().clone(),
        decode_threads: config.decode_threads,
        ..SourceFactory::default()
    };
    for id in &config.other_camera_ids {
        factory.controls.register_not_capture(id);
    }
    let trigger_task = needs_triggers(&cameras).then(|| {
        factory
            .triggers
            .spawn_subscriber(ados_sensorbus::socket_path())
    });
    if config.simulation {
        factory.sim = SimClock::publishing(ados_sensorbus::Publisher::spawn(
            ados_sensorbus::socket_path(),
            ados_sensorbus::Origin::Vision,
        ));
    }

    // The work counters the supervisor judges this engine on, published to the
    // status sidecar about once a second.
    let work = WorkStatus::new();
    // Entries of `cameras[]` that do not validate are skipped (by every
    // camera consumer) and listed with the reason.
    for bad in &config.invalid_cameras {
        work.set_source_error(
            &bad.id,
            Some(format!(
                "invalid cameras[{}] entry: {}",
                bad.index, bad.error
            )),
        );
    }
    {
        let work = work.clone();
        let cancel = cancel.clone();
        let path = work_status::sidecar_path(&config.socket_dir);
        let engine = engine.clone();
        // The detector's and the depth layer's demand state ride in the
        // sidecar for `GET /api/vision/status`.
        let fill = move |s: &mut ados_protocol::vision_status::VisionStatus| {
            let now_ms = mono_ns() / 1_000_000;
            s.detection = Some(engine.detection_status(&camera_ids, now_ms));
            s.depth = Some(engine.depth_status(&camera_ids, now_ms));
        };
        tasks.push(tokio::spawn(async move {
            work_status::run_writer(work, fill, path, cancel).await;
        }));
    }

    // One capture task per camera.
    for cam in cameras {
        let engine = engine.clone();
        let cancel = cancel.clone();
        let paces = paces.clone();
        let input = work.input();
        let work = work.clone();
        let factory = factory.clone();
        tasks.push(tokio::spawn(async move {
            run_camera(
                engine,
                cam,
                factory,
                paces,
                depth_interval_ms,
                (input, work),
                cancel,
            )
            .await;
        }));
    }

    // The vision.sock server.
    {
        let engine = engine.clone();
        let cancel = cancel.clone();
        let sock = config.vision_socket_path();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = visionsock::serve(engine, &sock, cancel).await {
                tracing::error!(error = %e, "vision_sock_serve_failed");
            }
        }));
    }

    // The vision-detections.sock broadcast: every published detection batch is
    // re-framed onto a Unix socket the agent's API process subscribes to and
    // forwards to the browser over a WebSocket.
    {
        let engine = engine.clone();
        let cancel = cancel.clone();
        let sock = config.detections_socket_path();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = detection_bus::serve(engine, &sock, cancel).await {
                tracing::error!(error = %e, "vision_detections_serve_failed");
            }
        }));
    }

    // The vision-frames.sock broadcast: every published frame descriptor is
    // re-framed onto a Unix socket an on-box service (the world-model capture
    // service) subscribes to, then maps the ring the descriptor names and reads
    // the slot itself. Only the descriptor crosses the socket; pixels stay in shm.
    {
        let engine = engine.clone();
        let cancel = cancel.clone();
        let sock = config.frames_socket_path();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = frame_bus::serve(engine, &sock, cancel).await {
                tracing::error!(error = %e, "vision_frames_serve_failed");
            }
        }));
    }

    // The depth layer: its broadcast, and the planar scan its scale fit reads
    // off the plugin event export. Only when a depth model runs.
    if depth_interval_ms > 0 {
        let engine_bus = engine.clone();
        let cancel_bus = cancel.clone();
        let sock = config.depth_socket_path();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = depth_bus::serve(engine_bus, &sock, cancel_bus).await {
                tracing::error!(error = %e, "vision_depth_serve_failed");
            }
        }));
        tasks.push(tokio::spawn(feed::run_scan_reader(
            feed::default_events_socket(),
            engine.range_feed(),
            cancel.clone(),
        )));
    }
    // The attitude and the downward rangefinder off the MAVLink socket: the
    // depth fit reads them, and the tracker reads the attitude history to
    // take the camera's own turn out of a track's motion.
    if depth_interval_ms > 0 || config.tracker_enabled {
        tasks.push(tokio::spawn(feed::run_mavlink_reader(
            config.mavlink_socket_path().into(),
            engine.range_feed(),
            cancel.clone(),
        )));
    }

    sd_ready();
    tracing::info!("ados-vision ready");
    wait_for_shutdown().await;
    tracing::info!("ados-vision stopping");
    cancel.trigger();
    if let Some(t) = trigger_task {
        t.abort();
    }
    for t in tasks {
        let _ = t.await;
    }
    tracing::info!("ados-vision stopped");
}

/// One engine-run detection model plus how the perception scheduler paces it.
#[derive(Debug, Clone)]
struct ModelPace {
    id: String,
    /// Target inference rate in Hz; `0.0` = unpaced (run whenever the accelerator
    /// is free), the historical single-detector behaviour.
    rate_hz: f32,
    priority: u8,
}
