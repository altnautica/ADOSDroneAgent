//! Secondary-camera encoders: spawn, supervise, stop.

use super::*;

impl VideoOrchestrator {
    /// Classify a local secondary leg's bus from its source hint, for encoder
    /// selection. A `/dev/videoN` path or a `"usb"`/`"ip"` hint → USB; a `"csi"`
    /// hint → CSI; a `framebus://` source → the frame bus. Network legs never
    /// reach here (they are mediamtx pulls).
    fn secondary_camera_type(source: &str) -> CameraType {
        let s = source.trim().to_ascii_lowercase();
        if s == "csi" {
            CameraType::Csi
        } else if ados_config::cameras::framebus_camera_id(&s).is_some() {
            CameraType::Framebus
        } else {
            CameraType::Usb
        }
    }

    /// Run one frame-bus pump per leg whose source is a captured camera, and
    /// stop the pumps of legs that are gone. Called before any encoder that
    /// reads a pipe is spawned.
    pub(crate) fn reconcile_framebus_feeds(&mut self) {
        let want: Vec<(String, crate::encoder::RawInput)> = self
            .legs
            .iter()
            .filter_map(|l| {
                let id = ados_config::cameras::framebus_camera_id(&l.source)?;
                Some((id.to_string(), l.raw_input()?))
            })
            .collect();
        self.framebus_feeds.reconcile(&want);
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
            let mut params = EncoderParams::from_camera_config(kind, &cam_cfg);
            params.framebus_input = leg.raw_input();
            let output = format!("rtsp://localhost:{port}/{}", leg.id);
            let cmd = match build_encoder_command(
                &params,
                &leg.source,
                &output,
                Some(&camera),
                &self.env,
            ) {
                Ok(c) if !c.is_empty() => c,
                Ok(_) => {
                    tracing::warn!(id = %leg.id, "secondary_encoder_command_empty");
                    continue;
                }
                Err(e) => {
                    // The reason matters here: an unsupported per-leg codec is
                    // the most likely cause and it is invisible without it.
                    tracing::warn!(
                        id = %leg.id,
                        error = %e,
                        "secondary_encoder_command_build_failed"
                    );
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
                    self.note_secondary_spawned(&leg.id, proc);
                    tracing::info!(id = %leg.id, encoder = ?kind, "secondary_encoder_started");
                }
                Err(e) => {
                    tracing::warn!(id = %leg.id, error = %e, "secondary_encoder_spawn_failed");
                }
            }
        }
    }

    /// Adopt a freshly spawned secondary encoder for `id`, replacing any dead
    /// slot, and start its liveness window over.
    ///
    /// The new encoder is a new publisher on the leg's mediamtx path, whose byte
    /// counter restarts with it. Carrying the previous encoder's flat-since stamp
    /// and `live: false` across the respawn read the fresh encoder as mute the
    /// moment it was born, so the next tick killed it again before it could
    /// publish a frame.
    pub(crate) fn note_secondary_spawned(&mut self, id: &str, proc: ManagedProcess) {
        self.secondary_encoders.retain(|(leg, _)| leg != id);
        self.secondary_encoders.push((id.to_string(), proc));
        self.leg_inbound.remove(id);
        self.leg_live.remove(id);
    }

    /// Terminate every owned secondary-leg encoder.
    pub async fn stop_secondary_encoders(&mut self) {
        for (_, mut proc) in std::mem::take(&mut self.secondary_encoders) {
            proc.terminate(Duration::from_secs(5)).await;
        }
    }

    /// Best-effort respawn of any DEAD or MUTE secondary-leg encoder. Isolated
    /// from the primary restart ladder; a leg that stays broken is retried on
    /// every health tick, forever, like every other ladder. Called at the end of
    /// the running tick.
    pub async fn supervise_secondary_encoders(&mut self) {
        if self.state != PipelineState::Running {
            return;
        }
        // "Dead" is NOT just "exited". `sample_leg_liveness` derives a per-leg
        // `live` flag from each leg's mediamtx inbound-byte counter; process
        // liveness is never proof of work, so a leg the byte counter says is not
        // live is fed into the same respawn path that handles an exit.
        //
        // Only an explicit `Some(false)` counts. A leg absent from `leg_live` is
        // UNKNOWN (a `sourceOnDemand` network pull with no reader attached is
        // legitimately flat and is deliberately recorded as unknown rather than
        // degraded, and a freshly respawned encoder has not been sampled yet),
        // and a network-pull leg owns no process here anyway.
        //
        // One pass over the encoders (is_running polls the child, so it needs
        // `&mut` and cannot run inside a closure that also borrows `leg_live`).
        let mute: std::collections::HashSet<String> = self
            .leg_live
            .iter()
            .filter(|(_, live)| !**live)
            .map(|(id, _)| id.clone())
            .collect();
        let mut dead_ids: Vec<String> = Vec::new();
        for (id, p) in self.secondary_encoders.iter_mut() {
            if !p.is_running() {
                dead_ids.push(id.clone());
            } else if mute.contains(id) {
                tracing::warn!(
                    leg = %id,
                    "secondary_encoder_mute: alive but inbound bytes flat; respawning"
                );
                dead_ids.push(id.clone());
            }
        }
        self.secondary_encoders
            .retain(|(id, _)| !dead_ids.contains(id));
        // Re-run the spawn pass every tick: it starts every local leg that is
        // not running — a leg just reaped above, and a leg whose earlier spawn
        // failed — and leaves running legs alone.
        self.start_secondary_encoders().await;
    }
}
