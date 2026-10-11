//! Pipeline teardown and the per-tick health checks.

use super::HealthFailure;
use crate::health::{
    grace_decision, inbound_decision, GraceDecision, InboundDecision, PipelineState,
};
use crate::mediamtx::MAIN_PATH;
use crate::recovery::RETRY_INTERVAL;
use crate::wfb_tee::{
    wfb_tee_output_is_stalled, wfb_tee_progress_is_stale, WFB_TEE_PROGRESS_TIMEOUT,
};
use std::time::{Duration, Instant};

use super::VideoOrchestrator;

impl VideoOrchestrator {
    /// Stop the encoding pipeline and mediamtx. Teardown order: wfb_tee →
    /// sei_tap → cloud_push → encoder → mediamtx.
    pub async fn stop_stream(&mut self) {
        tracing::info!("stop_stream_begin");
        self.stop_wfb_tee().await;
        self.stop_vision_tap().await;
        self.stop_secondary_encoders().await;
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
        // The encoder is gone with the pipeline; keeping its identity would
        // read as "something is still encoding".
        self.encoder_label = None;
        // Without this a deliberate stop leaves the sidecar claiming
        // `streaming` for the entire stopped window, and every status consumer
        // keeps offering a WHEP endpoint nothing is publishing to.
        self.persist_pipeline_outcome(crate::camera_state::PipelineOutcome::Stopped)
            .await;
        tracing::info!("pipeline_stopped");
    }

    /// One health probe: encoder PID alive + mediamtx alive + the startup-grace /
    /// path-ready / inbound-stall ladder. Returns the overall health verdict;
    /// side-effects (latching first-packet, recording bytes/s) are applied here against
    /// the pure decision helpers.
    pub async fn check_health(&mut self) -> bool {
        self.check_health_detail().await.is_ok()
    }

    /// [`Self::check_health`] with the cause of a failure, so the recovery
    /// can respawn only the encoder when only the encoder failed.
    pub(crate) async fn check_health_detail(&mut self) -> Result<(), HealthFailure> {
        // Encoder liveness.
        let Some(enc) = self.encoder.as_mut() else {
            return Err(HealthFailure::EncoderExited);
        };
        if !enc.is_running() {
            tracing::warn!("encoder_process_exited");
            return Err(HealthFailure::EncoderExited);
        }
        // mediamtx liveness — a dead mediamtx leaves ffmpeg blocked on a dead
        // RTSP socket while still looking alive.
        if !self.mediamtx.is_running() {
            tracing::warn!("mediamtx_died_during_stream");
            return Err(HealthFailure::MediamtxDied);
        }

        let elapsed = Instant::now().saturating_duration_since(self.started_at);
        let path_ready = self.mediamtx.path_ready(MAIN_PATH).await;

        // Startup-grace ladder until the first packet is seen.
        if !self.first_packet_seen {
            match grace_decision(path_ready, elapsed) {
                GraceDecision::FirstPacket => {
                    self.first_packet_seen = true;
                    self.no_first_packet_failures = 0;
                    tracing::info!(elapsed_s = elapsed.as_secs_f64(), "pipeline_first_packet");
                    return Ok(());
                }
                GraceDecision::StillWaiting => return Ok(()),
                GraceDecision::Expired => {
                    tracing::warn!(
                        elapsed_s = elapsed.as_secs_f64(),
                        "pipeline_grace_expired: no mediamtx publisher after grace window"
                    );
                    return Err(HealthFailure::GraceExpired);
                }
            }
        }

        // Live: the publisher must still be ready.
        if !path_ready {
            tracing::warn!("mediamtx_path_not_ready: encoder RTSP connection likely dead");
            return Err(HealthFailure::PathLost);
        }

        // Inbound-flow watchdog: assert mediamtx's byte counter advances.
        if self.check_inbound_flow_healthy().await {
            Ok(())
        } else {
            Err(HealthFailure::InboundStalled)
        }
    }

    /// Inbound-byte stall watchdog. Reads mediamtx's per-path `bytesReceived`, applies
    /// the pure [`inbound_decision`], and records the bytes/s. Falls back to healthy on
    /// an unreadable counter (a transient API hiccup must never force a needless
    /// restart).
    pub(super) async fn check_inbound_flow_healthy(&mut self) -> bool {
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
            RETRY_INTERVAL,
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

    /// Is the cloud push alive AND producing? `true` when healthy or — for the
    /// absent slot — only when cloud relay is NOT configured to be running here.
    ///
    /// The None branch is the same shape as the wfb-tee None-branch: a full pipeline
    /// restart (`stop_stream` → `start_stream`) clears `cloud_push` and `start_stream`
    /// does NOT re-arm it, so reporting healthy for None would leave a configured cloud
    /// push dead for the rest of the process lifetime. Report unhealthy for the absent
    /// slot WHILE cloud is enabled AND the pipeline is Running so the run-loop ladder
    /// re-arms it; when cloud is disabled, or the pipeline is not running, the absent
    /// slot is correct and healthy.
    ///
    /// The Some branch used to be `is_running()` and nothing else, which accepts
    /// a push wedged on a half-open relay socket as healthy: alive, holding its
    /// PID, delivering nothing to the cloud, forever. It now carries the same
    /// two-signal check the wfb tap has — the progress-token stamp plus the
    /// output-BYTE counter, because ffmpeg keeps printing `progress=continue`
    /// with a frozen `total_size` when its input wedges, so the token stamp
    /// alone reads such a push as alive.
    pub(super) async fn check_cloud_push_health(&mut self) -> bool {
        let Some(p) = self.cloud_push.as_mut() else {
            return !(self.config.cloud_enabled() && self.state == PipelineState::Running);
        };
        if !p.is_running() {
            tracing::warn!("cloud_push_process_exited");
            self.cloud_push = None;
            return false;
        }
        let last = self.cloud_push_progress.last_progress_at().await;
        if wfb_tee_progress_is_stale(last, Instant::now()) {
            tracing::warn!(
                threshold_s = WFB_TEE_PROGRESS_TIMEOUT.as_secs(),
                "cloud_push_zombie_detected: alive but progress flat; forcing restart"
            );
            return false;
        }
        // Gated on `output_seen` so it can never trip during the RTSP handshake
        // and first-IDR wait, where no output is expected yet.
        let output_seen = self.cloud_push_progress.last_output_bytes().await >= 0;
        let last_out = self.cloud_push_progress.last_output_advance_at().await;
        if wfb_tee_output_is_stalled(last_out, output_seen, Instant::now()) {
            tracing::warn!(
                threshold_s = WFB_TEE_PROGRESS_TIMEOUT.as_secs(),
                "cloud_push_zombie_detected: alive with progress tokens but output bytes flat; forcing restart"
            );
            return false;
        }
        true
    }

    /// Is the wfb tap alive AND producing? `true` when healthy, never started,
    /// or retired in favour of the encoder's own RTP output.
    ///
    /// With the fan-out in force there is no tap process and there must be no
    /// ladder chasing one — the radio leg is a branch of the encoder's single
    /// output, so it shares the encoder's fate: an RTP branch that cannot be
    /// opened fails the whole encoder (no `onfail=ignore` on the tee spec), and
    /// "the shared encode is producing" is already proved by the mediamtx
    /// inbound-byte delta watchdog. The independent downstream counter is then
    /// `wfb_tx`'s own injected-bytes rate, which `ados diag video` reads.
    pub(super) async fn check_wfb_tee_health(&mut self) -> bool {
        if self.encoder_emits_wfb_rtp {
            return true;
        }
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
        // Output-side check: ffmpeg keeps printing `progress=continue` with a
        // FROZEN `total_size` when its input wedges, so the progress-token stamp
        // above reads such a tap as alive. Trip on the output-byte counter having
        // gone flat (only after the tap has emitted at least one datagram, so the
        // RTSP handshake + first-IDR wait is never a false positive).
        let output_seen = self.wfb_tee_progress.last_output_bytes().await >= 0;
        let last_out = self.wfb_tee_progress.last_output_advance_at().await;
        if wfb_tee_output_is_stalled(last_out, output_seen, Instant::now()) {
            tracing::warn!(
                threshold_s = WFB_TEE_PROGRESS_TIMEOUT.as_secs(),
                "wfb_tee_zombie_detected: alive with progress tokens but output bytes flat; forcing restart"
            );
            return false;
        }
        true
    }

    /// Is the vision tap leg up? `true` when healthy or never started. The leg
    /// supervises its own decoder (spawned on engine connect, stopped when the
    /// engine is idle, respawned after an exit or a stall), so no engine
    /// attached is healthy-idle; only a leg whose task ended (its listener
    /// failed) needs the ladder.
    pub(super) async fn check_vision_tap_health(&mut self) -> bool {
        let Some(h) = self.vision_tap.as_ref() else {
            // No leg yet: the first bind is deferred until the RTSP source is
            // ready, so report unhealthy WHILE THE PIPELINE IS RUNNING and let
            // the run-loop ladder bring it up. When the pipeline is not running
            // the leg is correctly absent and this is healthy.
            return self.state != PipelineState::Running;
        };
        if h.is_finished() {
            tracing::warn!("vision_tap_leg_ended");
            self.vision_tap = None;
            return false;
        }
        true
    }
}
