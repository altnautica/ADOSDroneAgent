//! The video-pipeline FSM.
//!
//! This is the integration core that wires the leaf modules (camera
//! discovery, the encoder command builder, mediamtx, the wfb radio tap, the
//! camera-state sidecar) into the long-lived supervisor the drone runs on the
//! air side. It owns the full lifecycle: cold start, the 5 s health tick with
//! exponential-backoff restart + circuit breaker, the separate cloud-push and
//! wfb-tee restart ladders, the camera-hotplug-woken retry from the error
//! state, and the RAII teardown of every child process on shutdown.
//!
//! The sequencing is a faithful port of the Python `VideoPipeline`
//! orchestrator (`services/video/pipeline/pipeline.py`): the same start order,
//! the same grace / inbound-stall / wfb-stale health rules, the same backoff
//! ladders. The process-group ownership (setsid + killpg) that the Python
//! version did by hand lives structurally in [`crate::process::ManagedProcess`]
//! now, so a dropped future can never orphan a publish-bridge ffmpeg onto the
//! mediamtx `/main` slot.
//!
//! The encoder gets a PLAIN rate-limited stderr drain (NOT the wfb-tee's
//! progress tracker): the encoder is the source, its `frame=` cadence is the
//! wfb-tee's job to watch via the mediamtx inbound-byte counter, and attaching
//! a progress tracker to the encoder would conflate two independent liveness
//! signals.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify};

use crate::config::{AgentVideoConfig, CameraConfig};
use crate::discover::{self, DiscoveryResult};
use crate::encoder::{EncoderEnv, EncoderKind};
use crate::mediamtx::{MediamtxManager, MAIN_PATH};
use crate::process::ManagedProcess;
use crate::shutdown::Shutdown;
use crate::wfb_tee::{wfb_tee_progress_is_stale, ProgressTracker, WFB_TEE_PROGRESS_TIMEOUT};

// The pipeline's pure health-decision logic (constants, FSM states, the
// backoff / circuit-breaker / grace / inbound-flow decisions) lives in
// [`crate::health`]. Re-exported at this module path so the original
// `orchestrator::HEALTH_CHECK_INTERVAL`, `orchestrator::PipelineState`,
// `orchestrator::backoff_delay`, etc. callers keep resolving unchanged.
pub use crate::health::{
    backoff_delay, circuit_breaker_tripped, grace_decision, healthy_window_elapsed,
    inbound_decision, retry_cap, GraceDecision, InboundDecision, PipelineState, StartError,
    BASE_RESTART_DELAY, CIRCUIT_BREAKER_ATTEMPTS, HEALTHY_RESET_WINDOW, HEALTH_CHECK_INTERVAL,
    INBOUND_FLOW_STALL, MAX_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA, STARTUP_GRACE_MAX,
    WFB_TEE_RESTART_CEILING,
};

// --- orchestrator ------------------------------------------------------------

/// Owns and supervises the air-side video pipeline subprocess tree.
///
/// The fields are `pub(crate)` so the subprocess start/stop lifecycle (defined
/// in [`crate::lifecycle`]) and the supervision FSM (defined here) can share
/// the same state across the two modules; nothing here is part of the crate's
/// public API.
pub struct VideoOrchestrator {
    pub(crate) config: AgentVideoConfig,
    pub(crate) camera_cfg: CameraConfig,

    pub(crate) mediamtx: MediamtxManager,
    pub(crate) encoder: Option<ManagedProcess>,
    pub(crate) wfb_tee: Option<ManagedProcess>,
    pub(crate) cloud_push: Option<ManagedProcess>,
    pub(crate) sei_tap: Option<ManagedProcess>,
    /// The decoupled vision frame tap (a third ffmpeg → rawvideo → stdout).
    /// Spawned only when `video.vision.enabled` and `raw_tap` is off. Additive:
    /// a crash here never touches the encode or wfb path.
    pub(crate) vision_tap: Option<ManagedProcess>,
    /// The in-process reframer that reads the vision tap's raw frames, ADVT-
    /// headers them (Contract F), and serves the connecting vision engine.
    /// Bound to the same lifetime as `vision_tap`; aborted on stop/restart.
    pub(crate) vision_tap_reframer: Option<tokio::task::JoinHandle<()>>,
    /// Supervised-subprocess slot held for a future GStreamer air branch. It
    /// is never populated on the current bash path, so the teardown paths reap
    /// it for free if that branch is ever wired; there is no operator toggle
    /// that selects it.
    pub(crate) gst_air: Option<ManagedProcess>,

    pub(crate) wfb_tee_progress: ProgressTracker,
    /// Output-progress clock for the decoupled vision tap (Rule 37: liveness
    /// alone is never proof of work; the tap can hold the sink open while
    /// pushing nothing).
    pub(crate) vision_tap_progress: ProgressTracker,

    pub(crate) last_cameras: DiscoveryResult,
    pub(crate) encoder_type: Option<EncoderKind>,

    pub(crate) state: PipelineState,
    pub(crate) started_at: Instant,
    pub(crate) first_packet_seen: bool,

    /// mediamtx inbound-byte counter (`-1` ⇒ no sample yet).
    pub(crate) inbound_bytes_value: i64,
    pub(crate) inbound_bytes_changed_at: Instant,
    pub(crate) video_inbound_bytes_per_s: f64,

    /// The instant a healthy run began, or `None` (the armed sentinel,
    /// Python's `0.0`) when the next healthy tick should re-stamp it. A failed
    /// probe re-arms it to `None`.
    pub(crate) last_healthy_at: Option<Instant>,

    pub(crate) restart_count: u32,
    pub(crate) cloud_restart_count: u32,
    pub(crate) wfb_tee_restart_count: u32,
    pub(crate) vision_tap_restart_count: u32,
    pub(crate) last_start_error: StartError,

    /// Earliest instant the cloud-push branch may retry, or `None` when it may
    /// retry now. The cloud relay is the SECONDARY path; its backoff/park must
    /// never block the whole tick body and starve primary-radio (wfb-tee)
    /// recovery. So instead of sleeping the tick on the cloud backoff, the
    /// cloud branch stamps a deadline here and the tick falls through to the
    /// wfb / vision branches; the cloud branch is skipped until the deadline
    /// elapses.
    pub(crate) cloud_retry_after: Option<Instant>,

    /// Serializes teardown+respawn across the cold start and the health-check
    /// restart (held only around the bounded region, never across a backoff
    /// sleep). A runtime camera change is not a distinct operation: a fresh
    /// `/dev/video*` node fires SIGUSR1 → `camera_plugged` → the retry path,
    /// which re-runs the same locked teardown+respawn, so there is no separate
    /// switch lock.
    pub(crate) restart_lock: Arc<Mutex<()>>,
    /// Fired by SIGUSR1 (a fresh `/dev/video*` node) to short-circuit the
    /// no-primary backoff sleep.
    pub(crate) camera_plugged: Arc<Notify>,

    pub(crate) python_executable: String,
    pub(crate) env: EncoderEnv,

    /// Override for the camera-state sidecar path. `None` ⇒ the canonical
    /// `/run/ados/camera-state.json` contract path. Set only in tests so the
    /// wedge/park re-persist can be asserted against a temp file.
    pub(crate) camera_state_path: Option<std::path::PathBuf>,
}

impl VideoOrchestrator {
    /// Build an orchestrator for the given config. `config_dir` is where the
    /// mediamtx config file is written (canonically `/etc/ados`).
    pub fn new(
        config: AgentVideoConfig,
        camera_cfg: CameraConfig,
        config_dir: &std::path::Path,
    ) -> Self {
        let now = Instant::now();
        Self {
            config,
            camera_cfg,
            mediamtx: MediamtxManager::new(config_dir),
            encoder: None,
            wfb_tee: None,
            cloud_push: None,
            sei_tap: None,
            vision_tap: None,
            vision_tap_reframer: None,
            gst_air: None,
            wfb_tee_progress: ProgressTracker::new(),
            vision_tap_progress: ProgressTracker::new(),
            last_cameras: DiscoveryResult::empty(),
            encoder_type: None,
            state: PipelineState::Stopped,
            started_at: now,
            first_packet_seen: false,
            inbound_bytes_value: -1,
            inbound_bytes_changed_at: now,
            video_inbound_bytes_per_s: 0.0,
            last_healthy_at: None,
            restart_count: 0,
            cloud_restart_count: 0,
            wfb_tee_restart_count: 0,
            vision_tap_restart_count: 0,
            last_start_error: StartError::None,
            cloud_retry_after: None,
            restart_lock: Arc::new(Mutex::new(())),
            camera_plugged: Arc::new(Notify::new()),
            python_executable: discover::python_executable(),
            env: EncoderEnv::detect(),
            camera_state_path: None,
        }
    }

    /// Re-persist the camera-state sidecar from the last-known discovery
    /// snapshot. Used both on a healthy tick and across the wedge/park backoff
    /// sleeps so a present-but-wedged camera does not read as `unknown` to the
    /// staleness gate while the orchestrator is stuck. Honors
    /// [`Self::camera_state_path`] (canonical path when `None`).
    fn refresh_camera_state(&self) {
        match self.camera_state_path.as_deref() {
            Some(p) => discover::persist_camera_state_to(&self.last_cameras, p),
            None => discover::persist_camera_state(&self.last_cameras),
        }
    }

    /// The Notify handle SIGUSR1 fires to wake the no-primary backoff sleep.
    pub fn camera_plugged_handle(&self) -> Arc<Notify> {
        self.camera_plugged.clone()
    }

    /// Current FSM state.
    pub fn state(&self) -> PipelineState {
        self.state
    }

    /// Most recent inbound video throughput in bytes/s (observability).
    pub fn video_inbound_bytes_per_s(&self) -> f64 {
        (self.video_inbound_bytes_per_s * 10.0).round() / 10.0
    }

    pub(crate) fn pipe_uri(&self) -> String {
        format!("rtsp://localhost:{}/main", self.mediamtx.rtsp_port())
    }

    pub(crate) fn sei_latency_on(&self) -> bool {
        self.config.wfb.sei_latency
    }

    /// True when the additive vision frame tap is configured to run on this
    /// node: enabled in config AND this is not a ground-station profile (the
    /// air-side pipeline never runs on a ground station, so neither does the
    /// tap). This is the single gate the tap consults.
    pub(crate) fn vision_enabled(&self) -> bool {
        self.config.vision.enabled && !self.config.is_ground_station()
    }

    /// Stop the encoding pipeline and mediamtx. Teardown order: wfb_tee →
    /// sei_tap → cloud_push → encoder → mediamtx. Mirrors `stop_stream`.
    pub async fn stop_stream(&mut self) {
        tracing::info!("stop_stream_begin");
        self.stop_gst_air().await;
        self.stop_wfb_tee().await;
        self.stop_vision_tap().await;
        if let Some(mut tap) = self.sei_tap.take() {
            tap.terminate(Duration::from_secs(5)).await;
        }
        self.stop_cloud_push().await;
        if let Some(mut enc) = self.encoder.take() {
            if enc.is_running() {
                enc.terminate(Duration::from_secs(5)).await;
            }
        }
        self.mediamtx.stop().await;
        self.state = PipelineState::Stopped;
        tracing::info!("pipeline_stopped");
    }

    /// One health probe: encoder PID alive + mediamtx alive + the startup-grace
    /// / path-ready / inbound-stall ladder. Mirrors `_check_health`. Returns
    /// the overall health verdict; side-effects (latching first-packet,
    /// recording bytes/s) are applied here against the pure decision helpers.
    pub async fn check_health(&mut self) -> bool {
        // Encoder liveness.
        let Some(enc) = self.encoder.as_mut() else {
            return false;
        };
        if !enc.is_running() {
            tracing::warn!("encoder_process_exited");
            return false;
        }
        // mediamtx liveness — a dead mediamtx leaves ffmpeg blocked on a dead
        // RTSP socket while still looking alive.
        if !self.mediamtx.is_running() {
            tracing::warn!("mediamtx_died_during_stream");
            return false;
        }

        let elapsed = Instant::now().saturating_duration_since(self.started_at);
        let path_ready = self.mediamtx.path_ready(MAIN_PATH).await;

        // Startup-grace ladder until the first packet is seen.
        if !self.first_packet_seen {
            match grace_decision(path_ready, elapsed) {
                GraceDecision::FirstPacket => {
                    self.first_packet_seen = true;
                    tracing::info!(elapsed_s = elapsed.as_secs_f64(), "pipeline_first_packet");
                    return true;
                }
                GraceDecision::StillWaiting => return true,
                GraceDecision::Expired => {
                    tracing::warn!(
                        elapsed_s = elapsed.as_secs_f64(),
                        "pipeline_grace_expired: no mediamtx publisher after grace window"
                    );
                    return false;
                }
            }
        }

        // Live: the publisher must still be ready.
        if !path_ready {
            tracing::warn!("mediamtx_path_not_ready: encoder RTSP connection likely dead");
            return false;
        }

        // Inbound-flow watchdog: assert mediamtx's byte counter advances.
        self.check_inbound_flow_healthy().await
    }

    /// Inbound-byte stall watchdog. Reads mediamtx's per-path `bytesReceived`,
    /// applies the pure [`inbound_decision`], and records the bytes/s. Falls
    /// back to healthy on an unreadable counter (a transient API hiccup must
    /// never force a needless restart). Mirrors `_check_inbound_flow_healthy`.
    async fn check_inbound_flow_healthy(&mut self) -> bool {
        let Some(current) = self.mediamtx.inbound_bytes(MAIN_PATH).await else {
            return true;
        };
        let current = current as i64;
        let now = Instant::now();
        let since_change = now.saturating_duration_since(self.inbound_bytes_changed_at);
        match inbound_decision(
            self.inbound_bytes_value,
            current,
            since_change,
            HEALTH_CHECK_INTERVAL,
        ) {
            InboundDecision::Seed => {
                self.inbound_bytes_value = current;
                self.inbound_bytes_changed_at = now;
                true
            }
            InboundDecision::Advanced { bytes_per_s } => {
                self.video_inbound_bytes_per_s = bytes_per_s;
                self.inbound_bytes_value = current;
                self.inbound_bytes_changed_at = now;
                true
            }
            InboundDecision::WithinStall => true,
            InboundDecision::Stalled => {
                self.video_inbound_bytes_per_s = 0.0;
                tracing::warn!(
                    bytes_received = current,
                    stalled_s = since_change.as_secs_f64(),
                    "video_inbound_flow_stalled: encoder alive + publisher present but bytes flat; restarting publish"
                );
                false
            }
        }
    }

    /// Is the cloud push still alive? `true` when healthy or — for the absent
    /// slot — only when cloud relay is NOT configured to be running here.
    ///
    /// The None branch is the same shape as the wfb-tee None-branch: a full
    /// pipeline restart (`stop_stream` → `start_stream`) clears `cloud_push`
    /// and `start_stream` does NOT re-arm it, so reporting healthy for None
    /// would leave a configured cloud push dead for the rest of the process
    /// lifetime. Report unhealthy for the absent slot WHILE cloud is enabled
    /// AND the pipeline is Running so the run-loop ladder re-arms it; when
    /// cloud is disabled, or the pipeline is not running, the absent slot is
    /// correct and healthy. Mirrors `_check_cloud_push_health`.
    fn check_cloud_push_health(&mut self) -> bool {
        match self.cloud_push.as_mut() {
            None => !(self.config.cloud_enabled() && self.state == PipelineState::Running),
            Some(p) => {
                if p.is_running() {
                    true
                } else {
                    tracing::warn!("cloud_push_process_exited");
                    self.cloud_push = None;
                    false
                }
            }
        }
    }

    /// Is the wfb tap alive AND producing? `true` when healthy or never
    /// started. Mirrors `_check_wfb_tee_health` (Rule 37: liveness ≠ work).
    async fn check_wfb_tee_health(&mut self) -> bool {
        let Some(p) = self.wfb_tee.as_mut() else {
            // No tee yet: the first spawn is deferred until the RTSP source is
            // ready, so report unhealthy WHILE THE PIPELINE IS RUNNING and let
            // the run-loop ladder bring it up (the ladder itself defers on
            // !path_ready). When the pipeline is not running the tee is
            // correctly absent and this is healthy. Without this, a tee that
            // was deferred at stream start stays None forever — healthy by the
            // old rule — so the ladder never starts it and the radio is starved.
            return self.state != PipelineState::Running;
        };
        if !p.is_running() {
            tracing::warn!("wfb_tee_process_exited");
            self.wfb_tee = None;
            // Unhealthy: route into the run-loop wfb ladder so the tee gets
            // respawned (the ladder defers when the RTSP source is down).
            return false;
        }
        let last = self.wfb_tee_progress.last_progress_at().await;
        if wfb_tee_progress_is_stale(last, Instant::now()) {
            tracing::warn!(
                threshold_s = WFB_TEE_PROGRESS_TIMEOUT.as_secs(),
                "wfb_tee_zombie_detected: alive but progress flat; forcing restart"
            );
            return false;
        }
        true
    }

    /// Is the vision frame tap alive AND producing? `true` when healthy or
    /// never started. Same Rule-37 liveness-plus-work contract as the wfb tap
    /// (a tap alive but pushing nothing is a zombie). Only the decoupled tap is
    /// supervised here; the raw_tap split rides the encoder's own health.
    async fn check_vision_tap_health(&mut self) -> bool {
        let Some(p) = self.vision_tap.as_mut() else {
            // No tap yet: the first spawn is deferred until the RTSP source is
            // ready, so report unhealthy WHILE THE PIPELINE IS RUNNING and let
            // the run-loop ladder bring it up. When the pipeline is not running
            // the tap is correctly absent and this is healthy. Without this, a
            // tap that was deferred at stream start stays None forever (healthy
            // by the old rule) so the ladder never starts it.
            return self.state != PipelineState::Running;
        };
        if !p.is_running() {
            tracing::warn!("vision_tap_process_exited");
            self.vision_tap = None;
            // Unhealthy: route into the run-loop vision ladder so the tap gets
            // respawned (the ladder defers when the RTSP source is down).
            return false;
        }
        let last = self.vision_tap_progress.last_progress_at().await;
        if wfb_tee_progress_is_stale(last, Instant::now()) {
            tracing::warn!(
                threshold_s = WFB_TEE_PROGRESS_TIMEOUT.as_secs(),
                "vision_tap_zombie_detected: alive but progress flat; forcing restart"
            );
            return false;
        }
        true
    }

    /// The main service loop. Drives the 5 s health tick, the restart ladders,
    /// and the camera-hotplug-woken retry, terminating on `shutdown` with a
    /// full teardown of every child process.
    pub async fn run(mut self, shutdown: Shutdown) {
        tracing::info!("video_pipeline_service_start");
        let mut tick = tokio::time::interval(HEALTH_CHECK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // 1 Hz telemetry: aggregate the pipeline's encoder metrics once a second
        // (NOT per encoder tick) and ship them to the logging daemon. Best-effort
        // and non-blocking; an absent daemon drops the low-severity samples.
        let telemetry = ados_protocol::logd::emitter::IngestEmitter::new("ados-video");
        let mut telemetry_tick = tokio::time::interval(Duration::from_secs(1));
        telemetry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.wait() => {
                    tracing::info!("video_pipeline_shutdown");
                    break;
                }
                _ = tick.tick() => {
                    self.tick_once(&shutdown).await;
                }
                _ = telemetry_tick.tick() => {
                    self.emit_telemetry(&telemetry);
                }
            }
        }

        // Final teardown: gst air → wfb tee → vision tap → cloud push →
        // encoder → mediamtx. The ManagedProcess Drop killpg is the backstop;
        // this is the graceful ordered path.
        self.stop_gst_air().await;
        self.stop_wfb_tee().await;
        self.stop_vision_tap().await;
        self.stop_cloud_push().await;
        if let Some(mut tap) = self.sei_tap.take() {
            tap.terminate(Duration::from_secs(2)).await;
        }
        if let Some(mut enc) = self.encoder.take() {
            enc.terminate(Duration::from_secs(5)).await;
        }
        self.mediamtx.stop().await;
        tracing::info!("video_pipeline_service_stopped");
    }

    /// Ship the once-a-second encoder telemetry to the logging daemon.
    ///
    /// Emitted only while the pipeline is `Running` (an idle / errored pipeline
    /// has no encoder, so reporting a bitrate / frame rate would be a lie).
    /// `encoder_bitrate_kbps` and `framerate_hz` are the active encoder's
    /// configured output values. `queue_depth_frames` and
    /// `dropped_frames_cumulative` are NOT emitted: the streaming-copy path
    /// (`-c:v copy`) has no source for them, so emitting a constant `0.0` would
    /// be a measured-looking placeholder. They return only when a real
    /// measurement backs them. Tagged with the agent `profile` and the `main`
    /// stream name.
    fn emit_telemetry(&self, emitter: &ados_protocol::logd::emitter::IngestEmitter) {
        use ados_protocol::logd::{Fields, Value};
        if self.state != PipelineState::Running {
            return;
        }
        let profile = self
            .config
            .profile
            .clone()
            .unwrap_or_else(|| "drone".to_string());
        let mut tags = Fields::new();
        tags.insert("profile".to_string(), Value::from(profile));
        tags.insert("stream".to_string(), Value::from("main"));
        emitter.emit_metric(
            "video.encoder_bitrate_kbps",
            self.camera_cfg.bitrate_kbps as f64,
            tags.clone(),
        );
        emitter.emit_metric("video.framerate_hz", self.camera_cfg.fps as f64, tags);
    }

    /// One iteration of the run loop's body, factored out so the `select!`
    /// arm stays small. Handles the Running health ladder and the
    /// Error/Stopped retry-from-error path.
    async fn tick_once(&mut self, shutdown: &Shutdown) {
        match self.state {
            PipelineState::Running => self.tick_running(shutdown).await,
            PipelineState::Error | PipelineState::Stopped => {
                self.tick_retry_from_error(shutdown).await
            }
            PipelineState::Starting => {
                // A transient Starting state outside start_stream() shouldn't
                // persist; nothing to do this tick.
            }
        }
    }

    /// The Running-state health ladder: health → cloud → wfb, each with its own
    /// restart cadence.
    async fn tick_running(&mut self, shutdown: &Shutdown) {
        let health_ok = self.check_health().await;
        if health_ok {
            self.note_healthy_tick();
            self.refresh_camera_state();
        } else {
            self.note_unhealthy_tick();
        }

        if !health_ok {
            self.restart_count += 1;
            if circuit_breaker_tripped(self.restart_count) {
                tracing::error!(
                    attempts = self.restart_count,
                    "pipeline_circuit_breaker: too many failures, waiting 5 minutes"
                );
                self.state = PipelineState::Error;
                // Keep the camera-state sidecar fresh across the long park so a
                // present camera does not read as `unknown` to the staleness
                // gate while the orchestrator is wedged.
                self.refresh_camera_state();
                interruptible_sleep(MAX_RESTART_DELAY, shutdown, &self.camera_plugged, false).await;
                self.restart_count = 0;
                return;
            }
            let delay = backoff_delay(self.restart_count, BASE_RESTART_DELAY, MAX_RESTART_DELAY);
            tracing::warn!(
                attempt = self.restart_count,
                backoff_s = delay.as_secs_f64(),
                "pipeline_health_check_failed: restarting"
            );
            // Back off BEFORE taking the restart lock (the Python discipline:
            // the lock is never held across the long sleep). Refresh the
            // camera-state sidecar before the sleep so it stays fresh while the
            // pipeline is unhealthy.
            self.refresh_camera_state();
            let pre = delay.saturating_sub(HEALTH_CHECK_INTERVAL);
            interruptible_sleep(pre, shutdown, &self.camera_plugged, false).await;
            let _guard = self.restart_lock.clone().lock_owned().await;
            self.stop_stream().await;
            let ok = self.start_stream().await;
            drop(_guard);
            if ok {
                self.restart_count = 0;
            }
            return;
        }

        // Encoder healthy — check the cloud push ladder. The cloud relay is the
        // SECONDARY path: its backoff/park is a non-blocking deadline, never an
        // in-tick sleep, so a configured-but-unreachable relay can never starve
        // the wfb-tee recovery below. A pending `cloud_retry_after` deadline in
        // the future suppresses the branch (the tick falls through to wfb /
        // vision); once the deadline elapses the branch runs, attempts the
        // re-arm, and either succeeds or stamps a fresh deadline. This branch
        // never `return`s, so an unhealthy cloud push no longer short-circuits
        // the radio fan-out.
        let cloud_due = self
            .cloud_retry_after
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(true);
        if cloud_due {
            self.tick_cloud_push().await;
        }

        // Encoder + cloud fine — check the wfb tee ladder (no circuit breaker;
        // Rule 26: video retries forever).
        if !self.check_wfb_tee_health().await {
            if !self.mediamtx.path_ready(MAIN_PATH).await {
                tracing::warn!("wfb_tee_source_down: RTSP source not ready; deferring tee respawn");
                self.stop_wfb_tee().await;
            } else {
                self.wfb_tee_restart_count += 1;
                let delay = backoff_delay(
                    self.wfb_tee_restart_count,
                    BASE_RESTART_DELAY,
                    WFB_TEE_RESTART_CEILING,
                );
                tracing::warn!(
                    attempt = self.wfb_tee_restart_count,
                    backoff_s = delay.as_secs_f64(),
                    "wfb_tee_restarting"
                );
                self.stop_wfb_tee().await;
                let pre = delay.saturating_sub(HEALTH_CHECK_INTERVAL);
                interruptible_sleep(pre, shutdown, &self.camera_plugged, false).await;
                self.start_wfb_tee().await;
                if self.wfb_tee.is_some() {
                    self.wfb_tee_restart_count = 0;
                }
            }
            return;
        }

        // Vision tap ladder (additive, lowest priority). Same shape as the wfb
        // tap: defer the respawn when the RTSP source is down, otherwise back
        // off and restart. No circuit breaker — an additive consumer retries
        // forever and never affects the encode/radio verdict above.
        let vision_supervised = self.vision_enabled() && !self.config.vision.raw_tap;
        if vision_supervised && !self.check_vision_tap_health().await {
            if !self.mediamtx.path_ready(MAIN_PATH).await {
                tracing::warn!(
                    "vision_tap_source_down: RTSP source not ready; deferring tap respawn"
                );
                self.stop_vision_tap().await;
            } else {
                self.vision_tap_restart_count += 1;
                let delay = backoff_delay(
                    self.vision_tap_restart_count,
                    BASE_RESTART_DELAY,
                    WFB_TEE_RESTART_CEILING,
                );
                tracing::warn!(
                    attempt = self.vision_tap_restart_count,
                    backoff_s = delay.as_secs_f64(),
                    "vision_tap_restarting"
                );
                self.stop_vision_tap().await;
                let pre = delay.saturating_sub(HEALTH_CHECK_INTERVAL);
                interruptible_sleep(pre, shutdown, &self.camera_plugged, false).await;
                self.start_vision_tap().await;
                if self.vision_tap.is_some() {
                    self.vision_tap_restart_count = 0;
                }
            }
            return;
        }

        // Healthy tick — if SEI latency is on and the one-shot tap exited (it
        // runs a single read session), respawn it. No circuit breaker (latency
        // telemetry retries forever per Rule 26); deferred when the path is not
        // ready so there is no hot-loop against a dead source.
        if self.sei_latency_on() {
            let exited = self
                .sei_tap
                .as_mut()
                .map(|p| !p.is_running())
                .unwrap_or(true);
            if exited {
                self.sei_tap = None;
                self.start_sei_tap().await;
            }
        }
    }

    /// The non-blocking cloud-push re-arm step, run from `tick_running` only
    /// when any pending `cloud_retry_after` deadline has elapsed.
    ///
    /// Unlike the encoder/wfb ladders this NEVER sleeps the tick: a configured
    /// cloud relay that is unreachable must not stall the primary-radio (wfb)
    /// recovery that runs after it. On an unhealthy push it tears down the
    /// stale process, then either parks for the circuit-breaker window or stamps
    /// a backoff deadline and tries the re-arm immediately (so a transient
    /// outage recovers on the first due tick); a persistent failure re-stamps
    /// the deadline on the following due tick. On a healthy push it clears the
    /// deadline and the counter.
    async fn tick_cloud_push(&mut self) {
        if self.check_cloud_push_health() {
            // Healthy (or not configured / not running) — nothing to do; clear
            // any leftover backoff state.
            self.cloud_retry_after = None;
            self.cloud_restart_count = 0;
            return;
        }
        // Unhealthy: the slot is absent (full-restart re-arm needed) or the
        // process exited. Reap any stale process first.
        self.stop_cloud_push().await;
        self.cloud_restart_count += 1;
        if circuit_breaker_tripped(self.cloud_restart_count) {
            tracing::error!(
                attempts = self.cloud_restart_count,
                "cloud_push_circuit_breaker: parking cloud push 5 minutes (radio recovery continues)"
            );
            self.cloud_retry_after = Some(Instant::now() + MAX_RESTART_DELAY);
            self.cloud_restart_count = 0;
            return;
        }
        let delay = backoff_delay(
            self.cloud_restart_count,
            BASE_RESTART_DELAY,
            MAX_RESTART_DELAY,
        );
        tracing::warn!(
            attempt = self.cloud_restart_count,
            backoff_s = delay.as_secs_f64(),
            "cloud_push_restarting"
        );
        if self.start_cloud_push().await {
            // Re-armed — clear the backoff state.
            self.cloud_retry_after = None;
            self.cloud_restart_count = 0;
        } else {
            // Failed to re-arm — defer the next attempt by the backoff window
            // WITHOUT blocking the tick, so the wfb branch keeps running.
            self.cloud_retry_after = Some(Instant::now() + delay);
        }
    }

    /// The Error/Stopped retry-from-error path: exponential backoff with the
    /// no-camera-vs-real-wedge cap split and a SIGUSR1-woken sleep.
    async fn tick_retry_from_error(&mut self, shutdown: &Shutdown) {
        self.restart_count += 1;
        let cap = retry_cap(self.last_start_error);
        let wake_on_camera = self.last_start_error == StartError::NoPrimaryCamera;
        // Keep the camera-state sidecar fresh across the retry backoff sleep so
        // a present-but-wedged camera (the last-known snapshot) does not read as
        // `unknown` to the staleness gate while the pipeline is parked in the
        // Error state. `start_stream` re-discovers + re-persists on the actual
        // retry; this only covers the long sleep windows.
        self.refresh_camera_state();
        if circuit_breaker_tripped(self.restart_count) {
            tracing::warn!(
                attempts = self.restart_count,
                backoff_s = cap.as_secs_f64(),
                "pipeline_retry_backoff: 10 consecutive failures, backing off"
            );
            interruptible_sleep(cap, shutdown, &self.camera_plugged, wake_on_camera).await;
            self.restart_count = 0;
            return;
        }
        let delay = backoff_delay(self.restart_count, BASE_RESTART_DELAY, cap);
        tracing::info!(
            attempt = self.restart_count,
            backoff_s = delay.as_secs_f64(),
            "pipeline_retry_from_error"
        );
        let pre = delay.saturating_sub(HEALTH_CHECK_INTERVAL);
        interruptible_sleep(pre, shutdown, &self.camera_plugged, wake_on_camera).await;
        if self.start_stream().await {
            self.restart_count = 0;
            tracing::info!("pipeline_recovered: stream started after retry");
        }
    }

    /// Stamp a healthy probe and clear the restart counter once the run has
    /// been continuously healthy for the reset window. Mirrors
    /// `_note_healthy_tick`: the first healthy tick after an unhealthy probe
    /// (or cold start) only stamps `last_healthy_at`; the counter clears only
    /// after the window elapses with no intervening unhealthy tick.
    fn note_healthy_tick(&mut self) {
        let now = Instant::now();
        let Some(since) = self.last_healthy_at else {
            // Armed sentinel — begin the healthy run, do not clear yet.
            self.last_healthy_at = Some(now);
            return;
        };
        if self.restart_count > 0 && healthy_window_elapsed(since, now) {
            tracing::info!(
                window_s = HEALTHY_RESET_WINDOW.as_secs(),
                attempts = self.restart_count,
                "pipeline_restart_counter_reset: healthy window reached"
            );
            self.restart_count = 0;
        }
    }

    /// Re-arm the consecutive-healthy timer on a failed probe so the window has
    /// to start over before the counter can clear. Mirrors `_note_unhealthy_tick`.
    fn note_unhealthy_tick(&mut self) {
        self.last_healthy_at = None;
    }
}

/// Sleep up to `dur`, waking early on shutdown or (when `wake_on_camera`) on a
/// camera-plugged SIGUSR1. A zero / negative duration returns immediately.
/// Mirrors `_sleep_or_wake_on_camera` fused with shutdown awareness.
async fn interruptible_sleep(
    dur: Duration,
    shutdown: &Shutdown,
    camera_plugged: &Arc<Notify>,
    wake_on_camera: bool,
) {
    if dur.is_zero() {
        return;
    }
    if wake_on_camera {
        tokio::select! {
            _ = tokio::time::sleep(dur) => {}
            _ = camera_plugged.notified() => {}
            _ = shutdown.wait() => {}
        }
    } else {
        tokio::select! {
            _ = tokio::time::sleep(dur) => {}
            _ = shutdown.wait() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_orch() -> VideoOrchestrator {
        VideoOrchestrator::new(
            AgentVideoConfig::default(),
            CameraConfig::default(),
            std::path::Path::new("/tmp"),
        )
    }

    #[test]
    fn vision_gate_off_by_default() {
        let o = test_orch();
        // Default config has the vision tap disabled → never enabled.
        assert!(!o.vision_enabled());
        // No tap process slot is occupied at construction.
        assert!(o.vision_tap.is_none());
    }

    #[test]
    fn vision_gate_respects_ground_station_profile() {
        let mut cfg = AgentVideoConfig::default();
        cfg.vision.enabled = true;
        // On a drone profile the gate opens.
        cfg.profile = Some("drone".into());
        let o = VideoOrchestrator::new(
            cfg.clone(),
            CameraConfig::default(),
            std::path::Path::new("/tmp"),
        );
        assert!(o.vision_enabled());
        // On a ground-station profile the air-side tap is suppressed even when
        // enabled in config.
        cfg.profile = Some("ground_station".into());
        let o = VideoOrchestrator::new(cfg, CameraConfig::default(), std::path::Path::new("/tmp"));
        assert!(!o.vision_enabled());
    }

    #[tokio::test]
    async fn vision_tap_health_true_when_not_started_and_not_running() {
        let mut o = test_orch();
        // No tap spawned and the pipeline is not running → healthy (nothing to
        // supervise; the default state is Stopped).
        assert_ne!(o.state, PipelineState::Running);
        assert!(o.check_vision_tap_health().await);
    }

    #[tokio::test]
    async fn vision_tap_health_unhealthy_when_none_but_running() {
        let mut o = test_orch();
        // No tap spawned but the pipeline is running → unhealthy, so the
        // run-loop ladder starts the deferred tap instead of leaving it None
        // forever.
        o.state = PipelineState::Running;
        assert!(o.vision_tap.is_none());
        assert!(!o.check_vision_tap_health().await);
    }

    #[tokio::test]
    async fn vision_tap_health_unhealthy_after_exit() {
        let mut o = test_orch();
        o.state = PipelineState::Running;
        // A tap that has already exited must read unhealthy so the ladder
        // respawns it. `true` exits immediately; wait for it to be reaped.
        let mut p = ManagedProcess::spawn("test-vision-tap", "true", &[]).unwrap();
        for _ in 0..50 {
            if !p.is_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!p.is_running(), "test process should have exited");
        o.vision_tap = Some(p);
        assert!(!o.check_vision_tap_health().await);
        // The exited slot is cleared so the ladder can re-spawn cleanly.
        assert!(o.vision_tap.is_none());
    }

    #[tokio::test]
    async fn start_vision_tap_noop_when_disabled() {
        let mut o = test_orch();
        // Disabled config: start is a no-op even if we force Running.
        o.state = PipelineState::Running;
        o.start_vision_tap().await;
        assert!(o.vision_tap.is_none());
    }

    fn cloud_orch() -> VideoOrchestrator {
        let cfg = AgentVideoConfig {
            cloud_relay_url: Some("rtsp://relay.example.com:8554".into()),
            ..AgentVideoConfig::default()
        };
        assert!(cfg.cloud_enabled());
        VideoOrchestrator::new(cfg, CameraConfig::default(), std::path::Path::new("/tmp"))
    }

    // --- cloud push must re-arm after a full pipeline restart ---------------

    #[test]
    fn cloud_push_health_none_unhealthy_when_enabled_and_running() {
        // After a full restart cloud_push is None; with cloud configured and the
        // pipeline Running the absent slot must read UNHEALTHY so the run-loop
        // ladder re-arms it (the bug: it read healthy and stayed dead).
        let mut o = cloud_orch();
        o.state = PipelineState::Running;
        assert!(o.cloud_push.is_none());
        assert!(!o.check_cloud_push_health());
    }

    #[test]
    fn cloud_push_health_none_healthy_when_not_running() {
        // Absent slot + pipeline not running → correct + healthy (nothing to
        // supervise; e.g. between stop_stream and start_stream).
        let mut o = cloud_orch();
        o.state = PipelineState::Stopped;
        assert!(o.check_cloud_push_health());
    }

    #[test]
    fn cloud_push_health_none_healthy_when_cloud_disabled() {
        // No cloud_relay_url configured → the absent slot is always healthy,
        // even while Running (the local-only default must not loop re-arming).
        let mut o = test_orch();
        assert!(!o.config.cloud_enabled());
        o.state = PipelineState::Running;
        assert!(o.cloud_push.is_none());
        assert!(o.check_cloud_push_health());
    }

    // --- a down cloud relay must not starve wfb recovery --------------------

    #[tokio::test]
    async fn tick_cloud_push_parks_without_blocking_on_breaker_trip() {
        // With the cloud restart counter at the breaker edge, the next
        // unhealthy step must PARK via a future deadline (not sleep the tick)
        // so the wfb branch keeps running. The whole call must return in well
        // under the 300 s park window.
        let mut o = cloud_orch();
        o.state = PipelineState::Running;
        // An exited cloud_push reads unhealthy (reaped in tick_cloud_push).
        let mut p = ManagedProcess::spawn("test-cloud-push", "true", &[]).unwrap();
        for _ in 0..50 {
            if !p.is_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!p.is_running(), "test process should have exited");
        o.cloud_push = Some(p);
        // One increment away from tripping the breaker.
        o.cloud_restart_count = CIRCUIT_BREAKER_ATTEMPTS - 1;

        let before = Instant::now();
        o.tick_cloud_push().await;
        let elapsed = before.elapsed();

        // Returned promptly — it did NOT sleep the 300 s park window.
        assert!(
            elapsed < Duration::from_secs(5),
            "tick_cloud_push blocked for {elapsed:?}; it must never sleep the tick"
        );
        // Parked: a future retry deadline is stamped and the counter reset.
        let deadline = o.cloud_retry_after.expect("a park deadline was stamped");
        assert!(
            deadline > Instant::now(),
            "the deadline must be in the future"
        );
        assert_eq!(o.cloud_restart_count, 0);
        // The stale process slot was reaped.
        assert!(o.cloud_push.is_none());
    }

    #[tokio::test]
    async fn parked_cloud_deadline_suppresses_branch_so_wfb_runs() {
        // The cloud branch is gated on `cloud_due`. A park deadline in the
        // future makes the branch NOT due, so the tick falls through to the wfb
        // / vision branches — the anti-starvation invariant. This asserts the
        // exact gate the run loop uses.
        let mut o = cloud_orch();
        o.cloud_retry_after = Some(Instant::now() + Duration::from_secs(300));
        let cloud_due = o
            .cloud_retry_after
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(true);
        assert!(
            !cloud_due,
            "a future park deadline must suppress the cloud branch"
        );

        // Once the deadline has elapsed the branch becomes due again.
        o.cloud_retry_after = Some(Instant::now() - Duration::from_secs(1));
        let cloud_due = o
            .cloud_retry_after
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(true);
        assert!(
            cloud_due,
            "an elapsed park deadline must re-open the cloud branch"
        );
    }

    #[tokio::test]
    async fn tick_cloud_push_clears_state_when_healthy() {
        // When cloud is disabled the absent slot is healthy; tick_cloud_push
        // must clear any leftover backoff state and not park.
        let mut o = test_orch();
        o.state = PipelineState::Running;
        o.cloud_restart_count = 3;
        o.cloud_retry_after = Some(Instant::now() + Duration::from_secs(10));
        o.tick_cloud_push().await;
        assert!(o.cloud_retry_after.is_none());
        assert_eq!(o.cloud_restart_count, 0);
    }

    // --- camera-state sidecar stays fresh while wedged ----------------------

    fn present_camera_discovery() -> DiscoveryResult {
        DiscoveryResult {
            cameras: vec![crate::discover::DiscoveredCamera {
                name: "HZ USB Camera".into(),
                camera_type: "usb".into(),
                device_path: "/dev/video0".into(),
                width: 1280,
                height: 720,
                capabilities: vec!["h264".into()],
                hardware_role: String::new(),
            }],
            primary: Some(crate::discover::Primary {
                device_path: "/dev/video0".into(),
                name: "HZ USB Camera".into(),
            }),
            total_cameras: 1,
        }
    }

    #[test]
    fn refresh_camera_state_writes_fresh_ready_snapshot_while_wedged() {
        // The wedge/park paths call refresh_camera_state(). With a present
        // camera cached in last_cameras it must (re)write a `ready` snapshot so
        // the staleness gate never drops the camera pill to `unknown` while the
        // orchestrator is stuck. Target a temp path so the write is observable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camera-state.json");
        let mut o = test_orch();
        o.last_cameras = present_camera_discovery();
        o.camera_state_path = Some(path.clone());

        // No sidecar yet.
        assert!(!path.exists());
        o.refresh_camera_state();
        assert!(path.exists(), "the wedge re-persist must write the sidecar");

        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            v["state"], "ready",
            "a present camera must read ready, not unknown"
        );
        assert_eq!(v["primary_path"], "/dev/video0");
        assert_eq!(v["total_cameras"], 1);
        let first_ts = v["updated_at_unix"].as_f64().unwrap();
        assert!(first_ts > 0.0);

        // A second refresh (a later wedge tick) advances the timestamp — the
        // freshness the staleness gate keys on.
        std::thread::sleep(Duration::from_millis(20));
        o.refresh_camera_state();
        let v2: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            v2["updated_at_unix"].as_f64().unwrap() >= first_ts,
            "each wedge re-persist must refresh the timestamp"
        );
    }

    // --- no placeholder telemetry -------------------------------------------

    #[tokio::test]
    async fn emit_telemetry_emits_only_the_two_real_metrics() {
        // The streaming-copy path has no source for queue depth / dropped
        // frames, so emit_telemetry must NOT emit them as constant-0.0
        // placeholders. The emitter's enqueue counter is the capture seam: each
        // emit_metric call enqueues exactly one frame, so the count proves the
        // emitted set. After the fix only two metrics (bitrate + framerate) flow
        // per pass, not four.
        use ados_protocol::logd::emitter::IngestEmitter;

        let mut o = test_orch();
        o.state = PipelineState::Running;
        // A bogus socket path means no live daemon; frames still enqueue into
        // the channel buffer so the enqueue count is exact for a single pass
        // (the channel capacity dwarfs two sends, so nothing drops here).
        let emitter =
            IngestEmitter::with_socket("ados-video-test", "/nonexistent/ados/ingest.sock");
        let stats = emitter.stats();
        o.emit_telemetry(&emitter);
        assert_eq!(
            stats.enqueued() + stats.dropped(),
            2,
            "exactly two metrics must be produced (bitrate + framerate); the two placeholders are gone"
        );
    }

    #[tokio::test]
    async fn emit_telemetry_silent_when_not_running() {
        // An idle / errored pipeline has no encoder, so no telemetry is emitted.
        use ados_protocol::logd::emitter::IngestEmitter;
        let o = test_orch();
        assert_ne!(o.state, PipelineState::Running);
        let emitter =
            IngestEmitter::with_socket("ados-video-test", "/nonexistent/ados/ingest.sock");
        let stats = emitter.stats();
        o.emit_telemetry(&emitter);
        assert_eq!(stats.enqueued() + stats.dropped(), 0);
    }

    #[test]
    fn note_healthy_tick_sentinel_state_machine() {
        let mut o = test_orch();
        // Cold start: armed sentinel.
        assert_eq!(o.last_healthy_at, None);
        // A failing run accrued a couple of restarts.
        o.restart_count = 3;
        // First healthy tick only stamps the sentinel; counter is NOT cleared
        // (the window hasn't elapsed yet).
        o.note_healthy_tick();
        assert!(o.last_healthy_at.is_some());
        assert_eq!(o.restart_count, 3);
        // Backdate the stamp past the window, then a healthy tick clears it.
        o.last_healthy_at = Some(Instant::now() - Duration::from_secs(61));
        o.note_healthy_tick();
        assert_eq!(o.restart_count, 0);
        // An unhealthy tick re-arms the sentinel.
        o.restart_count = 2;
        o.note_unhealthy_tick();
        assert_eq!(o.last_healthy_at, None);
        // Next healthy tick re-stamps (does not clear) — the window restarts.
        o.note_healthy_tick();
        assert!(o.last_healthy_at.is_some());
        assert_eq!(o.restart_count, 2);
    }
}
