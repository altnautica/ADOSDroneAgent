//! Recovery of the primary's input stage: the encoder and the source it reads.
//!
//! A source outage is the common failure on a live node (a network camera
//! rebooting, a link dropping, a captured camera's vision engine reopening its
//! source), and it is a failure of the input stage alone. mediamtx, the WHEP
//! path and every reader of `main` are healthy throughout, so recovery respawns
//! only the encoder and leaves them up; only a mediamtx failure, or an encoder
//! that cannot be rebuilt, takes the full pipeline restart.
//!
//! Recovery also has to be prompt when the source returns, not on whatever
//! schedule the failure left behind:
//! - while the inbound counter is flat, the run loop re-checks the input every
//!   [`crate::recovery::INPUT_WATCH_INTERVAL`], so a lost publisher or an
//!   exited encoder is acted on within that interval, not on the next tick;
//! - a network source that refuses or drops connections holds the encoder
//!   instead of spawning one whose connect would sit in the kernel's SYN
//!   retransmit schedule, and is probed until it answers
//!   ([`ados_protocol::source_probe`]); the encoder is respawned the moment it
//!   does;
//! - a frame-bus encoder waits on its pipe, so it is ready the moment the
//!   vision engine publishes a frame again.

use super::{HealthFailure, VideoOrchestrator};
use crate::health::PipelineState;
use crate::mediamtx::MAIN_PATH;
use ados_protocol::source_probe::SourceAddr;

/// The outcome of an input-stage respawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputRespawn {
    /// A fresh encoder is running.
    Respawned,
    /// The network source does not answer: the encoder is held and the source
    /// probe brings it back.
    AwaitingSource,
    /// The encoder could not be rebuilt or spawned.
    Failed,
}

/// Resolve once the held source answers; never while no source is held.
pub(super) async fn wait_for_source(source: Option<SourceAddr>) {
    match source {
        Some(source) => source.wait_reachable().await,
        None => std::future::pending().await,
    }
}

impl VideoOrchestrator {
    /// Recover the primary after `failure`: the input stage alone when only it
    /// failed and mediamtx is up, otherwise (or when the encoder cannot be
    /// rebuilt) the full pipeline restart.
    pub(super) async fn recover_primary(&mut self, failure: HealthFailure) {
        self.software_healthy_since = None;
        self.note_failed_run();
        self.restart_count += 1;
        tracing::warn!(
            attempt = self.restart_count,
            cause = ?failure,
            "pipeline_health_check_failed: restarting"
        );
        if failure.is_encoder_side() && self.mediamtx.is_running() {
            match self.respawn_input_stage().await {
                InputRespawn::Respawned => {
                    self.restart_count = 0;
                    return;
                }
                InputRespawn::AwaitingSource => return,
                InputRespawn::Failed => {}
            }
        }
        let _guard = self.restart_lock.clone().lock_owned().await;
        self.stop_stream().await;
        let ok = self.start_stream().await;
        drop(_guard);
        if ok {
            self.restart_count = 0;
        }
    }

    /// Respawn only the encoder. Re-discovers first (a local camera may have
    /// renegotiated). A network source is probed before the spawn: when it does
    /// not answer, the outgoing encoder is stopped and the new one held until
    /// the source probe sees it back.
    pub(crate) async fn respawn_input_stage(&mut self) -> InputRespawn {
        self.refresh_discovery(true).await;
        if let Some(source) = self
            .camera_cfg
            .network_source()
            .and_then(SourceAddr::from_url)
        {
            if !source.reachable().await {
                if self.source_wait.is_none() {
                    tracing::warn!(
                        host = %source.host,
                        port = source.port,
                        "primary_source_unreachable: encoder held until the source answers"
                    );
                }
                if let Some(enc) = self.encoder.take() {
                    crate::recovery::stop_stages(vec![enc]).await;
                }
                self.source_wait = Some(source);
                return InputRespawn::AwaitingSource;
            }
        }
        self.source_wait = None;
        if self.restart_encoder_only().await {
            InputRespawn::Respawned
        } else {
            InputRespawn::Failed
        }
    }

    /// The held network source answered: respawn the encoder now.
    pub(super) async fn on_source_answered(&mut self) {
        if self.state != PipelineState::Running {
            // A cold start opens the source itself.
            self.source_wait = None;
            return;
        }
        tracing::info!("primary_source_answered: respawning the encoder");
        match self.respawn_input_stage().await {
            InputRespawn::Respawned => self.restart_count = 0,
            InputRespawn::AwaitingSource => {}
            // The next tick finds no encoder and takes the full restart.
            InputRespawn::Failed => self.source_wait = None,
        }
    }

    /// Whether the fast input watch runs: an encoder that had been streaming
    /// whose inbound counter was flat on the last sample.
    pub(super) fn input_watch_due(&self) -> bool {
        self.state == PipelineState::Running
            && self.encoder.is_some()
            && self.first_packet_seen
            && self.inbound_flat
    }

    /// One fast input check. The two input failures that are visible at once,
    /// the encoder exiting and its publisher leaving `main`, take the recovery
    /// now; everything else (and a mediamtx failure) is the tick's.
    pub(super) async fn watch_input(&mut self) {
        if !self.mediamtx.is_running() {
            return;
        }
        let exited = self.encoder.as_mut().is_some_and(|enc| !enc.is_running());
        let failure = if exited {
            HealthFailure::EncoderExited
        } else if !self.mediamtx.path_ready(MAIN_PATH).await {
            HealthFailure::PathLost
        } else {
            return;
        };
        self.recover_primary(failure).await;
    }
}
