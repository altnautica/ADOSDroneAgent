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
use ados_protocol::framebus::ModelKind;
use ados_vision::backend::{select_backend, BackendPrefs};
use ados_vision::cameras::{camera_specs, needs_triggers, CameraSpec, SourceFactory};
use ados_vision::config::VisionConfig;
use ados_vision::daemon_support::{backoff, board_soc, init_tracing, sd_ready, wait_for_shutdown};
use ados_vision::detection_bus;
use ados_vision::engine::VisionEngine;
use ados_vision::frame_bus;
use ados_vision::perception_scheduler::PerceptionScheduler;
use ados_vision::source::{AnySource, FrameSource, SimClock};
use ados_vision::visionsock;
use ados_vision::work_status::{self, InputLiveness, WorkStatus, INPUT_SILENCE_TIMEOUT};

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
        },
    );
    tracing::info!(soc = %soc, backend = backend.name(), "vision backend selected");

    // Reclaim any ring a previous run stranded before opening new ones. `Drop`
    // unlinks on a clean exit but does not run on SIGKILL or an OOM-kill, and a
    // camera that re-enumerates under a new /dev/videoN mints a ring under a
    // new name — either way the old mapping holds tmpfs RAM until it is
    // unlinked. The unit's ExecStartPre does this too; doing it here as well
    // means the guarantee does not depend on how the engine was started.
    ados_vision::ring::sweep_stale_rings();

    let sizing = ados_vision::engine::RingSizing::new(
        config.effective_slot_count(),
        config.effective_shm_budget_bytes(),
    );
    // The tracker is off unless the config opts in. When on, the published
    // detection batch carries a stable track_id + lock_state on the locked
    // object; when off, the engine publishes raw detections. When re-id is on
    // and a re-id model is configured, the tracker associates on the model's
    // appearance embedding (registered below); otherwise it is motion-only.
    let reid_model_id = config.reid.as_ref().map(|r| r.model_id.clone());
    let engine = VisionEngine::with_tracker_reid(
        backend,
        sizing,
        config.tracker_enabled,
        ados_vision::tracker::TrackerConfig::default(),
        config.reid_enabled,
        reid_model_id,
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

    // Register the appearance (re-id) model when enabled. It is loaded as a
    // second engine-run model whose `embed` the tracker calls per detection; a
    // failed load (missing file / sidecar) leaves the tracker motion-only.
    if config.reid_enabled {
        if let Some(reid) = &config.reid {
            let meta = reid.to_metadata();
            let id = meta.id.clone();
            match engine.register_model(meta).await {
                Ok((_, loaded)) => tracing::info!(
                    model = %id, backend_loaded = loaded, path = %reid.model_path,
                    "re-id model registered"
                ),
                Err(e) => tracing::error!(model = %id, error = %e, "re-id registration failed"),
            }
        } else {
            tracing::warn!(
                "reid_enabled but no vision.reid model configured; tracker stays motion-only"
            );
        }
    }

    // Build the per-camera schedule input: every registered engine-run detection
    // model, each with a target rate + priority so the perception scheduler paces
    // them over the ONE accelerator. Sourced from the model registry (task-typed)
    // so an added detection model auto-schedules. A single unpaced detector (the
    // historical default) maps to one always-due model — byte-equivalent to the
    // old single-detector loop.
    let paces: Vec<ModelPace> = {
        let ids = engine.models_for_kind(ModelKind::Detection).await;
        let default_priority = 100u8;
        ids.into_iter()
            .map(|id| {
                let (rate_hz, priority) = match &config.detector {
                    Some(det) if det.model_id == id => (
                        det.rate_hz.unwrap_or(0.0),
                        det.priority.unwrap_or(default_priority),
                    ),
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
    tracing::info!(count = cameras.len(), "vision cameras resolved");

    let mut tasks = Vec::new();

    // Triggered cameras join their frames to the sensor bus trigger records;
    // on a simulated node each frame's simulator time is a clock pair.
    let mut factory = SourceFactory::default();
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
        tasks.push(tokio::spawn(async move {
            work_status::run_writer(work, path, cancel).await;
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
            run_camera(engine, cam, factory, paces, (input, work), cancel).await;
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

/// Run one camera's capture loop: open its source, pull frames (each in the
/// source's native format), and publish each into the engine ring. A source
/// error backs off and re-opens.
///
/// Every frame taken off the source is counted on `input`, and the input is
/// live from its first frame until the source fails, ends or delivers nothing
/// for [`INPUT_SILENCE_TIMEOUT`]. A silent source is closed and reopened, so an
/// upstream that stopped sending never reads as a stalled engine.
///
/// Inference is driven by a per-camera [`PerceptionScheduler`] over `paces`: on
/// each free frame the scheduler returns the detection models DUE now (by rate +
/// priority), and they run in one spawned batch that serializes on the engine's
/// single accelerator lease. With one unpaced model the scheduler returns it
/// every free frame, exactly the old single-detector loop.
async fn run_camera(
    engine: Arc<VisionEngine>,
    cam: CameraSpec,
    factory: SourceFactory,
    paces: Vec<ModelPace>,
    (mut input, work): (InputLiveness, Arc<WorkStatus>),
    cancel: ados_protocol::shutdown::Shutdown,
) {
    let mut frame_id: u64 = 0;
    // The per-camera schedule: each camera paces its models independently.
    let mut scheduler = PerceptionScheduler::new();
    for p in &paces {
        scheduler.upsert(p.id.clone(), p.rate_hz, p.priority);
    }
    // One in-flight inference batch per camera: a frame captured while a batch is
    // busy is skipped, so capture (and the video ring) never blocks on
    // inference. Decoupled, latest-frame-wins.
    let infer_sem = Arc::new(tokio::sync::Semaphore::new(1));
    // The last source failure, surfaced in the status sidecar and logged at
    // warn once per distinct reason, cleared by the next delivered frame.
    let mut last_error: Option<String> = None;
    loop {
        // Build the source for this attempt.
        let mut source: AnySource = factory.open(&cam);
        let camera_id = cam.id().to_string();
        // Consumed frame buffers go back to this source's pool, so a steady
        // stream reuses a few buffers instead of allocating one per frame.
        let recycle = source.recycler();
        loop {
            tokio::select! {
                frame = tokio::time::timeout(INPUT_SILENCE_TIMEOUT, source.next_frame()) => {
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(_) => {
                            tracing::debug!(
                                camera = %camera_id,
                                silent_s = INPUT_SILENCE_TIMEOUT.as_secs(),
                                "frame source delivered nothing; reopening"
                            );
                            break;
                        }
                    };
                    match frame {
                        Ok(raw) => {
                            input.frame();
                            if raw.dropped_corrupt > 0 {
                                work.add_error_frames(u64::from(raw.dropped_corrupt));
                            }
                            if last_error.take().is_some() {
                                work.set_source_error(&camera_id, None);
                            }
                            frame_id = frame_id.wrapping_add(1);
                            let mut spare = Some(raw.data);
                            let data_ref = spare.as_deref().unwrap_or(&[]);
                            match engine
                                .publish_frame(
                                    &camera_id,
                                    frame_id,
                                    raw.timing,
                                    raw.width,
                                    raw.height,
                                    raw.format,
                                    data_ref,
                                )
                                .await
                            {
                                Ok(desc) => {
                                    // Run the models DUE now, unless a batch is
                                    // already in flight (skip then, keeping capture
                                    // decoupled, latest-wins). Acquire the slot
                                    // FIRST so a busy frame never consumes a model's
                                    // scheduling turn; only mark models run when they
                                    // actually run.
                                    if !paces.is_empty() {
                                        if let Ok(permit) =
                                            infer_sem.clone().try_acquire_owned()
                                        {
                                            let due = scheduler.take_due(mono_ns() / 1_000_000);
                                            if !due.is_empty() {
                                                let eng = engine.clone();
                                                let data = spare.take().unwrap_or_default();
                                                let back = recycle.clone();
                                                tokio::spawn(async move {
                                                    let _permit = permit;
                                                    for det in due {
                                                        if let Err(e) = eng
                                                            .infer_and_publish(&det, &desc, &data)
                                                            .await
                                                        {
                                                            tracing::warn!(model = %det, error = %e, "detector_infer_failed");
                                                        }
                                                    }
                                                    back.give(data);
                                                });
                                            }
                                            // else: permit drops here, released; no
                                            // model was due this frame.
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(camera = %camera_id, error = %e, "publish_frame_failed");
                                }
                            }
                            if let Some(buf) = spare {
                                recycle.give(buf);
                            }
                        }
                        Err(e) => {
                            let reason = format!("{e:#}");
                            if last_error.as_deref() != Some(reason.as_str()) {
                                tracing::warn!(camera = %camera_id, error = %reason, "frame_source_failed; reopening");
                                work.set_source_error(&camera_id, Some(reason.clone()));
                                last_error = Some(reason);
                            }
                            break;
                        }
                    }
                }
                _ = cancel.wait() => return,
            }
        }
        input.lost();
        if backoff(&cancel).await {
            return;
        }
    }
}
