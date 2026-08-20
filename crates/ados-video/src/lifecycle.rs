//! Subprocess start/stop lifecycle for the video pipeline.
//!
//! This module owns the cold-start sequence (`start_stream`) and every leaf
//! spawn/teardown the orchestrator drives — the wfb radio tap, the decoupled
//! vision tap, the headless SEI tap, and the cloud-relay push. Each spawn
//! carries the
//! setsid/killpg process-group ownership (via [`crate::process::ManagedProcess`])
//! so a dropped future can never orphan a child onto a mediamtx publisher slot.
//!
//! The supervision policy (the health ticks, the restart ladders, the run
//! loop) lives in [`crate::orchestrator`]; this module is the mechanism those
//! decisions actuate.

use std::time::{Duration, Instant};

use crate::discover;
use crate::encoder::{
    augment_encoder_with_raw_tap, binary_present, build_encoder_command, detect_encoder_for_camera,
    wrap_with_sei_inject, CameraInfo, CameraType, EncoderParams,
};
use crate::health::{PipelineState, StartError};
use crate::mediamtx::MAIN_PATH;
use crate::orchestrator::VideoOrchestrator;
use crate::process::{kill_orphans, ManagedProcess};
use crate::tap::{self, spawn_vision_tap};

/// Give up respawning a local secondary encoder after this many attempts, so a
/// permanently-broken camera does not respawn forever every tick.
const MAX_SECONDARY_RESPAWNS: u32 = 5;
use crate::wfb_tee::{drain_wfb_tee_stderr, orphan_pattern, spawn_wfb_tee, ProgressTracker};

impl VideoOrchestrator {
    /// Re-persist `camera-state.json` with the pipeline's outcome stamped on it.
    ///
    /// Discovery and streaming are sequential but independent steps of one
    /// `start_stream`, and `persist_camera_state` runs BEFORE every `StartError`
    /// bail. Without this the sidecar keeps saying `state: "ready"` with a live
    /// model name for a pipeline sitting in `Error`, which is exactly how a node
    /// ends up showing a confident camera card and no video. This is the ONLY
    /// writer of the file after the initial discovery persist — every exit of
    /// `start_stream`, `stop_stream`, and every health tick through
    /// [`VideoOrchestrator::refresh_camera_state`] — so the two facts can never
    /// disagree and no caller can silently reset the outcome to `unknown`.
    ///
    /// Honors [`VideoOrchestrator::camera_state_path`] (the canonical contract
    /// path when `None`), so a test can observe the stamp without writing to the
    /// developer's real `/run/ados/camera-state.json`.
    pub(crate) fn persist_pipeline_outcome(&self, outcome: crate::camera_state::PipelineOutcome) {
        // A retry is not a fresh start. The health loop re-enters `start_stream`
        // on its backoff, and stamping `starting` there erased the standing
        // failure: on a node with no usable encoder the surface flapped between
        // `error` with its reason and `connecting` with none, every few seconds,
        // for a pipeline that had not recovered at all. The last failure stands
        // until this attempt resolves it — success stamps `streaming`, another
        // failure stamps its own reason.
        let outcome = match outcome {
            crate::camera_state::PipelineOutcome::Starting
                if self.last_start_error != StartError::None =>
            {
                crate::camera_state::PipelineOutcome::Error
            }
            other => other,
        };
        // A reason belongs to a failure. Carrying the previous cycle's
        // StartError onto a `starting` or `stopped` stamp points the operator at
        // a subsystem that is not the one holding this pipeline back.
        let reason = match outcome {
            crate::camera_state::PipelineOutcome::Error => {
                self.last_start_error.reason().map(str::to_string)
            }
            _ => None,
        };
        let encoder = self.encoder_label.clone();
        let encoder_hw = encoder.as_deref().map(crate::encoder::encoder_is_hardware);
        let snapshot = self
            .last_cameras
            .camera_state_snapshot()
            .with_pipeline(outcome, reason, encoder, encoder_hw);
        let path = self
            .camera_state_path
            .as_deref()
            .unwrap_or(std::path::Path::new(crate::camera_state::CAMERA_STATE_JSON));
        if let Err(e) = snapshot.write_to(path) {
            tracing::warn!(error = %e, "camera_state_pipeline_persist_failed");
        }
    }

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

        // Cold-start from the CURRENTLY DESIRED attention state, not from
        // whatever the last run left in `camera_cfg`. A hero promotion that
        // arrived while the pipeline was down (or during startup, before the run
        // loop began watching) is honoured by the very first encoder command
        // rather than silently deferred to the next switch.
        let (_, desired_settings) = self.desired_encoder_settings();
        crate::orchestrator::apply_settings_to(&mut self.camera_cfg, desired_settings);

        // Reap any stale encoder from a prior cycle by process group.
        if let Some(mut enc) = self.encoder.take() {
            if enc.is_running() {
                tracing::info!(pid = enc.pid(), "killing_stale_encoder");
                enc.terminate(Duration::from_secs(5)).await;
            }
        }

        self.state = PipelineState::Starting;
        // The previous cycle's encoder was reaped above, so the identity it
        // published is no longer true. Stamp the transition: a cold start can
        // take tens of seconds (discovery, orphan sweeps, the mediamtx gate),
        // and for that whole window the sidecar otherwise still reads
        // `streaming` for a pipeline that has no encoder at all.
        self.encoder_label = None;
        self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Starting);

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
        self.last_cameras = discovery;
        // Publish the fresh discovery through the one writer. The bare
        // `persist_camera_state` this replaces rebuilt the snapshot from
        // discovery alone, so it reset the outcome to `unknown` and bypassed the
        // sidecar-path override tests rely on.
        self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Starting);

        let Some(primary) = self.last_cameras.primary_camera_info() else {
            tracing::error!("no_primary_camera");
            // A node with no camera has no encoder identity either; leaving the
            // previous cycle's kind in place publishes one in the sidecar.
            self.encoder_type = None;
            self.encoder_label = None;
            self.last_start_error = StartError::NoPrimaryCamera;
            self.state = PipelineState::Error;
            self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error);
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
            self.encoder_label = None;
            self.last_start_error = StartError::NoEncoder;
            self.state = PipelineState::Error;
            self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error);
            return false;
        };
        self.encoder_type = Some(kind);

        // Build the encoder command. Shared with the attention-switch
        // encoder-only respawn below, so a hero/thumbnail change produces
        // byte-identical argv to a cold start at the same settings.
        let Some(cmd) = self.build_primary_encoder_command(&primary, &device_path, kind) else {
            // Distinct from the spawn and mediamtx failures: without its own
            // variant this bail publishes whichever reason the previous cycle
            // left behind, sending the operator after the wrong subsystem.
            self.encoder_label = None;
            self.last_start_error = StartError::EncoderCommandFailed;
            self.state = PipelineState::Error;
            self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error);
            return false;
        };
        self.encoder_label = Some(crate::encoder::encoder_label(kind, &cmd));

        // Configure + start mediamtx (gates on the RTSP port internally). The
        // primary publishes into `main`; any secondary legs are added as their
        // own mediamtx paths (sourceOnDemand pulls) so mediamtx serves each at
        // `:8889/<id>/whep` independently. A single-leg node yields exactly the
        // one `main` publisher path (byte-identical to the single-stream path).
        if let Err(e) = self.mediamtx.write_config(&self.legs_to_streams()) {
            tracing::error!(error = %e, "mediamtx_config_write_failed");
            self.last_start_error = StartError::MediamtxFailed;
            self.state = PipelineState::Error;
            self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error);
            return false;
        }
        match self.mediamtx.start().await {
            Ok(true) => {}
            _ => {
                tracing::error!("mediamtx_start_failed; cannot stream without mediamtx");
                self.last_start_error = StartError::MediamtxFailed;
                self.state = PipelineState::Error;
                self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error);
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
                self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error);
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
        self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Streaming);

        // Publish the resolved leg list so the status surfaces + the GCS stream
        // switcher can advertise each `:8889/<id>/whep` leg.
        self.refresh_video_streams();

        // Publish the attention state the encoder actually cold-started with,
        // so the swarm beacon's hero bit and the adaptive ladder's self-heal
        // check read truth from the first frame onward.
        let (desired, settings) = self.desired_encoder_settings();
        self.encoder_control.note_applied(
            crate::profile::EncoderState::new(desired.profile, desired.ceiling_kbps, settings),
            desired.generation,
            true,
        );
        self.publish_encoder_state(&crate::profile::EncoderState::new(
            desired.profile,
            desired.ceiling_kbps,
            settings,
        ));

        // Bring up the owned encoders for any LOCAL secondary legs (each
        // publishes its camera into its own mediamtx path). Additive + isolated.
        self.start_secondary_encoders().await;

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

    /// Build the primary leg's encoder argv from the CURRENT capture settings.
    ///
    /// Factored out of [`Self::start_stream`] so the attention-switch respawn
    /// ([`Self::restart_encoder_only`]) reuses the identical composition — the
    /// optional SEI wrap and the opt-in pre-encode vision-tap splice included.
    /// Duplicating it would let a hero switch silently drop the vision tap.
    /// `None` on any build failure (already logged).
    fn build_primary_encoder_command(
        &self,
        primary: &CameraInfo,
        device_path: &str,
        kind: crate::encoder::EncoderKind,
    ) -> Option<Vec<String>> {
        let pipe_uri = self.pipe_uri();
        let mut params = EncoderParams::from_camera_config(kind, &self.camera_cfg);
        if self.force_software {
            // The hardware / GStreamer encoder was abandoned this session (it
            // never produced a packet); force the always-runnable software path
            // (ffmpeg libx264) so video keeps flowing instead of crash-looping.
            params.encoder = "software".to_string();
        }
        let cmd = match build_encoder_command(
            &params,
            device_path,
            &pipe_uri,
            Some(primary),
            &self.env,
        ) {
            Ok(c) if !c.is_empty() => c,
            Ok(_) => {
                tracing::error!("encoder_command_empty");
                return None;
            }
            Err(e) => {
                tracing::error!(error = %e, "encoder_command_build_failed");
                return None;
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
        // commands fall back to the decoupled third-ffmpeg tap, which never
        // touches the encoder. Off by default.
        if self.vision_enabled() && self.config.vision.raw_tap {
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
                tracing::info!(sink = %v.sink, "vision_raw_tap_spliced_into_encoder");
            } else {
                tracing::info!(
                    "vision_raw_tap_requested_but_command_not_eligible; using decoupled tap"
                );
            }
            Some(augmented)
        } else {
            Some(cmd)
        }
    }

    /// Respawn ONLY the encoder child against the current capture settings.
    ///
    /// This is what an attention switch (hero ⇄ thumbnail) and an adaptive
    /// bitrate clamp actuate. mediamtx, the wfb tap, the cloud push, the SEI tap
    /// and the vision tap all keep running: the encoder reconnects to the same
    /// mediamtx `main` publisher slot within its startup grace, so the cost is a
    /// sub-second gap rather than a full pipeline cold start.
    ///
    /// Returns `true` when a new encoder was spawned. On failure the pipeline is
    /// left encoder-less and the run loop's existing health ladder cold-starts
    /// it — the same recovery path an encoder crash takes.
    pub async fn restart_encoder_only(&mut self) -> bool {
        // The same lock the cold start and the health-check restart take, so an
        // attention switch can never interleave with a pipeline restart.
        let lock = std::sync::Arc::clone(&self.restart_lock);
        let _guard = lock.lock().await;

        let Some(primary) = self.last_cameras.primary_camera_info() else {
            tracing::warn!("encoder_restart_skipped: no primary camera in the last discovery");
            return false;
        };
        let Some(kind) = self.encoder_type else {
            tracing::warn!("encoder_restart_skipped: no encoder backend detected yet");
            return false;
        };
        let device_path = primary.device_path.clone();
        let Some(cmd) = self.build_primary_encoder_command(&primary, &device_path, kind) else {
            return false;
        };
        // An attention switch rebuilds the argv, so re-resolve the identity the
        // sidecar publishes from the command actually about to run.
        self.encoder_label = Some(crate::encoder::encoder_label(kind, &cmd));

        if let Some(mut enc) = self.encoder.take() {
            if enc.is_running() {
                enc.terminate(Duration::from_secs(5)).await;
            }
        }
        // The outgoing encoder held the mediamtx publisher slot; sweep any
        // straggler so the incoming one is not refused the path.
        kill_orphans(&self.pipe_uri()).await;

        let program = cmd[0].clone();
        let args: Vec<String> = cmd[1..].to_vec();
        let mut enc = match ManagedProcess::spawn("encoder", &program, &args) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, encoder = ?kind, "encoder_respawn_failed");
                self.last_start_error = StartError::EncoderSpawnFailed;
                return false;
            }
        };
        if let Some(stderr) = enc.take_stderr() {
            tokio::spawn(crate::stderr_drain::drain_plain(stderr, "encoder"));
        }
        self.encoder = Some(enc);

        // Re-arm the startup-grace window: the fresh encoder has not published
        // yet, and without this the inbound-flow watchdog reads the respawn gap
        // as a stall and cold-starts the whole pipeline underneath us.
        let now = Instant::now();
        self.started_at = now;
        self.first_packet_seen = false;
        self.inbound_bytes_value = -1;
        self.inbound_bytes_changed_at = now;
        self.video_inbound_bytes_per_s = 0.0;
        tracing::info!(encoder = ?kind, "encoder_respawned_for_attention_change");
        true
    }

    /// Spawn the wfb radio tap (idempotent). Best-effort: a failure leaves the rest of
    /// the pipeline up.
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

    /// Stop the wfb radio tap.
    pub async fn stop_wfb_tee(&mut self) {
        if let Some(mut p) = self.wfb_tee.take() {
            p.terminate(Duration::from_secs(5)).await;
        }
        // Belt-and-suspenders orphan sweep.
        kill_orphans(&orphan_pattern()).await;
    }

    /// Spawn the decoupled vision frame tap (idempotent). Best-effort and strictly
    /// additive: a failure leaves the encode + radio path fully up.
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

    /// Classify a local secondary leg's bus from its source hint, for encoder
    /// selection. A `/dev/videoN` path or a `"usb"`/`"ip"` hint → USB; a `"csi"`
    /// hint → CSI. Network legs never reach here (they are mediamtx pulls).
    fn secondary_camera_type(source: &str) -> CameraType {
        let s = source.trim().to_ascii_lowercase();
        if s == "csi" {
            CameraType::Csi
        } else {
            CameraType::Usb
        }
    }

    /// Build + spawn the owned encoders for the LOCAL secondary legs (each
    /// publishes its camera into `rtsp://localhost:<port>/<id>`). Best-effort +
    /// strictly additive: a failure on one leg leaves the primary + the other
    /// legs fully up (separate processes). Idempotent — a leg already running is
    /// left alone. Skips the primary (the inline pipeline owns it) and network
    /// pulls (mediamtx owns them).
    pub async fn start_secondary_encoders(&mut self) {
        if self.state != PipelineState::Running {
            return;
        }
        let port = self.mediamtx.rtsp_port();
        for leg in self.legs.clone() {
            if leg.is_primary || leg.is_network_pull {
                continue;
            }
            if self
                .secondary_encoders
                .iter_mut()
                .any(|(id, p)| id == &leg.id && p.is_running())
            {
                continue;
            }
            // Skip a leg that has exhausted its respawn budget (given up on).
            if self
                .secondary_respawn_attempts
                .get(&leg.id)
                .copied()
                .unwrap_or(0)
                > MAX_SECONDARY_RESPAWNS
            {
                continue;
            }
            let camera_type = Self::secondary_camera_type(&leg.source);
            let Some(kind) = detect_encoder_for_camera(
                camera_type,
                binary_present("rpicam-vid"),
                binary_present("ffmpeg"),
                binary_present("gst-launch-1.0"),
            ) else {
                tracing::warn!(id = %leg.id, "secondary_encoder_skipped: no encoder available");
                continue;
            };
            let camera = CameraInfo {
                camera_type,
                device_path: leg.source.clone(),
                capabilities: Vec::new(),
            };
            let cam_cfg = leg.to_camera_config();
            let params = EncoderParams::from_camera_config(kind, &cam_cfg);
            let output = format!("rtsp://localhost:{port}/{}", leg.id);
            let cmd = match build_encoder_command(
                &params,
                &leg.source,
                &output,
                Some(&camera),
                &self.env,
            ) {
                Ok(c) if !c.is_empty() => c,
                _ => {
                    tracing::warn!(id = %leg.id, "secondary_encoder_command_build_failed");
                    continue;
                }
            };
            match ManagedProcess::spawn(&format!("encoder-{}", leg.id), &cmd[0], &cmd[1..]) {
                Ok(mut proc) => {
                    if let Some(stderr) = proc.take_stderr() {
                        tokio::spawn(crate::stderr_drain::drain_plain(
                            stderr,
                            "secondary_encoder",
                        ));
                    }
                    // Replace any dead slot for this leg, else push.
                    self.secondary_encoders.retain(|(id, _)| id != &leg.id);
                    self.secondary_encoders.push((leg.id.clone(), proc));
                    // Stamp the (re)start so a leg that survives a healthy window
                    // clears its respawn count (consecutive-failure semantics).
                    self.secondary_started_at
                        .insert(leg.id.clone(), Instant::now());
                    tracing::info!(id = %leg.id, encoder = ?kind, "secondary_encoder_started");
                }
                Err(e) => {
                    tracing::warn!(id = %leg.id, error = %e, "secondary_encoder_spawn_failed");
                }
            }
        }
    }

    /// Terminate every owned secondary-leg encoder.
    pub async fn stop_secondary_encoders(&mut self) {
        for (id, mut proc) in std::mem::take(&mut self.secondary_encoders) {
            let _ = id;
            proc.terminate(Duration::from_secs(5)).await;
        }
        // Clear the per-leg circuit-breaker state so a reconfigure / pipeline
        // restart starts each leg fresh rather than inheriting a stale count.
        self.secondary_respawn_attempts.clear();
        self.secondary_started_at.clear();
    }

    /// Best-effort respawn of any DEAD secondary-leg encoder. Isolated from the
    /// primary restart ladder — a secondary is a LAN-WHEP-only extra, so a plain
    /// liveness respawn (no backoff ladder / circuit breaker) is sufficient.
    /// Called at the end of the running tick.
    pub async fn supervise_secondary_encoders(&mut self) {
        if self.state != PipelineState::Running {
            return;
        }
        // One pass splits the encoders into still-running and dead (is_running
        // polls the child, so it needs &mut and cannot run inside a closure that
        // also borrows the counter maps).
        let now = Instant::now();
        let mut dead_ids: Vec<String> = Vec::new();
        let mut running_ids: Vec<String> = Vec::new();
        for (id, p) in self.secondary_encoders.iter_mut() {
            if p.is_running() {
                running_ids.push(id.clone());
            } else {
                dead_ids.push(id.clone());
            }
        }
        // Clear the respawn count for any leg that has run healthy (process alive)
        // for a full window since its last (re)start — the secondary analog of the
        // primary's `note_healthy_tick`, so the budget counts CONSECUTIVE failures
        // and a flaky-but-recoverable camera is never permanently abandoned.
        for id in &running_ids {
            let over_budget = self
                .secondary_respawn_attempts
                .get(id)
                .copied()
                .unwrap_or(0)
                > 0;
            let window_elapsed = self
                .secondary_started_at
                .get(id)
                .is_some_and(|since| crate::health::healthy_window_elapsed(*since, now));
            if over_budget && window_elapsed {
                tracing::info!(
                    leg = %id, window_s = crate::health::HEALTHY_RESET_WINDOW.as_secs(),
                    "secondary_encoder_respawn_counter_reset: healthy window reached"
                );
                self.secondary_respawn_attempts.remove(id);
            }
        }
        if !dead_ids.is_empty() {
            self.secondary_encoders
                .retain(|(id, _)| !dead_ids.contains(id));
            // Count a respawn per dead leg, so a permanently-broken local camera
            // is given up on (a bounded circuit breaker) rather than respawned
            // forever every tick.
            for id in &dead_ids {
                let n = self
                    .secondary_respawn_attempts
                    .entry(id.clone())
                    .or_insert(0);
                *n += 1;
                if *n > MAX_SECONDARY_RESPAWNS {
                    tracing::warn!(
                        leg = %id, attempts = *n,
                        "secondary_encoder_giving_up: too many respawns"
                    );
                }
            }
            // Re-run the spawn pass; it only starts legs not already running and
            // skips legs past the respawn cap.
            self.start_secondary_encoders().await;
        }
    }

    /// Stop the decoupled vision frame tap. Parallels
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

    /// Start the cloud-relay push (an ffmpeg that copies local RTSP to the cloud
    /// relay). Returns `true` on spawn.
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

    /// Stop the cloud push.
    pub async fn stop_cloud_push(&mut self) {
        if let Some(mut p) = self.cloud_push.take() {
            p.terminate(Duration::from_secs(5)).await;
            tracing::info!("cloud_push_stopped");
        }
    }

    /// Roll back a partial start: tear down anything spawned after mediamtx.start(),
    /// then mark Error.
    async fn teardown_after_partial_start(&mut self) {
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
