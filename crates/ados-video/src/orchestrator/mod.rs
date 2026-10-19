//! The video-pipeline FSM.
//!
//! This is the integration core that wires the leaf modules (camera
//! discovery, the encoder command builder, mediamtx, the wfb radio tap, the
//! camera-state sidecar) into the long-lived supervisor the drone runs on the
//! air side. It owns the full lifecycle: cold start, the health tick whose
//! period is every ladder's one fixed retry interval
//! ([`crate::recovery::RETRY_INTERVAL`]: pipeline restart, encoder respawn,
//! cloud push, wfb tap, vision tap, SEI tap, secondary legs), the
//! camera-hotplug-woken retry from the error state, and the RAII teardown of
//! every child process on shutdown.
//!
//! The sequencing follows the Python `VideoPipeline` orchestrator
//! (`services/video/pipeline/pipeline.py`): the same start order and the same
//! grace / inbound-stall / wfb-stale health rules. The process-group ownership
//! (setsid + killpg) that the Python
//! version did by hand lives structurally in [`crate::process::ManagedProcess`]
//! now, so a dropped future can never orphan a publish-bridge ffmpeg onto the
//! mediamtx `/main` slot.
//!
//! The encoder gets a PLAIN rate-limited stderr drain (NOT the wfb-tee's
//! progress tracker): the encoder is the source, its `frame=` cadence is the
//! wfb-tee's job to watch via the mediamtx inbound-byte counter, and attaching
//! a progress tracker to the encoder would conflate two independent liveness
//! signals.

use crate::config::{AgentVideoConfig, CameraConfig, ResolvedLeg};
use crate::discover::{self, DiscoveryResult};
use crate::encoder::{EncoderEnv, EncoderKind};
use crate::health::{PipelineState, StartError};
use crate::mediamtx::MediamtxManager;
use crate::process::ManagedProcess;
use crate::profile::{EncoderControl, EncoderSettings, VideoProfile};
use crate::wfb_tee::ProgressTracker;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};

mod health_checks;
mod ladders;
mod settings;

/// How long the pipeline must stream healthily on the forced software encoder
/// before the hardware path is re-armed for the next encoder start.
pub const SOFTWARE_REARM_AFTER: Duration = Duration::from_secs(600);

/// What made the primary pipeline unhealthy, so recovery can be scoped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthFailure {
    /// No encoder process, or it exited.
    EncoderExited,
    /// mediamtx died: everything downstream of it is gone too.
    MediamtxDied,
    /// The encoder never published within the startup grace.
    GraceExpired,
    /// The publisher left the path after streaming.
    PathLost,
    /// The publisher is present but the inbound byte counter is flat.
    InboundStalled,
}

impl HealthFailure {
    /// A failure of the encoder alone, recoverable by respawning the encoder
    /// while mediamtx (and every reader on it) keeps running.
    pub fn is_encoder_side(self) -> bool {
        matches!(
            self,
            Self::EncoderExited | Self::GraceExpired | Self::InboundStalled
        )
    }
}

/// Whether an unhealthy run that never produced a first packet counts toward
/// abandoning the hardware encoder. Only a local V4L2 source whose encoder
/// itself logged errors counts: a network camera that is unreachable, or a USB
/// camera slow to deliver, is a SOURCE failure that software encoding would
/// not fix.
pub fn counts_toward_software_fallback(network_source: bool, encoder_error_lines: u32) -> bool {
    !network_source && encoder_error_lines > 0
}

/// Whether a healthy stretch on the forced software encoder has lasted long
/// enough to re-arm the hardware path.
pub fn software_rearm_due(healthy_since: Option<Instant>, now: Instant) -> bool {
    healthy_since.is_some_and(|since| now.saturating_duration_since(since) >= SOFTWARE_REARM_AFTER)
}

// --- orchestrator ------------------------------------------------------------

/// Owns and supervises the air-side video pipeline subprocess tree.
///
/// The fields are `pub(crate)` so the subprocess start/stop lifecycle (defined
/// in [`crate::lifecycle`]) and the supervision FSM (defined here) can share
/// the same state across the two modules; nothing here is part of the crate's
/// public API.
pub struct VideoOrchestrator {
    pub(crate) config: AgentVideoConfig,
    pub(crate) camera_cfg: CameraConfig,
    /// The resolved video legs this node serves. The primary leg (`main`) is
    /// driven by the inline pipeline below (encoder / wfb / cloud / vision);
    /// secondary legs are additional mediamtx paths (`sourceOnDemand` pulls)
    /// added to the mediamtx config so mediamtx serves each at `:8889/<id>/whep`
    /// independently — one dead secondary source cannot wedge the primary or a
    /// sibling (per-path isolation is mediamtx's, not the orchestrator's). An
    /// empty `cameras` list resolves to a single `main` leg (back-compat).
    pub(crate) legs: Vec<ResolvedLeg>,

    /// Per-leg inbound-byte watchdog state (last counter value + when it last
    /// advanced), keyed by leg id. Feeds the per-leg `live` flag in the sidecar
    /// so a stalled secondary leg is surfaced honestly.
    pub(crate) leg_inbound: std::collections::HashMap<String, (u64, Instant)>,
    /// The derived per-leg liveness the sidecar stamps (`id -> is_live`).
    pub(crate) leg_live: std::collections::HashMap<String, bool>,

    pub(crate) mediamtx: MediamtxManager,
    pub(crate) encoder: Option<ManagedProcess>,
    /// Owned encoders for LOCAL secondary legs (keyed by leg id): each publishes
    /// its own camera into its own mediamtx path, LAN-WHEP-only. Structurally
    /// isolated from the primary pipeline (separate processes) — a dead secondary
    /// encoder cannot wedge the primary or a sibling. Network secondary legs are
    /// mediamtx `sourceOnDemand` pulls and own no process here. Empty on a
    /// single-leg / network-only node.
    pub(crate) secondary_encoders: Vec<(String, ManagedProcess)>,
    pub(crate) wfb_tee: Option<ManagedProcess>,
    /// True when the PRIMARY encoder's own argv carries the radio's RTP output,
    /// so [`Self::wfb_tee`] is deliberately `None` and must not be spawned,
    /// swept for, or reported unhealthy. Re-derived from the built argv on every
    /// (re)start, never from config.
    pub(crate) encoder_emits_wfb_rtp: bool,
    pub(crate) cloud_push: Option<ManagedProcess>,
    pub(crate) sei_tap: Option<ManagedProcess>,
    /// The vision frame tap leg: the bound socket plus the decoder it runs
    /// while a vision engine is attached (see [`crate::tap_lazy`]). Spawned
    /// only when `video.vision.enabled`. Additive: a crash here never touches
    /// the encode or wfb path.
    pub(crate) vision_tap: Option<tokio::task::JoinHandle<()>>,
    /// The frame-bus pumps feeding the encoders of captured cameras
    /// (`framebus://<id>` legs), one per camera.
    pub(crate) framebus_feeds: crate::framebus_input::Feeds,
    pub(crate) wfb_tee_progress: ProgressTracker,
    /// Output-progress clock for the cloud relay push. Process liveness is
    /// never proof of work: a cloud push wedged on a half-open relay socket
    /// holds its PID and pushes nothing, and the health check used to accept
    /// exactly that as healthy.
    pub(crate) cloud_push_progress: ProgressTracker,
    /// When the current one-shot SEI tap session began, or `None` when no tap
    /// is running. A `--once` tap is expected to EXIT, so `!is_running()` is
    /// its correct respawn trigger — but a session that never returns (an RTSP
    /// connect that blocks forever) stays `is_running()` and the exit check can
    /// never fire, so latency telemetry stops silently. This bounds one
    /// session's wall time, which is the delta-equivalent for work that is
    /// finite by construction.
    pub(crate) sei_tap_started_at: Option<Instant>,

    pub(crate) last_cameras: DiscoveryResult,
    pub(crate) encoder_type: Option<EncoderKind>,
    /// The on-wire encoder identity of the CURRENT command, resolved by
    /// [`crate::encoder::encoder_label`] once the argv is built. Held here
    /// rather than derived from [`Self::encoder_type`] because the kind cannot
    /// tell a hardware `h264_v4l2m2m` run from the software `libx264` fallback.
    /// `None` whenever no encoder command is in force.
    pub(crate) encoder_label: Option<String>,

    pub(crate) state: PipelineState,
    pub(crate) started_at: Instant,
    pub(crate) first_packet_seen: bool,

    /// mediamtx inbound-byte counter (`-1` ⇒ no sample yet).
    pub(crate) inbound_bytes_value: i64,
    pub(crate) inbound_bytes_changed_at: Instant,
    pub(crate) video_inbound_bytes_per_s: f64,

    /// Consecutive failed recovery attempts of each ladder, for the log lines
    /// only: every ladder retries on the next health tick however high these
    /// climb, and each is cleared by the attempt that succeeds.
    pub(crate) restart_count: u32,
    pub(crate) cloud_restart_count: u32,
    pub(crate) wfb_tee_restart_count: u32,
    pub(crate) vision_tap_restart_count: u32,
    /// Encoder-ONLY respawns since this service started: every time an
    /// attention switch or an adaptive-bitrate change actually re-spawned the
    /// encoder child. Published on the attention sidecar as `encoder_respawns`.
    ///
    /// Each respawn is a fresh SPS/PPS, a decoder reset and a visible black
    /// frame, so a rig where the adaptive ladder is oscillating shows up here as
    /// a climbing counter instead of having to be inferred from an operator's
    /// "the picture keeps blinking". The radio ladder keeps its own equivalent
    /// counter on the other half of the same loop.
    pub(crate) encoder_respawns: u64,
    pub(crate) last_start_error: StartError,
    /// Consecutive cold-start attempts whose encoder never produced a first
    /// packet. Drives the software fallback: a hardware / GStreamer encoder that
    /// cannot stream on this box is abandoned for ffmpeg libx264 rather than
    /// restarted against the same wall forever.
    pub(crate) no_first_packet_failures: u32,
    /// Latched once the hardware / GStreamer encoder has been abandoned,
    /// forcing the encoder command onto the software path. Cleared by a camera
    /// hotplug and after [`SOFTWARE_REARM_AFTER`] of healthy streaming, which
    /// re-arms the hardware path for the next encoder start.
    pub(crate) force_software: bool,
    /// Start of the current healthy stretch on the forced software encoder.
    pub(crate) software_healthy_since: Option<Instant>,
    /// Encoder-side error lines the current primary encoder has logged
    /// (reset at every spawn; fed by the encoder's stderr drain).
    pub(crate) encoder_errors: Arc<AtomicU32>,
    /// A hotplug asked for full camera discovery on the next start.
    pub(crate) rediscover_requested: bool,
    /// The sysfs video-device fingerprint at the last full discovery.
    pub(crate) last_fingerprint: Option<String>,
    /// When the last full discovery ran.
    pub(crate) last_discovery_at: Option<Instant>,

    /// Serializes teardown+respawn across the cold start, the health-check
    /// restart and the attention-switch respawn. A runtime camera change is not
    /// a distinct operation: a fresh `/dev/video*` node fires SIGUSR1 →
    /// `camera_plugged` → the retry path, which re-runs the same locked
    /// teardown+respawn, so there is no separate switch lock.
    pub(crate) restart_lock: Arc<Mutex<()>>,
    /// Fired by SIGUSR1 (a fresh `/dev/video*` node) so a pipeline sitting in
    /// the error state retries at once instead of on the next health tick.
    pub(crate) camera_plugged: Arc<Notify>,

    pub(crate) python_executable: String,
    pub(crate) env: EncoderEnv,

    /// Override for the camera-state sidecar path. `None` ⇒ the canonical
    /// `/run/ados/camera-state.json` contract path. Set only in tests so the
    /// re-persist can be asserted against a temp file.
    pub(crate) camera_state_path: Option<std::path::PathBuf>,

    /// Override for the video-streams sidecar path. `None` ⇒ the canonical
    /// `/run/ados/video-streams.json` contract path. Set only in tests.
    pub(crate) video_streams_path: Option<std::path::PathBuf>,

    /// The HERO profile's settings, captured from the `video.camera` block
    /// before any attention switch mutates [`Self::camera_cfg`]. Every profile
    /// resolution reads this, never the possibly-already-switched live values,
    /// so switching hero → thumbnail → hero returns to the configured values
    /// rather than latching whatever the last switch left behind.
    pub(crate) hero_base: EncoderSettings,
    /// The shared handle the encoder command socket requests through and this
    /// orchestrator acknowledges on. The ONE entry point for both the operator's
    /// hero choice and the adaptive ladder's bitrate clamp.
    pub(crate) encoder_control: Arc<EncoderControl>,

    /// Override for the attention-state sidecar path. `None` ⇒ the canonical
    /// `/run/ados/video-profile.json`. Set only in tests.
    pub(crate) video_profile_path: Option<std::path::PathBuf>,
}

impl VideoOrchestrator {
    /// Build an orchestrator for the given config. `config_dir` is where the
    /// mediamtx config file is written (canonically `/etc/ados`).
    ///
    /// `env` is passed in ALREADY PROBED rather than resolved here.
    /// [`EncoderEnv::detect`] shells `gst-inspect-1.0` up to three times, and
    /// `gst-inspect` on a cold SBC page cache is not fast — running it inline
    /// stalled a reactor worker that the video routes and every watchdog tick
    /// share. Taking it as a parameter means an async caller MUST resolve it
    /// through [`EncoderEnv::detect_async`] (which does the probe on the
    /// blocking pool), so the inline-blocking version cannot come back.
    pub fn new(
        config: AgentVideoConfig,
        camera_cfg: CameraConfig,
        config_dir: &std::path::Path,
        env: EncoderEnv,
    ) -> Self {
        let now = Instant::now();
        // On a multi-leg node the primary leg IS the primary camera: its source
        // and encode-plane settings (geometry, bitrate, orientation, encoder,
        // GOP) drive the primary encoder. Without this overlay every per-leg
        // edit of the primary was saved and then ignored in favour of the
        // legacy `video.camera` block.
        let camera_cfg = config.primary_camera_config(camera_cfg);
        let legs = config.resolve_legs(&camera_cfg);

        // Attention profile, resolved before the first encoder command is
        // built. The primary camera's settings ARE the hero profile, so capture
        // them now; the boot profile then overwrites the live capture settings
        // so a fleet node cold-starts at 320x180/1 fps instead of grabbing 48%
        // of the shared channel's airtime before the ground station can demote
        // it.
        let mut camera_cfg = camera_cfg;
        let hero_base = crate::profile::base_settings(VideoProfile::Hero, &camera_cfg);
        let boot = crate::profile::boot_profile(&config.mode);
        let boot_settings = crate::profile::base_settings(boot, &camera_cfg);
        apply_settings_to(&mut camera_cfg, boot_settings);

        // Resolved before the struct literal so the borrow of `config` ends
        // before it is moved in.
        let recording_params = config.recording.to_params();

        Self {
            config,
            camera_cfg,
            legs,
            leg_inbound: std::collections::HashMap::new(),
            leg_live: std::collections::HashMap::new(),
            // The air profile: LAN/direct WHEP readers, so no STUN and no TCP
            // ICE candidate. Recording rides `video.recording` straight into
            // mediamtx's native fMP4 recorder — a single-encode tee of the
            // stream already being served, so it costs zero extra encoder
            // cycles on an SBC that is already loaded.
            mediamtx: MediamtxManager::for_profile(
                crate::mediamtx::MediamtxProfile::Air,
                config_dir,
            )
            .with_recording(recording_params),
            encoder: None,
            secondary_encoders: Vec::new(),
            wfb_tee: None,
            encoder_emits_wfb_rtp: false,
            cloud_push: None,
            sei_tap: None,
            vision_tap: None,
            framebus_feeds: crate::framebus_input::Feeds::default(),
            wfb_tee_progress: ProgressTracker::new(),
            cloud_push_progress: ProgressTracker::new(),
            sei_tap_started_at: None,
            last_cameras: DiscoveryResult::empty(),
            encoder_type: None,
            encoder_label: None,
            state: PipelineState::Stopped,
            started_at: now,
            first_packet_seen: false,
            inbound_bytes_value: -1,
            inbound_bytes_changed_at: now,
            video_inbound_bytes_per_s: 0.0,
            restart_count: 0,
            cloud_restart_count: 0,
            wfb_tee_restart_count: 0,
            vision_tap_restart_count: 0,
            encoder_respawns: 0,
            last_start_error: StartError::None,
            no_first_packet_failures: 0,
            force_software: false,
            software_healthy_since: None,
            encoder_errors: Arc::new(AtomicU32::new(0)),
            rediscover_requested: false,
            last_fingerprint: None,
            last_discovery_at: None,
            restart_lock: Arc::new(Mutex::new(())),
            camera_plugged: Arc::new(Notify::new()),
            python_executable: discover::python_executable(),
            env,
            camera_state_path: None,
            video_streams_path: None,
            hero_base,
            encoder_control: EncoderControl::new(boot, boot_settings),
            video_profile_path: None,
        }
    }

    /// The shared encoder-control handle. `main` hands this to the encoder
    /// command socket server; it is the only way another process retargets the
    /// encoder.
    pub fn encoder_control(&self) -> Arc<EncoderControl> {
        Arc::clone(&self.encoder_control)
    }
}

/// The liveness a leg gets when its inbound byte counter has been flat past the
/// window. A `sourceOnDemand` leg (a network-pull secondary) is only pulled while
/// a viewer reads it, so "flat" means "no reader", NOT "dead" — report `None`
/// (unknown; the GCS keeps the leg selectable) rather than a false-degraded
/// `Some(false)`; a genuinely dead on-demand source surfaces as a
/// failed pull the moment a viewer connects. Primary + local (publisher) legs
/// always push, so a flat counter there IS a real degradation → `Some(false)`.
fn liveness_on_flat(is_network_pull: bool) -> Option<bool> {
    if is_network_pull {
        None
    } else {
        Some(false)
    }
}

/// Whether a leg's mediamtx `bytesReceived` counter shows new work since the
/// last sample. ANY change counts: a counter that went DOWN is a fresh
/// publisher on the path (the per-path counter restarts when the source
/// reconnects, e.g. after a secondary encoder respawn), which is progress, not a
/// stall. Only an unchanged counter is flat.
fn leg_counter_moved(previous: u64, current: u64) -> bool {
    current != previous
}

/// Write resolved attention settings into the live capture config the encoder
/// command builder reads. Only the four encode knobs move; source and codec are
/// the operator's and never touched by an attention switch.
pub(crate) fn apply_settings_to(cfg: &mut CameraConfig, s: EncoderSettings) {
    cfg.width = s.width;
    cfg.height = s.height;
    cfg.fps = s.fps;
    cfg.bitrate_kbps = s.bitrate_kbps;
}

/// How far a ceiling-only change has to move the bitrate, as a percentage of
/// what the encoder is currently running, before it is worth a respawn.
///
/// Below this the change is held: an encoder respawn costs a fresh SPS/PPS, a
/// decoder reset and a ~1-2 s visible gap, and the radio's own FEC ladder —
/// which retunes through the `wfb_tx` management socket at no video cost —
/// absorbs a sub-threshold overshoot. At or above it the clamp is real enough
/// that continuing to transmit the higher rate would put surplus bits into the
/// FEC block, where they do not become quality, they become queue depth and
/// then loss.
pub const CEILING_DEFER_PERCENT: u32 = 25;

/// Whether a desired-vs-live difference is a ceiling-only change small enough
/// to hold on the encoder (see [`CEILING_DEFER_PERCENT`]).
///
/// A profile switch is NEVER deferrable: a hero ⇄ thumbnail change moves
/// geometry and frame rate, is the operator's explicit request, and cannot be
/// expressed as a rate nudge. Only the bitrate may be held.
pub fn ceiling_change_is_deferrable(live: EncoderSettings, target: EncoderSettings) -> bool {
    if live.width != target.width || live.height != target.height || live.fps != target.fps {
        return false;
    }
    if target.bitrate_kbps == live.bitrate_kbps || live.bitrate_kbps == 0 {
        return false;
    }
    // Integer comparison of |delta| / live < percent / 100, cross-multiplied so
    // there is no rounding step to argue about at the boundary: a change of
    // exactly the threshold applies.
    live.bitrate_kbps.abs_diff(target.bitrate_kbps) * 100
        < live.bitrate_kbps * CEILING_DEFER_PERCENT
}

#[cfg(test)]
mod tests;
