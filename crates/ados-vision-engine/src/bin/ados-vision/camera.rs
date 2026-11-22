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

/// The detector's turn on the frame clock. On wall time a frame that comes
/// while a batch is in flight is skipped (the camera's one inference permit),
/// so the detector runs as often as its batches allow. On the simulator's
/// clock in lockstep the simulator waits for each batch: no frame comes while
/// one runs, and the detector would meet its full rate in sim time, more
/// often than the same board manages in real time, holding the simulator for
/// every extra batch. So a simulated frame earlier on the frame clock than
/// the last batch's frame plus the wall time that batch ran is skipped, as it
/// would have come while the batch ran. A frame clock that goes back (a new
/// run) opens the turn.
#[derive(Clone, Default)]
struct DetectorTurn(Arc<std::sync::Mutex<Option<(u64, u64)>>>);

impl DetectorTurn {
    /// Whether a frame at `frame_ms` on the frame clock may start a batch.
    fn open(&self, frame_ms: u64) -> bool {
        let last = *self.0.lock().unwrap_or_else(|e| e.into_inner());
        last.is_none_or(|(at, until)| frame_ms < at || frame_ms >= until)
    }

    /// A batch on the frame at `frame_ms` ran for `ran_ms` of wall time.
    fn ran(&self, frame_ms: u64, ran_ms: u64) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((frame_ms, frame_ms.saturating_add(ran_ms)));
    }
}

/// Run one camera's capture loop: open its source, pull frames (each in the
/// source's native format), and publish each into the engine ring. A source
/// error, a silent source or a closed stream reopens it: at once after an
/// attempt that ran, paced at `SOURCE_RETRY` while attempts keep failing.
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
    // On the simulator's clock: each simulated frame keeps the camera's lane
    // busy (in lockstep, the simulator waits) until its batch is out.
    let pacing = engine.sim_pacing();
    // On the simulator's clock: a batch keeps the detector's turn for the
    // frame-clock time it ran ([`DetectorTurn`]).
    let turn = DetectorTurn::default();
    // The last source failure, surfaced in the status sidecar and logged at
    // warn once per distinct reason, cleared by the next delivered frame.
    let mut last_error: Option<String> = None;
    // A network camera that does not answer is probed, not opened: an open
    // would sit in the kernel's connect retry and see the camera back only at
    // its next SYN retransmit, seconds late.
    let probe = match &cam {
        CameraSpec::Owned(c) => ados_protocol::source_probe::SourceAddr::from_url(&c.source),
        CameraSpec::Tap { .. } => None,
    };
    loop {
        if let Some(addr) = &probe {
            if !addr.reachable().await {
                let camera_id = cam.id().to_string();
                let reason = "source not answering".to_string();
                if work_status::note_source_failure(&work, &mut last_error, &camera_id, reason) {
                    tracing::warn!(camera = %camera_id, "frame source not answering; probing until it does");
                }
                tokio::select! {
                    _ = addr.wait_reachable(tokio::time::Instant::now()) => {}
                    _ = cancel.wait() => return,
                }
            }
        }
        // Build the source for this attempt.
        let opened = tokio::time::Instant::now();
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
                                    // actually run. The models are paced on the
                                    // frame clock (a simulated frame's sim time).
                                    let now_ms = mono_ns() / 1_000_000;
                                    let sim_ms = frame_clock_ns(&desc).map(|ns| ns / 1_000_000);
                                    let frame_ms = sim_ms.unwrap_or(now_ms);
                                    let lane = pacing.as_ref().and_then(|p| p.frame(&camera_id, &desc).map(|ns| (p.clone(), ns)));
                                    let mut spawned = false;
                                    depth.offer(&engine, &desc, now_ms);
                                    if !paces.is_empty()
                                        && sim_ms.is_none_or(|ms| turn.open(ms))
                                        && engine.detection_wanted(&camera_id, now_ms)
                                    {
                                        if let Ok(permit) =
                                            infer_sem.clone().try_acquire_owned()
                                        {
                                            // A designation since the last batch
                                            // looks at its subject on this frame.
                                            if engine.take_designation(&camera_id) {
                                                scheduler.make_all_due();
                                            }
                                            let due = scheduler.take_due(frame_ms);
                                            if !due.is_empty() {
                                                let eng = engine.clone();
                                                let data = spare.take().unwrap_or_default();
                                                let back = recycle.clone();
                                                let lane = lane.clone();
                                                let cam = camera_id.clone();
                                                let turn = sim_ms.map(|ms| (turn.clone(), ms));
                                                spawned = true;
                                                tokio::spawn(async move {
                                                    let _permit = permit;
                                                    let started = std::time::Instant::now();
                                                    for det in due {
                                                        if let Err(e) = eng
                                                            .infer_and_publish(&det, &desc, &data)
                                                            .await
                                                        {
                                                            tracing::warn!(model = %det, error = %e, "detector_infer_failed");
                                                        }
                                                    }
                                                    if let Some((t, ms)) = turn {
                                                        let ran = started.elapsed().as_millis();
                                                        t.ran(ms, u64::try_from(ran).unwrap_or(u64::MAX));
                                                    }
                                                    back.give(data);
                                                    if let Some((p, ns)) = lane {
                                                        p.free(&cam, ns);
                                                    }
                                                });
                                            }
                                            // else: permit drops here, released; no
                                            // model was due this frame.
                                        }
                                    }
                                    if let Some((p, ns)) = lane.filter(|_| !spawned) {
                                        p.free(&camera_id, ns);
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
        if let Some(p) = &pacing {
            p.lost(&camera_id);
        }
        if backoff(&cancel, reopen_pause(opened.elapsed())).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DetectorTurn;

    #[test]
    fn a_batch_keeps_the_turn_for_its_run_time_on_the_frame_clock() {
        let turn = DetectorTurn::default();
        assert!(turn.open(1_000));
        // A 128 ms batch on the frame at 1000 ms: the 30 fps frames that
        // would have come while it ran are skipped, the fourth one runs.
        turn.ran(1_000, 128);
        assert!(!turn.open(1_033));
        assert!(!turn.open(1_100));
        assert!(turn.open(1_133));
        // A frame clock that went back is a new run.
        assert!(turn.open(400));
    }
}
