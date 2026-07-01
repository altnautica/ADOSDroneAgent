//! Subprocess start/stop lifecycle for the video pipeline.
//!
//! This module owns the cold-start sequence (`start_stream`) and every leaf
//! spawn/teardown the orchestrator drives — the wfb radio tap, the decoupled
//! vision tap, the headless SEI tap, the cloud-relay push, and the reaping of
//! the deferred GStreamer air-pipeline slot. Each spawn carries the
//! setsid/killpg process-group ownership (via [`crate::process::ManagedProcess`])
//! so a dropped future can never orphan a child onto a mediamtx publisher slot.
//!
//! The supervision policy (the health ticks, the restart ladders, the run
//! loop) lives in [`crate::orchestrator`]; this module is the mechanism those
//! decisions actuate.

use std::time::Duration;

use crate::discover;
use crate::encoder::{
    augment_encoder_with_raw_tap, binary_present, build_encoder_command, detect_encoder_for_camera,
    wrap_with_sei_inject, EncoderParams,
};
use crate::health::{PipelineState, StartError};
use crate::mediamtx::MAIN_PATH;
use crate::orchestrator::VideoOrchestrator;
use crate::process::{kill_orphans, ManagedProcess};
use crate::tap::{self, spawn_vision_tap};
use crate::wfb_tee::{drain_wfb_tee_stderr, orphan_pattern, spawn_wfb_tee, ProgressTracker};

impl VideoOrchestrator {
    /// Start the encoding + streaming pipeline. Returns `true` on success.
    ///
    /// Exact order mirrors `pipeline.py::start_stream`: reap stale encoder →
    /// discover + persist camera-state → bail on no-primary → orphan sweeps →
    /// detect encoder → build command → optional SEI wrap → mediamtx
    /// config+start → spawn encoder + plain stderr drain → latch Running →
    /// best-effort wfb-tee → optional SEI tap. cloud_push is NOT started here.
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

        // Resolve the capture source. An explicit network source
        // (`video.camera.source: rtsp://…` / `http://…`) streams from that URL
        // directly — the IP-camera mode — so no local camera probe runs; the
        // synthetic single-camera result flows through the exact same start
        // sequence (primary → encoder detect → command build) as a discovered
        // camera. Otherwise probe for a local V4L2/CSI camera as before.
        let net_source = self.camera_cfg.network_source().map(str::to_string);
        let discovery = match net_source {
            Some(url) => {
                tracing::info!(source = %url, "video_streaming_from_network_source");
                discover::DiscoveryResult::for_network_source(&url)
            }
            None => discover::discover(&self.python_executable, discover::DISCOVERY_TIMEOUT).await,
        };
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
        let now = std::time::Instant::now();
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

        // Best-effort radio fan-out + optional SEI tap. Only spawn the tee once
        // the encoder's RTSP publisher exists; otherwise the first DESCRIBE runs
        // against a missing path and ffmpeg exits in ~1-2 s. The run-loop ladder
        // brings the tee up once the path is ready.
        if self.mediamtx.path_ready(MAIN_PATH).await {
            self.start_wfb_tee().await;
        } else {
            tracing::debug!("wfb_tee_deferred: mediamtx path not ready at stream start");
        }
        if self.sei_latency_on() {
            self.start_sei_tap().await;
        }
        // Optional additive vision frame tap. When raw_tap is on the frames are
        // already produced by the spliced encoder output, so no separate
        // process is spawned; otherwise spawn the decoupled third ffmpeg — but,
        // exactly like the wfb tee above, only once the encoder's RTSP publisher
        // exists. Starting it against a missing `/main` makes ffmpeg's first
        // DESCRIBE fail and exit in ~1-2 s, and the tap then death-loops; the
        // health-check ladder brings it up when the path is ready.
        if self.vision_enabled() && !self.config.vision.raw_tap {
            if self.mediamtx.path_ready(MAIN_PATH).await {
                self.start_vision_tap().await;
            } else {
                tracing::debug!("vision_tap_deferred: mediamtx path not ready at stream start");
            }
        }
        true
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
        // Abort a stale reframer from a prior (exited) tap before respawn.
        if let Some(h) = self.vision_tap_reframer.take() {
            h.abort();
        }
        let v = &self.config.vision;
        let mut t = match spawn_vision_tap(
            self.mediamtx.rtsp_port(),
            v.fps,
            v.width,
            v.height,
            v.pixel_format(),
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
        // Bind the serving socket and start the reframer: read ffmpeg's raw
        // frames off stdout, ADVT-header them (Contract F), and serve the
        // connecting vision engine. A bind/stdout failure leaves the tap process
        // up but with no consumer — surfaced loudly, never silent (Rule 44).
        match (t.take_stdout(), tap::bind_vision_tap(&v.sink)) {
            (Some(stdout), Ok(listener)) => {
                let format = tap::frame_format_from_str(v.pixel_format());
                let (w, h) = (v.width, v.height);
                self.vision_tap_reframer = Some(tokio::spawn(tap::run_vision_tap_server(
                    listener, stdout, format, w, h,
                )));
            }
            (None, _) => {
                tracing::error!("vision_tap_no_stdout; reframer not started");
            }
            (_, Err(e)) => {
                tracing::error!(
                    error = %e,
                    sink = %v.sink,
                    "vision_tap_bind_failed; reframer not started"
                );
            }
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
        if let Some(h) = self.vision_tap_reframer.take() {
            h.abort();
        }
        if let Some(mut p) = self.vision_tap.take() {
            p.terminate(Duration::from_secs(5)).await;
        }
        // Remove the served socket so the next bind starts clean.
        let _ = std::fs::remove_file(&self.config.vision.sink);
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
    pub(crate) async fn stop_gst_air(&mut self) {
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
}
