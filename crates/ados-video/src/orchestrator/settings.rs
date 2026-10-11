//! Encoder settings, attention-state sidecar, camera state and leg liveness.

use super::{
    apply_settings_to, ceiling_change_is_deferrable, leg_counter_moved, liveness_on_flat,
    CEILING_DEFER_PERCENT,
};
use crate::health::PipelineState;
use crate::profile::{EncoderSettings, EncoderState, VideoProfile};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Notify;

use super::VideoOrchestrator;

impl VideoOrchestrator {
    /// The attention-state sidecar path (canonical unless overridden in tests).
    pub(super) fn video_profile_path(&self) -> &std::path::Path {
        self.video_profile_path
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(crate::profile::VIDEO_PROFILE_SIDECAR))
    }

    /// Publish the attention state so the out-of-process readers (the state
    /// snapshot that feeds the swarm beacon's hero bit, the adaptive ladder's
    /// self-heal check) see what the encoder is actually running.
    pub(crate) async fn publish_encoder_state(&self, state: &EncoderState) {
        let path = self.video_profile_path().to_path_buf();
        if let Err(e) = crate::profile::write_sidecar_async(path, state.clone()).await {
            tracing::warn!(error = %e, "video_profile_sidecar_write_failed");
        }
    }

    /// The settings the currently-desired profile + ceiling resolve to.
    pub(crate) fn desired_encoder_settings(&self) -> (crate::profile::Desired, EncoderSettings) {
        let d = self.encoder_control.desired();
        let mut s = match d.profile {
            VideoProfile::Hero => self.hero_base,
            VideoProfile::Thumbnail => self.camera_cfg.thumbnail,
        };
        if let Some(c) = d.ceiling_kbps {
            s.bitrate_kbps = s.bitrate_kbps.min(c);
        }
        (d, s)
    }

    /// The encoder settings currently live in [`Self::camera_cfg`].
    pub(crate) fn live_encoder_settings(&self) -> EncoderSettings {
        EncoderSettings {
            width: self.camera_cfg.width,
            height: self.camera_cfg.height,
            fps: self.camera_cfg.fps,
            bitrate_kbps: self.camera_cfg.bitrate_kbps,
        }
    }

    /// The state to publish and acknowledge: the resolved profile + settings
    /// plus the two orchestrator-owned observability fields, so no call site
    /// can publish a sidecar that silently drops the respawn counter.
    pub(crate) fn encoder_state(
        &self,
        profile: VideoProfile,
        ceiling_kbps: Option<u32>,
        settings: EncoderSettings,
        ceiling_deferred: bool,
    ) -> EncoderState {
        EncoderState::new(profile, ceiling_kbps, settings)
            .with_ceiling_deferred(ceiling_deferred)
            .with_encoder_respawns(self.encoder_respawns)
    }

    /// Reconcile the encoder against the desired profile + bitrate ceiling.
    ///
    /// Three outcomes, in order:
    ///
    /// 1. **Already live** — acknowledge without touching a process.
    /// 2. **A small ceiling-only change** — acknowledge the ceiling as accepted
    ///    but HOLD it on the encoder, and stamp `ceiling_deferred` on the
    ///    sidecar. The radio half of this loop applies FEC and MCS through the
    ///    `wfb_tx` management socket precisely so a retune costs no video gap;
    ///    the encoder half has no such channel, so its only actuator is a
    ///    respawn — a fresh SPS/PPS, a decoder reset and a visible black frame,
    ///    at exactly the moment the picture matters most. Below
    ///    [`CEILING_DEFER_PERCENT`] of the live bitrate that trade is not worth
    ///    making: the FEC ladder absorbs the difference.
    /// 3. **Anything else** (a profile switch, a geometry or fps change, or a
    ///    ceiling step at or above the threshold) — respawn only the encoder
    ///    child. mediamtx, the radio leg, the cloud push and the vision tap all
    ///    keep running.
    ///
    /// The deferral compares the target against what is LIVE, never against the
    /// previous request, so a sequence of small steps cannot accumulate past
    /// the threshold unnoticed: once the gap from the running bitrate reaches
    /// it, the next request applies.
    pub(crate) async fn apply_desired_encoder(&mut self) {
        let (desired, target) = self.desired_encoder_settings();
        let live = self.live_encoder_settings();

        if target == live {
            let state = self.encoder_state(desired.profile, desired.ceiling_kbps, target, false);
            let state = self
                .encoder_control
                .note_applied(state, desired.generation, false);
            self.publish_encoder_state(&state).await;
            return;
        }

        // Case 2: hold the rung. Only while Running — with no encoder up,
        // applying is free (the next cold start builds from the clamped
        // settings), so there is nothing to defer.
        if self.state == PipelineState::Running && ceiling_change_is_deferrable(live, target) {
            let state = self.encoder_state(desired.profile, desired.ceiling_kbps, live, true);
            tracing::info!(
                ceiling_kbps = ?desired.ceiling_kbps,
                live_bitrate_kbps = live.bitrate_kbps,
                target_bitrate_kbps = target.bitrate_kbps,
                threshold_percent = CEILING_DEFER_PERCENT,
                "video_ceiling_change_deferred: holding the rung instead of respawning the encoder"
            );
            let state = self
                .encoder_control
                .note_applied(state, desired.generation, false);
            self.publish_encoder_state(&state).await;
            return;
        }

        apply_settings_to(&mut self.camera_cfg, target);
        tracing::info!(
            profile = %desired.profile,
            width = target.width,
            height = target.height,
            fps = target.fps,
            bitrate_kbps = target.bitrate_kbps,
            "video_attention_profile_change"
        );

        // A pipeline that is not Running has no encoder to swap: the new
        // settings are already in `camera_cfg`, so the next cold start builds
        // its command from them. Acknowledge honestly either way.
        let restarted = if self.state == PipelineState::Running {
            self.restart_encoder_only().await
        } else {
            false
        };
        let state = self.encoder_state(desired.profile, desired.ceiling_kbps, target, false);
        let state = self
            .encoder_control
            .note_applied(state, desired.generation, restarted);
        self.publish_encoder_state(&state).await;
    }

    /// Re-persist the camera-state sidecar from the last-known discovery
    /// snapshot, stamped with the CURRENT pipeline outcome. Run on every
    /// healthy tick so the staleness gate always sees a fresh stamp.
    ///
    /// It goes through [`Self::persist_pipeline_outcome`] because this runs on
    /// every health tick: rebuilding the snapshot from discovery alone
    /// yields `pipeline_state: unknown`, which erased the `streaming` stamp
    /// (and the circuit breaker's reason with it) one tick after the pipeline
    /// came up — a streaming drone then read as `not_initialized`, with no
    /// playable endpoint, to every status consumer.
    ///
    /// A running pipeline is `streaming` only once the first packet reached
    /// mediamtx's `main` path (`first_packet_seen`, latched by the health
    /// check). Before that the encoder exists but publishes nothing: a network
    /// source that never answers, or a respawned encoder that has not
    /// reconnected yet, reads `starting`, never as a stream a consumer could
    /// pull.
    pub(super) async fn refresh_camera_state(&self) {
        self.persist_pipeline_outcome(match self.state {
            PipelineState::Running if self.first_packet_seen => {
                crate::camera_state::PipelineOutcome::Streaming
            }
            PipelineState::Running | PipelineState::Starting => {
                crate::camera_state::PipelineOutcome::Starting
            }
            PipelineState::Error => crate::camera_state::PipelineOutcome::Error,
            PipelineState::Stopped => crate::camera_state::PipelineOutcome::Stopped,
        })
        .await;
    }

    /// Sample each leg's mediamtx inbound-byte counter and derive per-leg
    /// liveness (`leg_live`): a leg whose counter moved is live; a leg flat
    /// for `LEG_FLAT_SECS` is stalled. Surfaces a dead secondary leg in the
    /// sidecar. mediamtx owns per-path re-establishment for a
    /// `sourceOnDemand` pull when a reader re-attaches; this is the detection.
    pub(crate) async fn sample_leg_liveness(&mut self) {
        const LEG_FLAT_SECS: u64 = 30;
        let now = Instant::now();
        // Only the extra legs need sampling here — the primary "main" leg's flow
        // is already watched by `check_inbound_flow_healthy`; sample it too for a
        // uniform per-leg surface.
        let legs: Vec<(String, bool)> = self
            .legs
            .iter()
            .map(|l| (l.id.clone(), l.is_network_pull))
            .collect();
        for (id, is_network_pull) in legs {
            let Some(cur) = self.mediamtx.inbound_bytes(&id).await else {
                // No sample (path not up yet) → leave prior liveness untouched.
                continue;
            };
            let entry = self.leg_inbound.entry(id.clone()).or_insert((cur, now));
            if leg_counter_moved(entry.0, cur) {
                entry.0 = cur;
                entry.1 = now;
                self.leg_live.insert(id, true);
            } else if now.saturating_duration_since(entry.1).as_secs() >= LEG_FLAT_SECS {
                match liveness_on_flat(is_network_pull) {
                    Some(v) => {
                        self.leg_live.insert(id, v);
                    }
                    None => {
                        self.leg_live.remove(&id);
                    }
                }
            }
        }
        // Drop liveness for legs that no longer exist (a config change).
        let present: std::collections::HashSet<&str> =
            self.legs.iter().map(|l| l.id.as_str()).collect();
        self.leg_inbound.retain(|k, _| present.contains(k.as_str()));
        self.leg_live.retain(|k, _| present.contains(k.as_str()));
    }

    /// Write the video-streams sidecar (the resolved leg list: id/role/codec)
    /// so the out-of-process status surfaces can advertise each leg + the GCS
    /// can populate its stream switcher. Best-effort — a write failure is
    /// logged, never fatal. Honors [`Self::video_streams_path`].
    pub(crate) async fn refresh_video_streams(&self) {
        let snap =
            crate::video_streams::VideoStreamsSnapshot::from_legs(&self.legs, &self.leg_live);
        let path = self
            .video_streams_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(crate::video_streams::VIDEO_STREAMS_JSON));
        if let Err(e) = snap.write_to_async(path).await {
            tracing::warn!(error = %e, "video_streams_sidecar_write_failed");
        }
    }

    /// The Notify handle SIGUSR1 fires to retry an error-state pipeline at once
    /// instead of on the next tick.
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

    /// The `(path, source)` pairs to feed [`MediamtxManager::write_config`]: an
    /// owned-encoder leg (the primary always, or a local secondary) is a
    /// `"publisher"` (its encoder publishes into the path), and a secondary
    /// network leg is its RTSP source (a mediamtx `sourceOnDemand` pull). Back-
    /// compat: a single `main` leg yields exactly `[("main","publisher")]`.
    pub(crate) fn legs_to_streams(&self) -> Vec<(String, String)> {
        self.legs
            .iter()
            .map(|leg| {
                if leg.is_network_pull {
                    (leg.id.clone(), leg.source.clone())
                } else {
                    (leg.id.clone(), "publisher".to_string())
                }
            })
            .collect()
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
}
