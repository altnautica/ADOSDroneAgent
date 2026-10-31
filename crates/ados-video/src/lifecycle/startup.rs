//! Cold start: `start_stream`, the primary encoder command, discovery refresh.

use super::*;

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
    pub(crate) async fn persist_pipeline_outcome(
        &self,
        outcome: crate::camera_state::PipelineOutcome,
    ) {
        // A retry is not a fresh start. The health loop re-enters `start_stream`
        // on every retry tick, and stamping `starting` there erased the standing
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
            .with_pipeline(outcome, reason, encoder, encoder_hw)
            .with_forced_software(self.force_software);
        let path = self
            .camera_state_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(crate::camera_state::CAMERA_STATE_JSON));
        if let Err(e) = snapshot.write_to_async(path).await {
            tracing::warn!(error = %e, "camera_state_pipeline_persist_failed");
        }
    }

    /// Start the encoding + streaming pipeline. Returns `true` on success.
    ///
    /// Order: reap stale encoder →
    /// discover + persist camera-state → bail on no-primary → orphan sweeps →
    /// detect encoder → build command → optional SEI split → mediamtx
    /// config+start → spawn encoder + encoder-error stderr drain → latch Running →
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
        self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Starting)
            .await;

        // Resolve the capture source. An explicit network source
        // (`video.camera.source: rtsp://…` / `http://…`) streams from that URL
        // directly — the IP-camera mode — so no local camera probe runs; the
        // synthetic single-camera result flows through the exact same start
        // sequence (primary → encoder detect → command build) as a discovered
        // camera. Otherwise probe for a local V4L2/CSI camera as before.
        //
        // /!\ While a camera is present this re-probe is LOAD-BEARING for
        // camera format renegotiation and must never be cached across
        // restarts. `select_input_format` (`encoder.rs`) resolves MJPEG-vs-YUYV
        // from `CameraInfo::capabilities` at argv-build time, so a UVC camera
        // that renegotiates or enumerates differently after a replug would
        // otherwise be driven with the stale `-input_format` forever. Only a
        // camera-less node skips it, and only while the sysfs fingerprint is
        // unchanged, no hotplug arrived and the last probe is under a minute
        // old (see `crate::sysfs_fingerprint`).
        self.refresh_discovery(false).await;
        // Publish the fresh discovery through the one writer. The bare
        // `persist_camera_state` this replaces rebuilt the snapshot from
        // discovery alone, so it reset the outcome to `unknown` and bypassed the
        // sidecar-path override tests rely on.
        self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Starting)
            .await;

        let Some(primary) = self.last_cameras.primary_camera_info() else {
            tracing::error!("no_primary_camera");
            // A node with no camera has no encoder identity either; leaving the
            // previous cycle's kind in place publishes one in the sidecar.
            self.encoder_type = None;
            self.encoder_label = None;
            self.last_start_error = StartError::NoPrimaryCamera;
            self.state = PipelineState::Error;
            self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error)
                .await;
            return false;
        };
        let device_path = primary.device_path.clone();

        // Orphan sweeps in the exact Python order: encoder holding the camera
        // node, rpicam-vid, then the bridge publisher to /main. The stale
        // encoder was already reaped above, so this is also the one window in
        // which the RTP-sender sweep is safe: with the fan-out in force the
        // live encoder's own command line matches that pattern, so it can never
        // be swept from the tap's start/stop paths.
        //
        // The `/main` sweep is PUBLISHER-scoped: every tap reads that same URI,
        // so a plain pattern sweep would kill a cloud push or SEI tap that a
        // partial teardown legitimately left running.
        kill_orphans(&format!(
            "-i {}",
            crate::encoder::input_locator(&device_path)
        ))
        .await;
        kill_orphans("rpicam-vid").await;
        let pipe_uri = self.pipe_uri();
        kill_publisher_orphans(&pipe_uri).await;
        kill_orphans(&orphan_pattern()).await;

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
            self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error)
                .await;
            return false;
        };
        self.encoder_type = Some(kind);

        // Build the encoder command. Shared with the attention-switch
        // encoder-only respawn below, so a hero/thumbnail change produces
        // byte-identical argv to a cold start at the same settings.
        let cmd = match self.build_primary_encoder_command(&primary, &device_path, kind) {
            Ok(c) => c,
            Err(reason) => {
                // Distinct from the spawn and mediamtx failures, and the builder's
                // own distinction between "bad geometry/source" and "codec this
                // pipeline cannot frame" is carried through: without it this bail
                // points the operator at the wrong config key.
                self.encoder_label = None;
                self.last_start_error = reason;
                self.state = PipelineState::Error;
                self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error)
                    .await;
                return false;
            }
        };
        let tokens = cmd.tokens();
        self.encoder_label = Some(crate::encoder::encoder_label(kind, &tokens));
        // Read the radio fan-out back off the argv that is about to run, so the
        // wfb-tap branches below (spawn, orphan sweep, health) key off what the
        // encoder actually does rather than re-deriving the decision.
        self.encoder_emits_wfb_rtp = crate::wfb_tee::cmd_emits_wfb_rtp(&tokens);

        // Configure + start mediamtx (gates on the RTSP port internally). The
        // primary publishes into `main`; any secondary legs are added as their
        // own mediamtx paths (sourceOnDemand pulls) so mediamtx serves each at
        // `:8889/<id>/whep` independently. A single-leg node yields exactly the
        // one `main` publisher path (byte-identical to the single-stream path).
        if let Err(e) = self.mediamtx.write_config(&self.legs_to_streams()).await {
            tracing::error!(error = %e, "mediamtx_config_write_failed");
            self.last_start_error = StartError::MediamtxFailed;
            self.state = PipelineState::Error;
            self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error)
                .await;
            return false;
        }
        match self.mediamtx.start().await {
            Ok(true) => {}
            _ => {
                tracing::error!("mediamtx_start_failed; cannot stream without mediamtx");
                self.last_start_error = StartError::MediamtxFailed;
                self.state = PipelineState::Error;
                self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error)
                    .await;
                return false;
            }
        }
        // A captured camera's frames reach the encoder through its frame-bus
        // pipe; the pump has to exist before the encoder opens it.
        self.reconcile_framebus_feeds();
        let source = self.camera_cfg.source.clone();
        self.renew_framebus_for(&source);

        // Spawn the encoder. A PLAIN rate-limited stderr drain (no progress
        // tracker — the encoder is the source, not the wfb tap).
        let mut enc = match cmd.spawn() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, encoder = ?kind, "encoder_spawn_failed");
                self.last_start_error = StartError::EncoderSpawnFailed;
                self.teardown_after_partial_start().await;
                self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Error)
                    .await;
                return false;
            }
        };
        // A fresh run starts its encoder-error count from zero.
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

        // Latch Running + reset all health counters for a clean cold-start
        // window.
        self.state = PipelineState::Running;
        let now = std::time::Instant::now();
        self.started_at = now;
        self.first_packet_seen = false;
        self.inbound_bytes_value = -1;
        self.inbound_bytes_changed_at = now;
        self.video_inbound_bytes_per_s = 0.0;
        self.last_start_error = StartError::None;
        tracing::info!(encoder = ?kind, "pipeline_started");
        // The encoder is spawned, but nothing is published on `main` yet: the
        // encoder still has to open its source (a network camera's RTSP
        // handshake, a UVC format negotiation) and reach mediamtx. `streaming`
        // is stamped by the health tick that first sees the publisher (see
        // `refresh_camera_state`), so a source that never produces a frame never
        // reads as a stream.
        self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Starting)
            .await;

        // Publish the resolved leg list so the status surfaces + the GCS stream
        // switcher can advertise each `:8889/<id>/whep` leg.
        self.refresh_video_streams().await;

        // Publish the attention state the encoder actually cold-started with,
        // so the swarm beacon's hero bit and the adaptive ladder's self-heal
        // check read truth from the first frame onward. A cold start applies the
        // clamp in full (it costs no extra respawn), so nothing is deferred here.
        let (desired, settings) = self.desired_encoder_settings();
        let state = self.encoder_state(desired.profile, desired.ceiling_kbps, settings, false);
        let state = self
            .encoder_control
            .note_applied(state, desired.generation, true);
        self.publish_encoder_state(&state).await;

        // Bring up the owned encoders for any LOCAL secondary legs (each
        // publishes its camera into its own mediamtx path). Additive + isolated.
        self.start_secondary_encoders().await;

        // Radio RTP: on the ffmpeg family the encoder emits it itself (one
        // encode, two muxers) and there is nothing to spawn. On the `bash -c`
        // pipeline arms the separate tap is still the path — and it may only be
        // spawned once the encoder's RTSP publisher exists, otherwise its first
        // DESCRIBE runs against a missing path and ffmpeg exits in ~1-2 s. The
        // run-loop ladder brings it up when the path is ready.
        if self.encoder_emits_wfb_rtp {
            tracing::info!("wfb_rtp_from_encoder: separate re-read tap not spawned");
        } else if self.mediamtx.path_ready(MAIN_PATH).await {
            self.start_wfb_tee().await;
        } else {
            tracing::debug!("wfb_tee_deferred: mediamtx path not ready at stream start");
        }
        if self.sei_latency_on() {
            self.start_sei_tap().await;
        }
        // Optional additive vision frame tap: the socket is bound now and the
        // decoder starts when the vision engine connects. Like the wfb tee
        // above, only once the encoder's RTSP publisher exists; the
        // health-check ladder brings it up when the path is ready.
        if self.vision_enabled() {
            if self.mediamtx.path_ready(MAIN_PATH).await {
                self.start_vision_tap().await;
            } else {
                tracing::debug!("vision_tap_deferred: mediamtx path not ready at stream start");
            }
        }
        true
    }

    /// Build the primary leg's encoder pipeline from the CURRENT capture
    /// settings.
    ///
    /// Factored out of [`Self::start_stream`] so the attention-switch respawn
    /// ([`Self::restart_encoder_only`]) reuses the identical composition, the
    /// optional SEI split included. `Err(StartError)` on any build failure
    /// (already logged), tagged so the camera-state sidecar names the real
    /// cause rather than the generic "command could not be built".
    pub(super) fn build_primary_encoder_command(
        &self,
        primary: &CameraInfo,
        device_path: &str,
        kind: crate::encoder::EncoderKind,
    ) -> Result<PrimaryCommand, StartError> {
        let pipe_uri = self.pipe_uri();
        let mut params = EncoderParams::from_camera_config(kind, &self.camera_cfg);
        params.framebus_input = self
            .legs
            .iter()
            .find(|l| l.is_primary)
            .and_then(|l| l.raw_input());
        if self.force_software {
            // The hardware / GStreamer encoder failed to produce video; force
            // the always-runnable software path (ffmpeg libx264) so video keeps
            // flowing instead of crash-looping. Cleared on hotplug and after a
            // healthy stretch on software (see the health tick).
            params.encoder = "software".to_string();
        }
        // Ask the builder for the radio's RTP copy as a second branch of this
        // encoder's own output, which is what retires the separate re-read tap
        // (`wfb_tee`) on the ffmpeg family. The builder ignores the request on
        // the `bash -c` pipeline arms (rpicam / GStreamer), and the caller
        // decides whether to spawn the tap by reading the built argv back
        // through `cmd_emits_wfb_rtp`, so there is no second source of truth.
        params.rtp_fanout = true;
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
                return Err(StartError::EncoderCommandFailed);
            }
            Err(e @ crate::encoder::EncoderError::UnsupportedCodec(_)) => {
                tracing::error!(error = %e, "encoder_command_build_failed");
                return Err(StartError::UnsupportedCodec);
            }
            Err(e) => {
                tracing::error!(error = %e, "encoder_command_build_failed");
                return Err(StartError::EncoderCommandFailed);
            }
        };

        // Optional SEI split upstream of mediamtx so every consumer sees the
        // same wall-clock marker on the same frame.
        if self.sei_latency_on() {
            if let Some(split) = wrap_with_sei_inject(&cmd, &pipe_uri) {
                tracing::info!(encoder = ?kind, "sei_inject_upstream_of_mediamtx");
                return Ok(PrimaryCommand::Sei(split));
            }
        }
        Ok(PrimaryCommand::Single(cmd))
    }

    /// Resolve the capture source into `last_cameras`.
    ///
    /// An explicit network source yields the synthetic single-camera result
    /// with no probe. A local probe runs per
    /// [`crate::sysfs_fingerprint::should_rediscover`]; `force` (and a pending
    /// hotplug) always runs it.
    pub(crate) async fn refresh_discovery(&mut self, force: bool) {
        if let Some(url) = self.camera_cfg.network_source().map(str::to_string) {
            tracing::info!(source = %url, "video_streaming_from_network_source");
            self.last_cameras = discover::DiscoveryResult::for_network_source(&url);
            return;
        }
        if let Some(id) = self.camera_cfg.framebus_camera().map(str::to_string) {
            // The vision engine owns this camera; there is nothing to probe.
            tracing::info!(camera = %id, "video_streaming_from_framebus");
            self.last_cameras = discover::DiscoveryResult::for_framebus_source(&id);
            return;
        }
        let fingerprint = crate::sysfs_fingerprint::fingerprint();
        let hotplug = std::mem::take(&mut self.rediscover_requested);
        let run = force
            || crate::sysfs_fingerprint::should_rediscover(
                self.last_cameras.primary_camera_info().is_some(),
                self.last_fingerprint.as_deref(),
                &fingerprint,
                hotplug,
                self.last_discovery_at.map(|at| at.elapsed()),
            );
        if !run {
            tracing::debug!("camera_discovery_skipped: no video device change");
            return;
        }
        self.last_fingerprint = Some(fingerprint);
        self.last_discovery_at = Some(Instant::now());
        let mut found =
            discover::discover(&self.python_executable, discover::DISCOVERY_TIMEOUT).await;
        // The probe knows nothing of the roster: honour the primary leg's own
        // device and never pick one a captured camera's vision engine owns.
        let owned: Vec<&str> = self
            .config
            .cameras
            .iter()
            .filter(|c| c.capture && c.enabled)
            .map(|c| c.source.trim())
            .collect();
        found.assign_primary(&self.camera_cfg.source, &owned);
        self.last_cameras = found;
    }
}
