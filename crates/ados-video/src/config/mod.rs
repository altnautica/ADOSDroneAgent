//! Video camera configuration, read from the `video.camera:` block of
//! `/etc/ados/config.yaml`. Field names and defaults mirror the Python
//! `CameraConfig` model (`core/config/video.py`). Only the fields the encoder
//! command builder reads are modelled here.

mod agent;
mod camera;
mod legs;
mod roster;
mod sections;

pub use agent::AgentVideoConfig;
pub use camera::{is_network_url, CameraConfig, CameraProfile, MAX_KEYFRAME_INTERVAL};
pub use legs::{CameraLeg, CameraMatch, ResolvedLeg};
pub use roster::RosterVideoConfig;
pub use sections::{RecordingConfig, VisionTapConfig, WfbVideoConfig};

#[cfg(test)]
mod tests;
