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

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::discover;
use crate::encoder::{
    binary_present, build_encoder_command, detect_encoder_for_camera, wrap_with_sei_inject,
    CameraInfo, CameraType, EncoderParams, SeiSplit,
};
use crate::health::{PipelineState, StartError};
use crate::mediamtx::MAIN_PATH;
use crate::orchestrator::VideoOrchestrator;
use crate::process::{kill_orphans, kill_publisher_orphans, ManagedProcess};
use crate::tap::{self, spawn_vision_tap, ENGINE_IDLE_STOP};
use crate::tap_lazy::{run_lazy_vision_tap, TapGeometry, TapLegState};
use crate::wfb_tee::{drain_wfb_tee_stderr, orphan_pattern, spawn_wfb_tee, ProgressTracker};

mod leaves;
mod secondary;
mod startup;

/// The primary encoder exactly as it will run: one command, or the encode and
/// publish stages of an SEI split with the in-process injector between them.
#[derive(Debug, Clone)]
pub(crate) enum PrimaryCommand {
    Single(Vec<String>),
    Sei(SeiSplit),
}

impl PrimaryCommand {
    /// Every token of the pipeline, for reading facts off it (the encoder
    /// label, whether it emits the radio RTP).
    pub(crate) fn tokens(&self) -> Vec<String> {
        match self {
            Self::Single(cmd) => cmd.clone(),
            Self::Sei(split) => split.render(),
        }
    }

    /// Spawn it as one process group.
    fn spawn(&self) -> std::io::Result<ManagedProcess> {
        match self {
            Self::Single(cmd) => {
                let (program, args) = cmd
                    .split_first()
                    .ok_or_else(|| std::io::Error::other("empty encoder command"))?;
                ManagedProcess::spawn("encoder", program, args)
            }
            Self::Sei(split) => {
                ManagedProcess::spawn_sei_pipeline("encoder", &split.encode, &split.publish)
            }
        }
    }
}

/// The headless SEI latency tap's argv (arguments only — the program is the
/// agent's Python interpreter).
///
/// Pure and separate from the spawn so the sweep-classification test can feed
/// the REAL command line in. `--rtsp` marks `pipe_uri` as this process's INPUT,
/// which is what keeps it out of the publisher orphan sweep.
pub fn sei_tap_args(pipe_uri: &str) -> Vec<String> {
    vec![
        "-m".into(),
        "ados.services.video.sei_tap".into(),
        "--once".into(),
        "--rtsp".into(),
        pipe_uri.to_string(),
    ]
}

/// The cloud-relay push's `ffmpeg` arguments: re-mux the local `main` stream to
/// the relay with no transcode.
///
/// `-progress pipe:2` forces the status report to stderr as flushed `key=value`
/// lines once per second. This is the liveness signal, not decoration: ffmpeg
/// suppresses the status line entirely when stderr is not a tty, and
/// `total_size=N` (bytes the muxer has written to the relay) is the only
/// OUTPUT-side proof the push is still moving. Without it the health check had
/// nothing but `is_running()`, and a cloud push wedged on a half-open relay
/// socket is alive and mute — the exact state process liveness cannot see.
///
/// Pure and separate from the spawn for the same reason as [`sei_tap_args`].
pub fn cloud_push_args(local_rtsp: &str, push_url: &str) -> Vec<String> {
    vec![
        "-rtsp_transport".into(),
        "tcp".into(),
        "-timeout".into(),
        "5000000".into(),
        "-i".into(),
        local_rtsp.to_string(),
        "-c".into(),
        "copy".into(),
        "-f".into(),
        "rtsp".into(),
        "-rtsp_transport".into(),
        "tcp".into(),
        "-progress".into(),
        "pipe:2".into(),
        push_url.to_string(),
    ]
}

#[cfg(test)]
mod tests;
