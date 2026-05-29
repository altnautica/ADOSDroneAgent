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
use crate::encoder::{
    augment_encoder_with_raw_tap, binary_present, build_encoder_command, detect_encoder_for_camera,
    wrap_with_sei_inject, EncoderEnv, EncoderKind, EncoderParams,
};
use crate::mediamtx::{MediamtxManager, MAIN_PATH};
use crate::process::{kill_orphans, ManagedProcess};
use crate::shutdown::Shutdown;
use crate::tap::{self, spawn_vision_tap};
use crate::wfb_tee::{
    drain_wfb_tee_stderr, orphan_pattern, spawn_wfb_tee, wfb_tee_progress_is_stale,
    ProgressTracker, WFB_TEE_PROGRESS_TIMEOUT,
};

// --- tunables (mirror constants.py + pipeline.py) ----------------------------

/// Health-tick cadence (`_HEALTH_CHECK_INTERVAL`).
pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Max startup grace before a publisher-less pipeline is declared dead
/// (`_STARTUP_GRACE_MAX_SECS`).
pub const STARTUP_GRACE_MAX: Duration = Duration::from_secs(30);
/// Inbound-byte stall window (`_INBOUND_FLOW_STALL_SECONDS`).
pub const INBOUND_FLOW_STALL: Duration = Duration::from_secs(12);
/// Base restart delay (`_base_restart_delay`).
pub const BASE_RESTART_DELAY: Duration = Duration::from_secs(5);
/// Cap on the exponential restart backoff for a real wedge (`_max_restart_delay`).
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(300);
/// Tighter cap when the failure is "no primary camera" — a USB hotplug
/// condition that resolves in seconds (`_max_restart_delay_no_camera`).
pub const MAX_RESTART_DELAY_NO_CAMERA: Duration = Duration::from_secs(30);
/// Consecutive-healthy window that clears the restart counter
/// (`_healthy_reset_window_secs`).
pub const HEALTHY_RESET_WINDOW: Duration = Duration::from_secs(60);
/// Ceiling on the wfb-tee restart backoff (the Python `min(..., 5.0)`).
pub const WFB_TEE_RESTART_CEILING: Duration = Duration::from_secs(5);
/// Consecutive-failure count that trips the 5-minute circuit-breaker park.
pub const CIRCUIT_BREAKER_ATTEMPTS: u32 = 10;

/// Pipeline lifecycle state (`PipelineState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineState {
    Stopped,
    Starting,
    Running,
    Error,
}

/// The tagged cause of the most recent `start_stream` failure, so the retry
/// loop can pick the right backoff cap (`_last_start_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// No camera won the auto-assign — transient USB hotplug; 30 s cap.
    NoPrimaryCamera,
    /// No encoder backend available for the camera.
    NoEncoder,
    /// The encoder subprocess failed to spawn.
    EncoderSpawnFailed,
    /// mediamtx failed to start.
    MediamtxFailed,
    /// The last start succeeded or the cause is unknown — 5-minute cap.
    None,
}

// --- pure health-decision functions (testable without subprocesses) ----------

/// Exponential backoff with a cap, in the Python `min(base * 2^(n-1), cap)`
/// shape. `attempt` is 1-based.
pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    // 2^(attempt-1), saturating so a large attempt count cannot overflow.
    let shift = attempt - 1;
    let factor: u64 = 1u64.checked_shl(shift.min(63)).unwrap_or(u64::MAX);
    let scaled = base
        .as_secs_f64()
        .mul_add(factor as f64, 0.0)
        .min(cap.as_secs_f64());
    Duration::from_secs_f64(scaled)
}

/// Pick the backoff cap for the error-state retry: the no-camera cap when the
/// last failure was a missing primary, otherwise the full 5-minute cap.
pub fn retry_cap(last_error: StartError) -> Duration {
    match last_error {
        StartError::NoPrimaryCamera => MAX_RESTART_DELAY_NO_CAMERA,
        _ => MAX_RESTART_DELAY,
    }
}

/// Should the circuit breaker trip (park for 5 minutes and reset the counter)?
pub fn circuit_breaker_tripped(restart_count: u32) -> bool {
    restart_count >= CIRCUIT_BREAKER_ATTEMPTS
}

/// Should a sustained-healthy run clear the restart counter? True once the
/// pipeline has been continuously healthy for strictly longer than
/// [`HEALTHY_RESET_WINDOW`] (the Python `> window` comparison).
pub fn healthy_window_elapsed(healthy_since: Instant, now: Instant) -> bool {
    now.saturating_duration_since(healthy_since) > HEALTHY_RESET_WINDOW
}

/// The decision the startup-grace branch of the health check makes, given the
/// mediamtx publisher probe + elapsed time. Pure so it is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceDecision {
    /// A publisher appeared — latch first-packet and report healthy.
    FirstPacket,
    /// Still inside the grace window with no publisher yet — report healthy.
    StillWaiting,
    /// Grace expired with no publisher — report unhealthy (restart).
    Expired,
}

/// Grace-window decision (`_check_health` pre-first-packet block).
pub fn grace_decision(path_ready: bool, elapsed: Duration) -> GraceDecision {
    if path_ready {
        GraceDecision::FirstPacket
    } else if elapsed < STARTUP_GRACE_MAX {
        GraceDecision::StillWaiting
    } else {
        GraceDecision::Expired
    }
}

/// The inbound-byte watchdog decision (`_check_inbound_flow_healthy`), as a
/// pure transition over the prior counter sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InboundDecision {
    /// First sample — seed the counter, report healthy.
    Seed,
    /// Counter advanced — report healthy, record the new bytes/s.
    Advanced { bytes_per_s: f64 },
    /// Counter flat but still within the stall window — report healthy.
    WithinStall,
    /// Counter flat past the stall window — report unhealthy (restart publish).
    Stalled,
}

/// Inbound-flow decision over a new `current` byte sample.
///
/// `prev` is the previously recorded counter (`< 0` ⇒ no sample yet).
/// `since_change` is how long the counter has sat flat. `interval` floors the
/// elapsed used for the bytes/s rate so a zero-elapsed sample cannot divide by
/// zero (mirrors the Python `max(now - changed_at, interval)`).
pub fn inbound_decision(
    prev: i64,
    current: i64,
    since_change: Duration,
    interval: Duration,
) -> InboundDecision {
    if prev < 0 {
        return InboundDecision::Seed;
    }
    if current > prev {
        let delta = (current - prev) as f64;
        let elapsed = since_change.max(interval).as_secs_f64();
        return InboundDecision::Advanced {
            bytes_per_s: delta / elapsed,
        };
    }
    if since_change < INBOUND_FLOW_STALL {
        InboundDecision::WithinStall
    } else {
        InboundDecision::Stalled
    }
}

// --- orchestrator ------------------------------------------------------------

/// Owns and supervises the air-side video pipeline subprocess tree.
pub struct VideoOrchestrator {
    config: AgentVideoConfig,
    camera_cfg: CameraConfig,

    mediamtx: MediamtxManager,
    encoder: Option<ManagedProcess>,
    wfb_tee: Option<ManagedProcess>,
    cloud_push: Option<ManagedProcess>,
    sei_tap: Option<ManagedProcess>,
    /// The decoupled vision frame tap (a third ffmpeg → rawvideo → sink).
    /// Spawned only when `video.vision.enabled` and `raw_tap` is off. Additive:
    /// a crash here never touches the encode or wfb path.
    vision_tap: Option<ManagedProcess>,
    /// The GStreamer air pipeline (deferred — kept as a supervised Python
    /// subprocess slot for the future GST branch; never spawned on the legacy
    /// path).
    gst_air: Option<ManagedProcess>,

    wfb_tee_progress: ProgressTracker,
    /// Output-progress clock for the decoupled vision tap (Rule 37: liveness
    /// alone is never proof of work; the tap can hold the sink open while
    /// pushing nothing).
    vision_tap_progress: ProgressTracker,

    last_cameras: DiscoveryResult,
    encoder_type: Option<EncoderKind>,

    state: PipelineState,
    started_at: Instant,
    first_packet_seen: bool,

    /// mediamtx inbound-byte counter (`-1` ⇒ no sample yet).
    inbound_bytes_value: i64,
    inbound_bytes_changed_at: Instant,
    video_inbound_bytes_per_s: f64,

    /// The instant a healthy run began, or `None` (the armed sentinel,
    /// Python's `0.0`) when the next healthy tick should re-stamp it. A failed
    /// probe re-arms it to `None`.
    last_healthy_at: Option<Instant>,

    restart_count: u32,
    cloud_restart_count: u32,
    wfb_tee_restart_count: u32,
    vision_tap_restart_count: u32,
    last_start_error: StartError,

    /// Serializes teardown+respawn across the cold start, the health-check
    /// restart, and any camera switch (held only around the bounded region,
    /// never across a backoff sleep).
    restart_lock: Arc<Mutex<()>>,
    /// Serializes camera-switch operations (reserved for the switch API).
    #[allow(dead_code)]
    switch_lock: Arc<Mutex<()>>,
    /// Fired by SIGUSR1 (a fresh `/dev/video*` node) to short-circuit the
    /// no-primary backoff sleep.
    camera_plugged: Arc<Notify>,

    python_executable: String,
    env: EncoderEnv,
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
            restart_lock: Arc::new(Mutex::new(())),
            switch_lock: Arc::new(Mutex::new(())),
            camera_plugged: Arc::new(Notify::new()),
            python_executable: discover::python_executable(),
            env: EncoderEnv::detect(),
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

    fn pipe_uri(&self) -> String {
        format!("rtsp://localhost:{}/main", self.mediamtx.rtsp_port())
    }

    fn sei_latency_on(&self) -> bool {
        self.config.wfb.sei_latency
    }

    /// Start the encoding + streaming pipeline. Returns `true` on success.
    ///
    /// Exact order mirrors `pipeline.py::start_stream`: reap stale encoder →
    /// discover + persist camera-state → bail on no-primary → orphan sweeps →
    /// (GST branch deferred) → detect encoder → build command → optional SEI
    /// wrap → mediamtx config+start → spawn encoder + plain stderr drain →
    /// latch Running → best-effort wfb-tee → optional SEI tap. cloud_push is
    /// NOT started here.
    pub async fn start_stream(&mut self) -> bool {
        if self.state == PipelineState::Running {
            tracing::warn!("pipeline_already_running");
            return true;
        }

        // Reap any stale encoder from a prior cycle by process group.
        if let Some(mut enc) = self.encoder.take() {
            if enc.is_running() {
                tracing::info!(pid = enc.pid(), "killing_stale_encoder");
                enc.terminate(Duration::from_secs(5)).await;
            }
        }

        self.state = PipelineState::Starting;

        // Discover cameras and persist the camera-state sidecar.
        let discovery =
            discover::discover(&self.python_executable, discover::DISCOVERY_TIMEOUT).await;
        discover::persist_camera_state(&discovery);
        self.last_cameras = discovery;

        let Some(primary) = self.last_cameras.primary_camera_info() else {
            tracing::error!("no_primary_camera");
            self.last_start_error = StartError::NoPrimaryCamera;
            self.state = PipelineState::Error;
            return false;
        };
        let device_path = primary.device_path.clone();

        // Orphan sweeps in the exact Python order: encoder holding the camera
        // node, rpicam-vid, then the bridge publisher to /main.
        kill_orphans(&format!("-i {device_path}")).await;
        kill_orphans("rpicam-vid").await;
        let pipe_uri = self.pipe_uri();
        kill_orphans(&pipe_uri).await;

        // GST air pipeline branch is deferred: the GStreamer AirPipeline stays
        // a supervised Python subprocess in a later gated step. On the legacy
        // path we never take it; if the flag is set we log and fall through to
        // the bash path (which is exactly the Python fallback behaviour).
        if self.config.use_gst_air_pipeline {
            tracing::warn!("gst_air_pipeline_requested_but_deferred; using legacy bash air path");
        }

        // Detect the encoder backend for the primary camera.
        let kind = detect_encoder_for_camera(
            primary.camera_type,
            binary_present("rpicam-vid"),
            binary_present("ffmpeg"),
            binary_present("gst-launch-1.0"),
        );
        let Some(kind) = kind else {
            tracing::error!("no_encoder_available");
            self.encoder_type = None;
            self.last_start_error = StartError::NoEncoder;
            self.state = PipelineState::Error;
            return false;
        };
        self.encoder_type = Some(kind);

        // Build the encoder command.
        let params = EncoderParams::from_camera_config(kind, &self.camera_cfg);
        let cmd = match build_encoder_command(
            &params,
            &device_path,
            &pipe_uri,
            Some(&primary),
            &self.env,
        ) {
            Ok(c) if !c.is_empty() => c,
            Ok(_) => {
                tracing::error!("encoder_command_empty");
                self.state = PipelineState::Error;
                return false;
            }
            Err(e) => {
                tracing::error!(error = %e, "encoder_command_build_failed");
                self.state = PipelineState::Error;
                return false;
            }
        };

        // Optional SEI wrap upstream of mediamtx so every consumer sees the
        // same wall-clock marker on the same frame.
        let cmd = if self.sei_latency_on() {
            tracing::info!(encoder = ?kind, "sei_inject_upstream_of_mediamtx");
            wrap_with_sei_inject(&cmd, &pipe_uri, &self.env)
        } else {
            cmd
        };

        // Opt-in pre-encode vision tap: augment the encoder command with a
        // strictly-appended second rawvideo output to the vision sink, WITHOUT
        // changing the existing encode/RTSP output bytes. No-op (returns the
        // command unchanged) unless the command is a raw ffmpeg invocation
        // ending in the RTSP output — bash-pipeline / gstreamer / SEI-wrapped
        // commands fall back to the decoupled third-ffmpeg tap below, which
        // never touches the encoder. Off by default.
        let cmd = if self.vision_enabled() && self.config.vision.raw_tap {
            let v = &self.config.vision;
            let augmented = augment_encoder_with_raw_tap(
                &cmd,
                &pipe_uri,
                v.fps,
                v.width,
                v.height,
                v.pixel_format(),
                &v.sink,
            );
            if augmented.len() != cmd.len() {
                tracing::info!(
                    sink = %v.sink,
                    "vision_raw_tap_spliced_into_encoder"
                );
            } else {
                tracing::info!(
                    "vision_raw_tap_requested_but_command_not_eligible; using decoupled tap"
                );
            }
            augmented
        } else {
            cmd
        };

        // Configure + start mediamtx (gates on the RTSP port internally).
        if let Err(e) = self
            .mediamtx
            .write_config(&[(MAIN_PATH.to_string(), "publisher".to_string())])
        {
            tracing::error!(error = %e, "mediamtx_config_write_failed");
            self.last_start_error = StartError::MediamtxFailed;
            self.state = PipelineState::Error;
            return false;
        }
        match self.mediamtx.start().await {
            Ok(true) => {}
            _ => {
                tracing::error!("mediamtx_start_failed; cannot stream without mediamtx");
                self.last_start_error = StartError::MediamtxFailed;
                self.state = PipelineState::Error;
                return false;
            }
        }

        // Spawn the encoder. A PLAIN rate-limited stderr drain (no progress
        // tracker — the encoder is the source, not the wfb tap).
        let program = cmd[0].clone();
        let args: Vec<String> = cmd[1..].to_vec();
        let mut enc = match ManagedProcess::spawn("encoder", &program, &args) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, encoder = ?kind, "encoder_spawn_failed");
                self.last_start_error = StartError::EncoderSpawnFailed;
                self.teardown_after_partial_start().await;
                return false;
            }
        };
        if let Some(stderr) = enc.take_stderr() {
            tokio::spawn(crate::stderr_drain::drain_plain(stderr, "encoder"));
        }
        self.encoder = Some(enc);

        // Latch Running + reset all health counters for a clean cold-start
        // window.
        self.state = PipelineState::Running;
        let now = Instant::now();
        self.started_at = now;
        self.first_packet_seen = false;
        self.inbound_bytes_value = -1;
        self.inbound_bytes_changed_at = now;
        self.video_inbound_bytes_per_s = 0.0;
        // Arm the healthy-window sentinel: the first healthy tick stamps it,
        // matching the Python pipeline (start_stream does not touch it).
        self.last_healthy_at = None;
        self.last_start_error = StartError::None;
        tracing::info!(encoder = ?kind, "pipeline_started");

        // Best-effort radio fan-out + optional SEI tap.
        self.start_wfb_tee().await;
        if self.sei_latency_on() {
            self.start_sei_tap().await;
        }
        // Optional additive vision frame tap. When raw_tap is on the frames are
        // already produced by the spliced encoder output, so no separate
        // process is spawned; otherwise spawn the decoupled third ffmpeg.
        if self.vision_enabled() && !self.config.vision.raw_tap {
            self.start_vision_tap().await;
        }
        true
    }

    /// True when the additive vision frame tap is configured to run on this
    /// node: enabled in config AND this is not a ground-station profile (the
    /// air-side pipeline never runs on a ground station, so neither does the
    /// tap). This is the single gate the tap consults.
    fn vision_enabled(&self) -> bool {
        self.config.vision.enabled && !self.config.is_ground_station()
    }

    /// Spawn the wfb radio tap (idempotent). Best-effort: a failure leaves the
    /// rest of the pipeline up. Mirrors `start_wfb_tee`.
    pub async fn start_wfb_tee(&mut self) {
        if self.state != PipelineState::Running {
            tracing::warn!("wfb_tee_skipped: pipeline not running");
            return;
        }
        if let Some(p) = self.wfb_tee.as_mut() {
            if p.is_running() {
                return;
            }
        }
        // Sweep stale ffmpegs fighting for UDP 5600 before respawn.
        kill_orphans(&orphan_pattern()).await;
        let mut tee = match spawn_wfb_tee(self.mediamtx.rtsp_port()) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "wfb_tee_start_failed");
                return;
            }
        };
        // Fresh tracker so the just-spawned tap gets the full progress window.
        let tracker = ProgressTracker::new();
        if let Some(stderr) = tee.take_stderr() {
            tokio::spawn(drain_wfb_tee_stderr(stderr, tracker.clone()));
        }
        self.wfb_tee_progress = tracker;
        self.wfb_tee = Some(tee);
        tracing::info!(sei_latency = self.sei_latency_on(), "wfb_tee_started");
    }

    /// Stop the wfb radio tap. Mirrors `stop_wfb_tee`.
    pub async fn stop_wfb_tee(&mut self) {
        if let Some(mut p) = self.wfb_tee.take() {
            p.terminate(Duration::from_secs(5)).await;
        }
        // Belt-and-suspenders orphan sweep.
        kill_orphans(&orphan_pattern()).await;
    }

    /// Spawn the decoupled vision frame tap (idempotent). Best-effort and
    /// strictly additive: a failure leaves the encode + radio path fully up.
    /// Mirrors [`start_wfb_tee`](Self::start_wfb_tee) — same setsid/killpg
    /// ownership, same orphan sweep, same output-progress watchdog.
    pub async fn start_vision_tap(&mut self) {
        if !self.vision_enabled() || self.config.vision.raw_tap {
            return;
        }
        if self.state != PipelineState::Running {
            tracing::warn!("vision_tap_skipped: pipeline not running");
            return;
        }
        if let Some(p) = self.vision_tap.as_mut() {
            if p.is_running() {
                return;
            }
        }
        let v = &self.config.vision;
        // Sweep any stale reader holding the sink before respawn.
        kill_orphans(&tap::orphan_pattern(&v.sink)).await;
        let mut t = match spawn_vision_tap(
            self.mediamtx.rtsp_port(),
            v.fps,
            v.width,
            v.height,
            v.pixel_format(),
            &v.sink,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "vision_tap_start_failed");
                return;
            }
        };
        // Fresh tracker so the just-spawned tap gets the full progress window.
        let tracker = ProgressTracker::new();
        if let Some(stderr) = t.take_stderr() {
            tokio::spawn(drain_wfb_tee_stderr(stderr, tracker.clone()));
        }
        self.vision_tap_progress = tracker;
        self.vision_tap = Some(t);
        tracing::info!(
            sink = %v.sink,
            fps = v.fps,
            width = v.width,
            height = v.height,
            format = %v.pixel_format(),
            "vision_tap_started"
        );
    }

    /// Stop the decoupled vision frame tap. Mirrors
    /// [`stop_wfb_tee`](Self::stop_wfb_tee).
    pub async fn stop_vision_tap(&mut self) {
        if let Some(mut p) = self.vision_tap.take() {
            p.terminate(Duration::from_secs(5)).await;
        }
        // Belt-and-suspenders orphan sweep for the sink.
        kill_orphans(&tap::orphan_pattern(&self.config.vision.sink)).await;
    }

    /// Spawn the headless SEI latency tap as a one-shot Python subprocess,
    /// gated on the mediamtx path being ready. Mirrors the SEI-tap spawn but
    /// runs `--once` so the Rust orchestrator owns the restart cadence (no
    /// 2 s Python hot-loop — that was the deferred-respawn bug).
    pub async fn start_sei_tap(&mut self) {
        if let Some(p) = self.sei_tap.as_mut() {
            if p.is_running() {
                return;
            }
        }
        // Only spawn once a publisher exists; otherwise defer to the health
        // tick (no hot-loop against a dead source).
        if !self.mediamtx.path_ready(MAIN_PATH).await {
            tracing::debug!("sei_tap_deferred: mediamtx path not ready");
            return;
        }
        let pipe_uri = self.pipe_uri();
        let args: Vec<String> = vec![
            "-m".into(),
            "ados.services.video.sei_tap".into(),
            "--once".into(),
            "--rtsp".into(),
            pipe_uri,
        ];
        let mut tap = match ManagedProcess::spawn("sei_tap", &self.python_executable, &args) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "headless_sei_tap_spawn_failed");
                return;
            }
        };
        if let Some(stderr) = tap.take_stderr() {
            tokio::spawn(crate::stderr_drain::drain_plain(stderr, "sei_tap"));
        }
        self.sei_tap = Some(tap);
        tracing::info!("headless_sei_tap_started");
    }

    /// Start the cloud-relay push (an ffmpeg that copies local RTSP to the
    /// cloud relay). Mirrors `start_cloud_push`. Returns `true` on spawn.
    pub async fn start_cloud_push(&mut self) -> bool {
        let Some(cloud_url) = self
            .config
            .cloud_relay_url
            .clone()
            .filter(|s| !s.is_empty())
        else {
            tracing::info!("cloud_push_disabled: no cloud_relay_url configured");
            return false;
        };
        if self.state != PipelineState::Running {
            tracing::warn!("cloud_push_skipped: pipeline not running");
            return false;
        }
        if let Some(p) = self.cloud_push.as_mut() {
            if p.is_running() {
                return true;
            }
        }
        let local_rtsp = format!("rtsp://localhost:{}/main", self.mediamtx.rtsp_port());
        let push_url = format!("{cloud_url}/main");
        let args: Vec<String> = vec![
            "-rtsp_transport".into(),
            "tcp".into(),
            "-timeout".into(),
            "5000000".into(),
            "-i".into(),
            local_rtsp,
            "-c".into(),
            "copy".into(),
            "-f".into(),
            "rtsp".into(),
            "-rtsp_transport".into(),
            "tcp".into(),
            push_url.clone(),
        ];
        let mut push = match ManagedProcess::spawn("cloud_push", "ffmpeg", &args) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "cloud_push_ffmpeg_spawn_failed");
                return false;
            }
        };
        if let Some(stderr) = push.take_stderr() {
            tokio::spawn(crate::stderr_drain::drain_plain(stderr, "cloud_push"));
        }
        self.cloud_push = Some(push);
        tracing::info!(destination = %push_url, "cloud_push_started");
        true
    }

    /// Stop the cloud push. Mirrors `stop_cloud_push`.
    pub async fn stop_cloud_push(&mut self) {
        if let Some(mut p) = self.cloud_push.take() {
            p.terminate(Duration::from_secs(5)).await;
            tracing::info!("cloud_push_stopped");
        }
    }

    /// Tear down the deferred GStreamer air-pipeline subprocess if one was ever
    /// spawned. On the legacy bash path this slot is always empty; the teardown
    /// is here so a future gated GST step inherits correct reaping for free.
    async fn stop_gst_air(&mut self) {
        if let Some(mut gst) = self.gst_air.take() {
            gst.terminate(Duration::from_secs(5)).await;
        }
    }

    /// Roll back a partial start: tear down anything spawned after
    /// mediamtx.start(), then mark Error. Mirrors `_teardown_after_partial_start`.
    async fn teardown_after_partial_start(&mut self) {
        self.stop_gst_air().await;
        self.stop_wfb_tee().await;
        self.stop_vision_tap().await;
        if let Some(mut tap) = self.sei_tap.take() {
            tap.terminate(Duration::from_secs(2)).await;
        }
        if let Some(mut enc) = self.encoder.take() {
            enc.terminate(Duration::from_secs(2)).await;
        }
        self.mediamtx.stop().await;
        self.state = PipelineState::Error;
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

    /// Is the cloud push still alive? `true` when healthy or not configured.
    /// Mirrors `_check_cloud_push_health`.
    fn check_cloud_push_health(&mut self) -> bool {
        match self.cloud_push.as_mut() {
            None => true,
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
            return true;
        };
        if !p.is_running() {
            tracing::warn!("wfb_tee_process_exited");
            self.wfb_tee = None;
            return true; // the run loop will respawn via the wfb ladder
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
            return true;
        };
        if !p.is_running() {
            tracing::warn!("vision_tap_process_exited");
            self.vision_tap = None;
            return true; // the run loop will respawn via the vision ladder
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

        loop {
            tokio::select! {
                _ = shutdown.wait() => {
                    tracing::info!("video_pipeline_shutdown");
                    break;
                }
                _ = tick.tick() => {
                    self.tick_once(&shutdown).await;
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
            discover::persist_camera_state(&self.last_cameras);
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
            // the lock is never held across the long sleep).
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

        // Encoder healthy — check the cloud push ladder.
        if !self.check_cloud_push_health() {
            self.cloud_restart_count += 1;
            let delay = backoff_delay(
                self.cloud_restart_count,
                BASE_RESTART_DELAY,
                MAX_RESTART_DELAY,
            );
            if circuit_breaker_tripped(self.cloud_restart_count) {
                tracing::error!(
                    attempts = self.cloud_restart_count,
                    "cloud_push_circuit_breaker: waiting 5 minutes"
                );
                interruptible_sleep(MAX_RESTART_DELAY, shutdown, &self.camera_plugged, false).await;
                self.cloud_restart_count = 0;
            } else {
                tracing::warn!(
                    attempt = self.cloud_restart_count,
                    backoff_s = delay.as_secs_f64(),
                    "cloud_push_restarting"
                );
                self.stop_cloud_push().await;
                let pre = delay.saturating_sub(HEALTH_CHECK_INTERVAL);
                interruptible_sleep(pre, shutdown, &self.camera_plugged, false).await;
                if self.start_cloud_push().await {
                    self.cloud_restart_count = 0;
                }
            }
            return;
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

    /// The Error/Stopped retry-from-error path: exponential backoff with the
    /// no-camera-vs-real-wedge cap split and a SIGUSR1-woken sleep.
    async fn tick_retry_from_error(&mut self, shutdown: &Shutdown) {
        self.restart_count += 1;
        let cap = retry_cap(self.last_start_error);
        let wake_on_camera = self.last_start_error == StartError::NoPrimaryCamera;
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

    #[test]
    fn backoff_ladder_matches_python_shape() {
        // base 5s, cap 300s → 5,10,20,40,80,160,300(capped),300,...
        assert_eq!(
            backoff_delay(1, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff_delay(2, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(10)
        );
        assert_eq!(
            backoff_delay(3, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(20)
        );
        assert_eq!(
            backoff_delay(4, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(40)
        );
        assert_eq!(
            backoff_delay(5, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(80)
        );
        assert_eq!(
            backoff_delay(6, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(160)
        );
        // 7 → 320 capped to 300.
        assert_eq!(
            backoff_delay(7, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(300)
        );
        assert_eq!(
            backoff_delay(20, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(300)
        );
        // attempt 0 is a no-op (defensive).
        assert_eq!(
            backoff_delay(0, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::ZERO
        );
    }

    #[test]
    fn no_camera_cap_is_30s() {
        // base 5s, cap 30s → 5,10,20,30(capped),30,...
        assert_eq!(
            backoff_delay(1, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff_delay(2, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(10)
        );
        assert_eq!(
            backoff_delay(3, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(20)
        );
        // 4 → 40 capped to 30.
        assert_eq!(
            backoff_delay(4, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(30)
        );
        assert_eq!(
            backoff_delay(10, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn wfb_tee_ceiling_is_5s() {
        // wfb tee backoff caps at 5s regardless of attempt.
        assert_eq!(
            backoff_delay(1, BASE_RESTART_DELAY, WFB_TEE_RESTART_CEILING),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff_delay(2, BASE_RESTART_DELAY, WFB_TEE_RESTART_CEILING),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff_delay(5, BASE_RESTART_DELAY, WFB_TEE_RESTART_CEILING),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn retry_cap_picks_by_error_class() {
        assert_eq!(
            retry_cap(StartError::NoPrimaryCamera),
            MAX_RESTART_DELAY_NO_CAMERA
        );
        assert_eq!(retry_cap(StartError::NoEncoder), MAX_RESTART_DELAY);
        assert_eq!(retry_cap(StartError::EncoderSpawnFailed), MAX_RESTART_DELAY);
        assert_eq!(retry_cap(StartError::MediamtxFailed), MAX_RESTART_DELAY);
        assert_eq!(retry_cap(StartError::None), MAX_RESTART_DELAY);
    }

    #[test]
    fn circuit_breaker_trips_at_ten() {
        assert!(!circuit_breaker_tripped(9));
        assert!(circuit_breaker_tripped(10));
        assert!(circuit_breaker_tripped(11));
    }

    #[test]
    fn healthy_window_boundary_at_60s() {
        let base = Instant::now();
        // Strict `>` (the Python `now - last > window`): exactly 60s is NOT
        // elapsed; just past 60s is.
        assert!(!healthy_window_elapsed(
            base,
            base + Duration::from_millis(59_999)
        ));
        assert!(!healthy_window_elapsed(
            base,
            base + Duration::from_secs(60)
        ));
        assert!(healthy_window_elapsed(
            base,
            base + Duration::from_millis(60_001)
        ));
        assert!(healthy_window_elapsed(
            base,
            base + Duration::from_secs(120)
        ));
    }

    #[test]
    fn grace_decision_transitions() {
        // Publisher present at any time → first packet.
        assert_eq!(
            grace_decision(true, Duration::ZERO),
            GraceDecision::FirstPacket
        );
        assert_eq!(
            grace_decision(true, Duration::from_secs(40)),
            GraceDecision::FirstPacket
        );
        // No publisher inside the window → still waiting.
        assert_eq!(
            grace_decision(false, Duration::from_secs(5)),
            GraceDecision::StillWaiting
        );
        assert_eq!(
            grace_decision(false, Duration::from_millis(29_999)),
            GraceDecision::StillWaiting
        );
        // No publisher past the window → expired.
        assert_eq!(
            grace_decision(false, Duration::from_secs(30)),
            GraceDecision::Expired
        );
        assert_eq!(
            grace_decision(false, Duration::from_secs(45)),
            GraceDecision::Expired
        );
    }

    #[test]
    fn inbound_decision_seed_on_first_sample() {
        assert_eq!(
            inbound_decision(-1, 1000, Duration::ZERO, HEALTH_CHECK_INTERVAL),
            InboundDecision::Seed
        );
    }

    #[test]
    fn inbound_decision_advanced_computes_rate() {
        // 10000 bytes over a 5s floored interval → 2000 B/s.
        let d = inbound_decision(0, 10_000, Duration::from_secs(5), HEALTH_CHECK_INTERVAL);
        match d {
            InboundDecision::Advanced { bytes_per_s } => {
                assert!((bytes_per_s - 2000.0).abs() < 1e-6, "got {bytes_per_s}");
            }
            other => panic!("expected Advanced, got {other:?}"),
        }
        // A sub-interval elapsed is floored to the interval so the rate cannot
        // spike artificially.
        let d = inbound_decision(0, 5_000, Duration::from_millis(100), HEALTH_CHECK_INTERVAL);
        match d {
            InboundDecision::Advanced { bytes_per_s } => {
                // floored to 5s → 1000 B/s, not 50000 B/s.
                assert!((bytes_per_s - 1000.0).abs() < 1e-6, "got {bytes_per_s}");
            }
            other => panic!("expected Advanced, got {other:?}"),
        }
    }

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
    async fn vision_tap_health_true_when_not_started() {
        let mut o = test_orch();
        // No tap spawned → health is vacuously true (nothing to supervise).
        assert!(o.check_vision_tap_health().await);
    }

    #[tokio::test]
    async fn start_vision_tap_noop_when_disabled() {
        let mut o = test_orch();
        // Disabled config: start is a no-op even if we force Running.
        o.state = PipelineState::Running;
        o.start_vision_tap().await;
        assert!(o.vision_tap.is_none());
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

    #[test]
    fn inbound_decision_within_and_past_stall() {
        // Flat counter inside the 12s stall window → still healthy.
        assert_eq!(
            inbound_decision(1000, 1000, Duration::from_secs(11), HEALTH_CHECK_INTERVAL),
            InboundDecision::WithinStall
        );
        // Flat counter at exactly 12s → stalled.
        assert_eq!(
            inbound_decision(1000, 1000, Duration::from_secs(12), HEALTH_CHECK_INTERVAL),
            InboundDecision::Stalled
        );
        // Counter went backwards (mediamtx path reset) is treated as flat →
        // stall logic applies.
        assert_eq!(
            inbound_decision(2000, 1500, Duration::from_secs(20), HEALTH_CHECK_INTERVAL),
            InboundDecision::Stalled
        );
    }
}
