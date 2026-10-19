//! Leaf spawns and teardowns: encoder respawn, wfb tee, vision/SEI taps, cloud push.

use super::*;

impl VideoOrchestrator {
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
        let cmd = match self.build_primary_encoder_command(&primary, &device_path, kind) {
            Ok(c) => c,
            Err(reason) => {
                // Stamp the cause: without it the sidecar carries whatever the
                // last cold start left behind while the respawn silently failed.
                self.last_start_error = reason;
                return false;
            }
        };
        // An attention switch rebuilds the argv, so re-resolve the identity the
        // sidecar publishes from the command actually about to run, and with it
        // whether that command carries the radio fan-out.
        let tokens = cmd.tokens();
        self.encoder_label = Some(crate::encoder::encoder_label(kind, &tokens));
        self.encoder_emits_wfb_rtp = crate::wfb_tee::cmd_emits_wfb_rtp(&tokens);

        if let Some(mut enc) = self.encoder.take() {
            if enc.is_running() {
                enc.terminate(Duration::from_secs(5)).await;
            }
        }
        // The outgoing encoder held the mediamtx publisher slot; sweep any
        // straggler so the incoming one is not refused the path.
        //
        // PUBLISHER-scoped, and that is the whole point of this call: the radio
        // tap, the cloud push, the vision tap and the SEI tap all read this
        // exact URI, so the raw-pattern sweep this replaces SIGKILLed all four
        // on every attention switch and every adaptive-bitrate step — dropping
        // the operator's radio picture, the cloud feed and the detection stream
        // to change one encoder parameter, against the contract stated above.
        kill_publisher_orphans(&self.pipe_uri()).await;
        // With the fan-out in force the encoder IS the RTP sender, so this is
        // the one safe window to sweep a stale sender (ours is down, the fresh
        // one is not spawned yet). Sweeping at any other time would SIGKILL the
        // live encoder, whose command line matches the same pattern.
        if self.encoder_emits_wfb_rtp {
            kill_orphans(&orphan_pattern()).await;
        }

        let source = self.camera_cfg.source.clone();
        self.renew_framebus_for(&source);
        let mut enc = match cmd.spawn() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, encoder = ?kind, "encoder_respawn_failed");
                self.last_start_error = StartError::EncoderSpawnFailed;
                return false;
            }
        };
        self.encoder_errors
            .store(0, std::sync::atomic::Ordering::Release);
        if let Some(stderr) = enc.take_stderr() {
            tokio::spawn(crate::stderr_drain::drain_encoder(
                stderr,
                "encoder",
                Arc::clone(&self.encoder_errors),
            ));
        }
        self.encoder = Some(enc);
        // Count it: this is the event the adaptive ladder can storm on, and the
        // counter is what makes a storm visible on the sidecar rather than
        // inferred from a blinking picture.
        self.encoder_respawns = self.encoder_respawns.saturating_add(1);

        // Re-arm the startup-grace window: the fresh encoder has not published
        // yet, and without this the inbound-flow watchdog reads the respawn gap
        // as a stall and cold-starts the whole pipeline underneath us.
        let now = Instant::now();
        self.started_at = now;
        self.first_packet_seen = false;
        self.inbound_bytes_value = -1;
        self.inbound_bytes_changed_at = now;
        self.video_inbound_bytes_per_s = 0.0;
        tracing::info!(
            encoder = ?kind,
            encoder_respawns = self.encoder_respawns,
            "encoder_respawned_for_attention_change"
        );
        true
    }

    /// Spawn the wfb radio tap (idempotent). Best-effort: a failure leaves the rest of
    /// the pipeline up.
    ///
    /// A no-op when the encoder emits the radio's RTP itself: there is nothing
    /// to spawn, and the orphan sweep below would match the live encoder's own
    /// command line and kill it.
    pub async fn start_wfb_tee(&mut self) {
        if self.encoder_emits_wfb_rtp {
            tracing::debug!("wfb_tee_not_needed: encoder emits the radio RTP directly");
            return;
        }
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
    ///
    /// The belt-and-suspenders orphan sweep is skipped when the encoder emits
    /// the radio RTP itself — the pattern matches the encoder's own command
    /// line, and teardown order is tap-then-encoder, so sweeping here would
    /// kill the encoder out from under the graceful stop.
    pub async fn stop_wfb_tee(&mut self) {
        if let Some(mut p) = self.wfb_tee.take() {
            p.terminate(Duration::from_secs(5)).await;
        }
        if !self.encoder_emits_wfb_rtp {
            kill_orphans(&orphan_pattern()).await;
        }
    }

    /// Bring up the vision frame tap leg (idempotent). Best-effort and strictly
    /// additive: a failure leaves the encode + radio path fully up.
    ///
    /// The serving socket is bound BEFORE anything else: a bind failure logs
    /// and leaves the leg down with nothing spawned, and the ladder retries the
    /// bind on its next tick. The decoder itself is spawned by the leg only
    /// when a vision engine connects (see [`crate::tap_lazy`]).
    pub async fn start_vision_tap(&mut self) {
        if !self.vision_enabled() {
            return;
        }
        if self.state != PipelineState::Running {
            tracing::warn!("vision_tap_skipped: pipeline not running");
            return;
        }
        if self.vision_tap.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let v = &self.config.vision;
        let listener = match tap::bind_vision_tap(&v.sink) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    sink = %v.sink,
                    "vision_tap_bind_failed; no decoder spawned"
                );
                self.vision_tap = None;
                return;
            }
        };
        let geometry = TapGeometry {
            format: tap::frame_format_from_str(v.pixel_format()),
            width: v.width,
            height: v.height,
        };
        let (port, fps, width, height) = (self.mediamtx.rtsp_port(), v.fps, v.width, v.height);
        let pixel_format = v.pixel_format().to_string();
        let spawn = move || spawn_vision_tap(port, fps, width, height, &pixel_format);
        let state = Arc::new(TapLegState::default());
        self.vision_tap = Some(tokio::spawn(run_lazy_vision_tap(
            listener,
            geometry,
            ENGINE_IDLE_STOP,
            spawn,
            state,
            Arc::clone(&self.encoder_control),
        )));
        tracing::info!(
            sink = %v.sink,
            fps = v.fps,
            width = v.width,
            height = v.height,
            format = %v.pixel_format(),
            "vision_tap_listening"
        );
    }

    /// Stop the vision frame tap leg (and its decoder, if one is running).
    /// Parallels [`stop_wfb_tee`](Self::stop_wfb_tee).
    pub async fn stop_vision_tap(&mut self) {
        if let Some(h) = self.vision_tap.take() {
            // Aborting drops the leg's decoder handle, whose `Drop` kills the
            // decoder's process group.
            h.abort();
            let _ = h.await;
        }
        // Remove the served socket so the next bind starts clean.
        let _ = std::fs::remove_file(&self.config.vision.sink);
    }

    /// Spawn the headless SEI latency tap as a one-shot Python subprocess,
    /// gated on the mediamtx path being ready. Mirrors the SEI-tap spawn but
    /// runs `--once` so the Rust orchestrator owns the restart cadence: one
    /// session per retry tick at most, and none while `main` has no publisher
    /// (a self-looping Python tap respawned against a dead source).
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
        let args = sei_tap_args(&self.pipe_uri());
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
        // Stamped so the supervisor can bound ONE session's wall time: a
        // one-shot that never returns stays `is_running()` forever and the
        // exit-triggered respawn can never fire.
        self.sei_tap_started_at = Some(Instant::now());
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
        let push_url = format!("{cloud_url}/main");
        let args = cloud_push_args(&self.pipe_uri(), &push_url);
        let mut push = match ManagedProcess::spawn("cloud_push", "ffmpeg", &args) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "cloud_push_ffmpeg_spawn_failed");
                return false;
            }
        };
        // A fresh tracker per spawn, so the new push gets the full window
        // before the output-stall check can trip and `total_size` restarting
        // from zero does not read as flat.
        self.cloud_push_progress = ProgressTracker::new();
        if let Some(stderr) = push.take_stderr() {
            tokio::spawn(crate::wfb_tee::drain_wfb_tee_stderr(
                stderr,
                self.cloud_push_progress.clone(),
            ));
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
    pub(super) async fn teardown_after_partial_start(&mut self) {
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
