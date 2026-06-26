//! Atlas world-model capture — the light, on-drone half of the world-model
//! program.
//!
//! This crate is the camera side of the split: the drone selects pose-tagged
//! keyframes from its camera stream(s) and emits the keyframe + pose contract a
//! separate compute node reconstructs a 3D world model from (gaussian splat /
//! point cloud / mesh / occupancy). It does no reconstruction and no training —
//! that is the compute node's job.
//!
//! Three pieces:
//!
//! - [`CaptureConfig`] declares the rig: one camera up to an all-sides set, the
//!   flight [`CaptureProfile`], and the keyframe [`SelectionParams`]. Camera
//!   count is configurable from 1 to N and drives ONE flow at any count.
//! - [`KeyframeSelector`] is the per-camera gate that decides, from pose deltas
//!   and elapsed time, when a frame is worth a keyframe.
//! - [`CaptureSession`] is the state machine: it takes pose-tagged
//!   [`FrameInput`]s, always feeds the ~10 Hz pose stream, and produces a
//!   [`KeyframeEnvelope`] whenever a camera's selector fires.
//!
//! The wire types ([`KeyframeEnvelope`], [`PoseDescriptor`], [`CaptureStatus`],
//! …) are the shared `ados_protocol::atlas` contract; this crate consumes them,
//! it does not redefine them. They are re-exported here for one import surface.

mod config;
mod selector;
mod session;

pub use config::{CameraConfig, CaptureConfig, CaptureProfile, SelectionParams};
pub use selector::{rotation_angle, KeyframeSelector};
pub use session::{CaptureOutput, CaptureSession, FrameInput};

// Re-export the shared wire contract so callers get one import surface.
pub use ados_protocol::atlas::{
    CameraIntrinsics, CameraRole, CaptureState, CaptureStatus, Distortion, GlobalAnchor,
    ImageEncoding, ImuSample, KeyframeEnvelope, KeyframeFlags, KeyframeImage, KeyframeTier, Pose,
    PoseDescriptor, PoseSource, VioHealth, ATLAS_CAPTURE_STATE_TOPIC, ATLAS_POSE_OFFLOAD_TOPIC,
    PLUGIN_ATLAS_POSE_TOPIC,
};

/// Errors from the Atlas capture core. The frame-by-frame capture path is
/// infallible by design (a frame either selects a keyframe or it does not);
/// the only fallible surface is validating a capture configuration before a
/// session runs.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AtlasError {
    /// A capture config has no enabled camera, so it could never produce a
    /// keyframe.
    #[error("capture config has no enabled cameras")]
    NoEnabledCameras,
    /// Two cameras share an id, which would collapse their per-camera selectors
    /// onto one entry.
    #[error("duplicate camera id `{0}` in capture config")]
    DuplicateCameraId(String),
}
