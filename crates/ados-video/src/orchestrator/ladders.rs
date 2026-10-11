//! The service loop, the health tick and the recovery ladders.

use super::{counts_toward_software_fallback, software_rearm_due};
use crate::health::{
    sei_tap_session_wedged, should_fallback_to_software, PipelineState, SEI_TAP_SESSION_MAX,
};
use crate::mediamtx::MAIN_PATH;
use crate::recovery::{retry_ticker, tap_recovery, TapRecovery};
use ados_protocol::shutdown::Shutdown;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::VideoOrchestrator;

impl VideoOrchestrator {
    /// The main service loop. Drives the health tick (period
    /// [`RETRY_INTERVAL`], every ladder's one fixed retry interval) and the
    /// camera-hotplug-woken retry, terminating on `shutdown` with a full
    /// teardown of every child process.
    pub async fn run(mut self, shutdown: Shutdown) {
        tracing::info!("video_pipeline_service_start");
        let mut tick = retry_ticker();

        // 1 Hz telemetry: aggregate the pipeline's encoder metrics once a second
        // (NOT per encoder tick) and ship them to the logging daemon. Best-effort
        // and non-blocking; an absent daemon drops the low-severity samples.
        let telemetry = ados_protocol::logd::emitter::IngestEmitter::new("ados-video");
        let mut telemetry_tick = tokio::time::interval(Duration::from_secs(1));
        telemetry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Attention switches are applied the moment they are requested, not on
        // the next health tick: a hero promotion waiting on the tick would
        // leave the operator staring at a 1 fps thumbnail of the aircraft they
        // just selected.
        let mut desired_rx = self.encoder_control.subscribe_desired();
        // Close the startup window: the command socket is served before the cold
        // start, so a switch may already be pending. Subscribe first (so nothing
        // arriving from here on is missed), then reconcile once.
        self.apply_desired_encoder().await;
        // A fresh `/dev/video*` node retries an error-state pipeline at once
        // rather than on the next tick. A local handle so the select arm borrows it,
        // not `self`.
        let camera_plugged = Arc::clone(&self.camera_plugged);

        loop {
            tokio::select! {
                _ = shutdown.wait() => {
                    tracing::info!("video_pipeline_shutdown");
                    break;
                }
                _ = tick.tick() => {
                    self.tick_once().await;
                }
                _ = camera_plugged.notified() => {
                    // A new video device: discover afresh and give the
                    // hardware encoder another chance.
                    self.rediscover_requested = true;
                    self.rearm_hardware_encoder("camera_hotplug");
                    if self.state != PipelineState::Running {
                        tracing::info!("camera_plugged: retrying the pipeline now");
                        self.tick_retry_from_error().await;
                    }
                }
                _ = telemetry_tick.tick() => {
                    self.emit_telemetry(&telemetry);
                }
                res = desired_rx.changed() => {
                    if res.is_err() {
                        // Unreachable while this orchestrator holds the control
                        // Arc, but a closed channel returns Err immediately
                        // forever — park on shutdown rather than spin the loop.
                        shutdown.wait().await;
                        break;
                    }
                    self.apply_desired_encoder().await;
                }
            }
        }

        // Final teardown: wfb tee → vision tap → secondary legs → cloud push →
        // encoder → mediamtx. The ManagedProcess Drop killpg is the backstop;
        // this is the graceful ordered path.
        self.stop_wfb_tee().await;
        self.stop_vision_tap().await;
        self.stop_secondary_encoders().await;
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
    pub(super) fn emit_telemetry(&self, emitter: &ados_protocol::logd::emitter::IngestEmitter) {
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
    pub(super) async fn tick_once(&mut self) {
        match self.state {
            PipelineState::Running => self.tick_running().await,
            PipelineState::Error | PipelineState::Stopped => self.tick_retry_from_error().await,
            PipelineState::Starting => {
                // A transient Starting state outside start_stream() shouldn't
                // persist; nothing to do this tick.
            }
        }
    }

    /// The Running-state ladder: the primary's health first, then every
    /// consumer of `main` ([`Self::tick_consumers`]). Every ladder acts on this
    /// tick and retries on the next one; nothing in here sleeps, so the run
    /// loop stays responsive to shutdown and to an attention switch.
    pub(super) async fn tick_running(&mut self) {
        let health = self.check_health_detail().await;
        let health_ok = health.is_ok();
        // Sample per-leg liveness, then re-stamp the leg-list sidecar EVERY tick,
        // regardless of the PRIMARY's health: the advertised legs are static
        // (resolved at startup) and the secondary legs are served by mediamtx
        // independently of the primary, so an unhealthy primary (mid
        // restart-ladder) must not let the sidecar go stale and drop the healthy
        // secondary legs from the heartbeat/switcher.
        self.sample_leg_liveness().await;
        self.refresh_video_streams().await;
        if health_ok {
            self.note_healthy_tick();
            self.refresh_camera_state().await;
        }

        // Supervise the isolated secondary-leg encoders (best-effort respawn of
        // any dead one). Independent of the primary's health ladder below.
        self.supervise_secondary_encoders().await;

        if let Err(failure) = health {
            self.software_healthy_since = None;
            self.note_failed_run();
            self.restart_count += 1;
            tracing::warn!(
                attempt = self.restart_count,
                cause = ?failure,
                "pipeline_health_check_failed: restarting"
            );
            // The encoder alone failed and mediamtx is up: re-discover (the
            // camera may have renegotiated) and respawn only the encoder, so
            // WHEP viewers, network-pull legs, recording segments and the cloud
            // push keep their mediamtx session. Anything else, or a respawn
            // that fails, takes the full restart.
            if failure.is_encoder_side() && self.mediamtx.is_running() {
                self.refresh_discovery(true).await;
                if self.restart_encoder_only().await {
                    self.restart_count = 0;
                    return;
                }
            }
            let _guard = self.restart_lock.clone().lock_owned().await;
            self.stop_stream().await;
            let ok = self.start_stream().await;
            drop(_guard);
            if ok {
                self.restart_count = 0;
            }
            return;
        }
        self.tick_consumers().await;
    }

    /// The consumer ladders, run on every tick the primary is healthy: cloud
    /// push → wfb tap → vision tap → SEI tap.
    ///
    /// The ladders are independent and none returns early, so on the tick
    /// that first sees `main` published again every tap is respawned inside
    /// that one tick; a tap respawn never holds the next tap over to a later
    /// tick. An unhealthy tap is respawned at once when `main` has a publisher
    /// and torn down to be looked at again next tick when it has none
    /// ([`tap_recovery`]), so there is no hot-loop against a dead source.
    pub(crate) async fn tick_consumers(&mut self) {
        // The cloud relay is the SECONDARY path and its re-arm is a bare
        // spawn, so an unreachable relay costs one short-lived ffmpeg per tick
        // and never delays the tap recovery below.
        self.tick_cloud_push().await;

        // The wfb tee ladder (video retries forever).
        if !self.check_wfb_tee_health().await {
            match tap_recovery(self.mediamtx.path_ready(MAIN_PATH).await) {
                TapRecovery::Defer => {
                    tracing::warn!(
                        "wfb_tee_source_down: RTSP source not ready; deferring tee respawn"
                    );
                    self.stop_wfb_tee().await;
                }
                TapRecovery::Respawn => {
                    self.wfb_tee_restart_count += 1;
                    tracing::warn!(attempt = self.wfb_tee_restart_count, "wfb_tee_restarting");
                    self.stop_wfb_tee().await;
                    self.start_wfb_tee().await;
                    if self.wfb_tee.is_some() {
                        self.wfb_tee_restart_count = 0;
                    }
                }
            }
        }

        // Vision tap ladder (additive). Same shape as the wfb tap. An additive
        // consumer retries forever and never affects the encode/radio verdict.
        if self.vision_enabled() && !self.check_vision_tap_health().await {
            match tap_recovery(self.mediamtx.path_ready(MAIN_PATH).await) {
                TapRecovery::Defer => {
                    tracing::warn!(
                        "vision_tap_source_down: RTSP source not ready; deferring tap respawn"
                    );
                    self.stop_vision_tap().await;
                }
                TapRecovery::Respawn => {
                    self.vision_tap_restart_count += 1;
                    tracing::warn!(
                        attempt = self.vision_tap_restart_count,
                        "vision_tap_restarting"
                    );
                    self.stop_vision_tap().await;
                    self.start_vision_tap().await;
                    if self.vision_tap.is_some() {
                        self.vision_tap_restart_count = 0;
                    }
                }
            }
        }

        // SEI latency tap: if the one-shot tap exited (it runs a single read
        // session), respawn it. Latency telemetry retries forever; the spawn
        // itself defers while the path is not ready, so there is no hot-loop
        // against a dead source.
        //
        // `!is_running()` alone is not sufficient even for a one-shot. The tap
        // is a real RTSP consumer, and a session whose connect blocks forever
        // stays `is_running()` — so the exit check never fires, the tap is
        // never respawned, and latency telemetry stops with no symptom. A
        // one-shot's work is finite by construction, so its wall time IS the
        // delta signal: past the session ceiling it is wedged, not working.
        if self.sei_latency_on() {
            let exited = self
                .sei_tap
                .as_mut()
                .map(|p| !p.is_running())
                .unwrap_or(true);
            let wedged = !exited && sei_tap_session_wedged(self.sei_tap_started_at, Instant::now());
            if wedged {
                tracing::warn!(
                    ceiling_s = SEI_TAP_SESSION_MAX.as_secs(),
                    "sei_tap_zombie_detected: one-shot session past its ceiling; reaping"
                );
                if let Some(mut tap) = self.sei_tap.take() {
                    tap.terminate(Duration::from_secs(2)).await;
                }
                self.sei_tap_started_at = None;
            }
            if exited || wedged {
                self.sei_tap = None;
                self.sei_tap_started_at = None;
                self.start_sei_tap().await;
            }
        }
    }

    /// The cloud-push re-arm step, run on every Running tick.
    ///
    /// On an unhealthy push it tears down the stale process and spawns a fresh
    /// one; a relay that stays unreachable is retried on the next tick. On a
    /// healthy push it clears the failure counter.
    pub(super) async fn tick_cloud_push(&mut self) {
        if self.check_cloud_push_health().await {
            // Healthy (or not configured / not running).
            self.cloud_restart_count = 0;
            return;
        }
        // Unhealthy: the slot is absent (full-restart re-arm needed) or the
        // process exited or wedged. Reap any stale process first.
        self.stop_cloud_push().await;
        self.cloud_restart_count += 1;
        tracing::warn!(attempt = self.cloud_restart_count, "cloud_push_restarting");
        if self.start_cloud_push().await {
            self.cloud_restart_count = 0;
        }
    }

    /// The Error/Stopped retry path: one cold-start attempt per tick (or per
    /// camera hotplug), forever.
    pub(super) async fn tick_retry_from_error(&mut self) {
        self.restart_count += 1;
        tracing::info!(attempt = self.restart_count, "pipeline_retry_from_error");
        if self.start_stream().await {
            self.restart_count = 0;
            tracing::info!("pipeline_recovered: stream started after retry");
        }
    }

    /// Count a run that never produced a first packet toward the software
    /// fallback, but only when the encoder itself is to blame (see
    /// [`counts_toward_software_fallback`]). A run that streamed then stalled
    /// resets the count.
    pub(super) fn note_failed_run(&mut self) {
        if self.first_packet_seen {
            self.no_first_packet_failures = 0;
            return;
        }
        let network = self.camera_cfg.network_source().is_some();
        let errors = self.encoder_errors.load(Ordering::Acquire);
        if !counts_toward_software_fallback(network, errors) {
            tracing::info!(
                network_source = network,
                encoder_error_lines = errors,
                "encoder_failure_not_attributed_to_encoder: software fallback not counted"
            );
            return;
        }
        self.no_first_packet_failures += 1;
        if should_fallback_to_software(self.no_first_packet_failures, self.force_software) {
            tracing::warn!(
                failures = self.no_first_packet_failures,
                "encoder_fallback_to_software: the hardware/GStreamer encoder failed to produce video; forcing the software libx264 path"
            );
            self.force_software = true;
            self.no_first_packet_failures = 0;
        }
    }

    /// Track a healthy tick on the forced software encoder and re-arm the
    /// hardware path once the stretch is long enough.
    pub(super) fn note_healthy_tick(&mut self) {
        if !self.force_software {
            self.software_healthy_since = None;
            return;
        }
        let now = Instant::now();
        let since = *self.software_healthy_since.get_or_insert(now);
        if software_rearm_due(Some(since), now) {
            self.rearm_hardware_encoder("healthy_on_software");
        }
    }

    /// Clear the software override so the next encoder start tries the
    /// hardware path again.
    pub(crate) fn rearm_hardware_encoder(&mut self, reason: &'static str) {
        if self.force_software {
            tracing::info!(reason, "encoder_hardware_rearmed");
        }
        self.force_software = false;
        self.software_healthy_since = None;
        self.no_first_packet_failures = 0;
    }
}
