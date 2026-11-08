//! One camera's frame loop: publish, schedule detection, pace depth.

use super::*;

/// One camera's depth runs: at most one per `interval_ms`, one in flight,
/// each reading its frame back out of the ring into a reused buffer. With a
/// zero interval (no depth model) it never runs.
struct DepthPacer {
    interval_ms: u64,
    last_ms: Option<u64>,
    busy: Arc<tokio::sync::Semaphore>,
    buf: Arc<tokio::sync::Mutex<Vec<u8>>>,
}

impl DepthPacer {
    fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            last_ms: None,
            busy: Arc::new(tokio::sync::Semaphore::new(1)),
            buf: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Start a depth run on the frame `desc` names when the camera wants
    /// depth or surfaces, the pace allows, and no run is in flight.
    fn offer(&mut self, engine: &Arc<VisionEngine>, desc: &FrameDescriptor, now_ms: u64) {
        if self.interval_ms == 0
            || self
                .last_ms
                .is_some_and(|t| now_ms.saturating_sub(t) < self.interval_ms)
        {
            return;
        }
        let (depth, surfaces) = engine.depth_wanted(&desc.camera_id, now_ms);
        if !(depth || surfaces) {
            return;
        }
        let Ok(permit) = self.busy.clone().try_acquire_owned() else {
            return;
        };
        self.last_ms = Some(now_ms);
        let engine = engine.clone();
        let desc = desc.clone();
        let buf = self.buf.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut frame = buf.lock().await;
            if let Err(e) = engine.read_frame_into(&desc, &mut frame).await {
                tracing::debug!(camera = %desc.camera_id, error = %e, "depth_frame_read_failed");
                return;
            }
            if let Err(e) = engine.run_depth(&desc, &frame, now_ms).await {
                tracing::warn!(camera = %desc.camera_id, error = %e, "depth_infer_failed");
            }
        });
    }
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
/// single accelerator lease. Only while the engine wants detection on this
/// camera ([`VisionEngine::detection_wanted`]): otherwise frames are still
/// published and nothing is scheduled, so no model's turn is spent.
pub(super) async fn run_camera(
    engine: Arc<VisionEngine>,
    cam: CameraSpec,
    factory: SourceFactory,
    paces: Vec<ModelPace>,
    depth_interval_ms: u64,
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
    // The camera's depth runs: paced, one in flight.
    let mut depth = DepthPacer::new(depth_interval_ms);
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
                            let reason = work_status::silence_reason(INPUT_SILENCE_TIMEOUT);
                            if work_status::note_source_failure(&work, &mut last_error, &camera_id, reason) {
                                tracing::warn!(
                                    camera = %camera_id,
                                    silent_s = INPUT_SILENCE_TIMEOUT.as_secs(),
                                    "frame source delivered nothing; reopening"
                                );
                            }
                            break;
                        }
                    };
                    match frame {
                        Ok(raw) => {
                            input.frame();
                            if raw.dropped_corrupt > 0 {
                                work.add_error_frames(u64::from(raw.dropped_corrupt));
                            }
                            work_status::note_source_recovered(&work, &mut last_error, &camera_id);
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
                                    // Run the models DUE now, unless detection is
                                    // not wanted on this camera or a batch is
                                    // already in flight (skip then, keeping capture
                                    // decoupled, latest-wins). Acquire the slot
                                    // FIRST so a busy frame never consumes a model's
                                    // scheduling turn; only mark models run when they
                                    // actually run.
                                    let now_ms = mono_ns() / 1_000_000;
                                    depth.offer(&engine, &desc, now_ms);
                                    if !paces.is_empty() && engine.detection_wanted(&camera_id, now_ms) {
                                        if let Ok(permit) =
                                            infer_sem.clone().try_acquire_owned()
                                        {
                                            let due = scheduler.take_due(now_ms);
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
                            if work_status::note_source_failure(&work, &mut last_error, &camera_id, reason.clone()) {
                                tracing::warn!(camera = %camera_id, error = %reason, "frame_source_failed; reopening");
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
