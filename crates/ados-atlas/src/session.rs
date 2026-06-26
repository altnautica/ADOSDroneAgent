//! Capture session: the stateful core that turns a stream of pose-tagged
//! frames into the pose stream plus selected keyframes a compute node
//! reconstructs from.
//!
//! One session runs one flow regardless of camera count. Each enabled camera
//! gets its own [`KeyframeSelector`]; a frame always contributes to the ~10 Hz
//! pose stream and, when its camera's selector fires, a full keyframe. The
//! fusion keys off the enabled count downstream; the same code path serves one
//! camera or an all-sides rig.

use crate::config::CaptureConfig;
use crate::selector::KeyframeSelector;
use ados_protocol::atlas::{
    CameraIntrinsics, CaptureState, CaptureStatus, GlobalAnchor, ImuSample, KeyframeEnvelope,
    KeyframeFlags, KeyframeImage, KeyframeTier, Pose, PoseDescriptor, PoseSource, VioHealth,
};
use std::collections::HashMap;

/// One camera frame handed to the session, already pose-tagged. The pose may
/// have come from on-board VIO or an offloaded SLAM return (`pose_source`); the
/// session does not care which producer filled it.
#[derive(Debug, Clone)]
pub struct FrameInput {
    pub image: KeyframeImage,
    pub camera: CameraIntrinsics,
    pub pose: Pose,
    pub pose_source: PoseSource,
    pub global_anchor: Option<GlobalAnchor>,
    pub imu_window: Vec<ImuSample>,
}

/// The result of ingesting one frame: always a pose descriptor for the live
/// pose stream, plus a keyframe when the camera's selector fired.
#[derive(Debug, Clone)]
pub struct CaptureOutput {
    pub pose: PoseDescriptor,
    pub keyframe: Option<KeyframeEnvelope>,
}

/// The capture state machine for one session.
#[derive(Debug)]
pub struct CaptureSession {
    config: CaptureConfig,
    selectors: HashMap<String, KeyframeSelector>,
    state: CaptureState,
    session_id: String,
    kf_count: u64,
    vio_health: VioHealth,
    // Ingest-rate measurement over the frames actually accepted (enabled camera
    // + capturing). The rate is derived from real frame timestamps, not assumed.
    frame_count: u64,
    first_frame_ms: Option<i64>,
    last_frame_ms: Option<i64>,
}

impl CaptureSession {
    /// Build an idle session for the given config. No keyframes flow until
    /// [`start`](Self::start) moves it to capturing.
    pub fn new(config: CaptureConfig) -> Self {
        Self {
            config,
            selectors: HashMap::new(),
            state: CaptureState::Idle,
            session_id: String::new(),
            kf_count: 0,
            vio_health: VioHealth::Good,
            frame_count: 0,
            first_frame_ms: None,
            last_frame_ms: None,
        }
    }

    /// Begin a capture session. Resets keyframe/ingest counters and the
    /// per-camera selectors so a new session never inherits the previous run's
    /// last-keyframe state, and moves to [`CaptureState::Capturing`].
    pub fn start(&mut self, session_id: String) {
        self.session_id = session_id;
        self.state = CaptureState::Capturing;
        self.kf_count = 0;
        self.frame_count = 0;
        self.first_frame_ms = None;
        self.last_frame_ms = None;
        self.selectors.clear();
        tracing::info!(session_id = %self.session_id, cameras = self.enabled_camera_count(), "atlas capture started");
    }

    /// Pause capture (drops out of [`CaptureState::Capturing`]); a no-op from
    /// any other state. Selectors are retained so resuming continues the same
    /// keyframe cadence.
    pub fn pause(&mut self) {
        if self.state == CaptureState::Capturing {
            self.state = CaptureState::Paused;
            tracing::info!(session_id = %self.session_id, "atlas capture paused");
        }
    }

    /// Resume a paused session; a no-op from any other state.
    pub fn resume(&mut self) {
        if self.state == CaptureState::Paused {
            self.state = CaptureState::Capturing;
            tracing::info!(session_id = %self.session_id, "atlas capture resumed");
        }
    }

    /// Begin finalizing the session (no more frames are accepted). Valid from
    /// capturing or paused; a no-op otherwise. Follow with
    /// [`mark_bagged`](Self::mark_bagged) once the bag is written.
    pub fn finalize(&mut self) {
        if matches!(self.state, CaptureState::Capturing | CaptureState::Paused) {
            self.state = CaptureState::Finalizing;
            tracing::info!(session_id = %self.session_id, keyframes = self.kf_count, "atlas capture finalizing");
        }
    }

    /// Mark the session's bag fully written. Valid only from
    /// [`CaptureState::Finalizing`]; a no-op otherwise.
    pub fn mark_bagged(&mut self) {
        if self.state == CaptureState::Finalizing {
            self.state = CaptureState::Bagged;
            tracing::info!(session_id = %self.session_id, "atlas capture bagged");
        }
    }

    /// Update the current VIO/SLAM health, surfaced on the capture status.
    pub fn set_vio_health(&mut self, health: VioHealth) {
        self.vio_health = health;
    }

    /// The current capture state.
    pub fn state(&self) -> CaptureState {
        self.state
    }

    /// Count of enabled cameras (1 to N). The fusion layer keys off this.
    pub fn enabled_camera_count(&self) -> u32 {
        self.config.enabled_camera_count()
    }

    /// A snapshot of the capture status for the state topic.
    pub fn status(&self) -> CaptureStatus {
        CaptureStatus {
            session_id: self.session_id.clone(),
            state: self.state,
            keyframes: self.kf_count,
            vio_health: self.vio_health,
            camera_count: self.config.enabled_camera_count(),
            ingest_rate_hz: self.ingest_rate_hz(),
        }
    }

    /// Ingest one frame for `camera_id`. Returns `None` when the session is not
    /// capturing or the camera is not enabled. Otherwise it always produces a
    /// pose descriptor for the live pose stream and, when the camera's selector
    /// fires, a full keyframe (with `is_session_start` set on the first keyframe
    /// of the session).
    pub fn on_frame(
        &mut self,
        camera_id: &str,
        frame: FrameInput,
        ts_ms: i64,
    ) -> Option<CaptureOutput> {
        if self.state != CaptureState::Capturing {
            return None;
        }
        // Gate on the camera being present AND enabled, capturing its role in a
        // single pass; an unknown or disabled camera yields nothing.
        let role = match self.config.cameras.iter().find(|c| c.id == camera_id) {
            Some(c) if c.enabled => c.role,
            _ => return None,
        };

        // Record the ingest for the rate measurement (every accepted frame, not
        // only keyframes — the rate is the camera feed rate).
        self.frame_count += 1;
        self.first_frame_ms.get_or_insert(ts_ms);
        self.last_frame_ms = Some(ts_ms);

        // Every accepted frame contributes to the ~10 Hz pose stream.
        let pose = PoseDescriptor {
            pose: frame.pose.clone(),
            anchor: frame.global_anchor,
            ts_ms,
        };

        // Per-camera keyframe selection.
        let selected = self
            .selectors
            .entry(camera_id.to_string())
            .or_default()
            .should_select(&frame.pose, ts_ms, &self.config.selection);

        let keyframe = if selected {
            let is_first = self.kf_count == 0;
            let kf = KeyframeEnvelope {
                session_id: self.session_id.clone(),
                kf_id: self.kf_count,
                ts_unix_ms: ts_ms,
                camera_id: camera_id.to_string(),
                camera_role: role,
                tier: KeyframeTier::Full,
                image: frame.image,
                camera: frame.camera,
                pose: frame.pose,
                pose_source: frame.pose_source,
                global_anchor: frame.global_anchor,
                imu_window: frame.imu_window,
                flags: KeyframeFlags {
                    is_session_start: is_first,
                    ..KeyframeFlags::default()
                },
            };
            self.kf_count += 1;
            Some(kf)
        } else {
            None
        };

        Some(CaptureOutput { pose, keyframe })
    }

    /// Measured ingest rate (Hz) over the accepted frames, derived from the span
    /// between the first and last accepted frame timestamps. Zero until at least
    /// two frames over a positive span have been seen.
    fn ingest_rate_hz(&self) -> f32 {
        match (self.first_frame_ms, self.last_frame_ms) {
            (Some(first), Some(last)) if self.frame_count >= 2 && last > first => {
                let span_s = (last - first) as f64 / 1000.0;
                ((self.frame_count - 1) as f64 / span_s) as f32
            }
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CameraConfig, CaptureProfile, SelectionParams};
    use ados_protocol::atlas::{CameraRole, Distortion, ImageEncoding};

    fn cam(id: &str, role: CameraRole, enabled: bool) -> CameraConfig {
        CameraConfig {
            id: id.into(),
            role,
            enabled,
            reconstruct: enabled,
        }
    }

    fn config(cameras: Vec<CameraConfig>) -> CaptureConfig {
        CaptureConfig {
            cameras,
            profile: CaptureProfile::Freeform,
            selection: SelectionParams::default(),
        }
    }

    fn frame_at(t: [f64; 3]) -> FrameInput {
        FrameInput {
            image: KeyframeImage {
                encoding: ImageEncoding::Jpeg,
                width: 1280,
                height: 720,
                bytes: vec![0xAB; 8],
            },
            camera: CameraIntrinsics {
                k: [900.0, 0.0, 640.0, 0.0, 900.0, 360.0, 0.0, 0.0, 1.0],
                distortion: Distortion {
                    model: "radtan".into(),
                    params: vec![0.0, 0.0, 0.0, 0.0],
                },
            },
            pose: Pose {
                r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                t,
                cov: None,
            },
            pose_source: PoseSource::LocalVio,
            global_anchor: None,
            imu_window: vec![ImuSample {
                t_ms: 1,
                gyro: [0.0, 0.0, 0.0],
                accel: [0.0, 0.0, 9.81],
            }],
        }
    }

    #[test]
    fn single_camera_flow_produces_keyframes() {
        let mut s = CaptureSession::new(config(vec![cam("front", CameraRole::Primary, true)]));
        s.start("sess-a".into());

        // First frame → first keyframe.
        let out = s.on_frame("front", frame_at([0.0, 0.0, 0.0]), 0).unwrap();
        let kf = out.keyframe.expect("first frame is a keyframe");
        assert_eq!(kf.kf_id, 0);
        assert_eq!(kf.session_id, "sess-a");
        assert_eq!(kf.camera_role, CameraRole::Primary);
        assert_eq!(kf.tier, KeyframeTier::Full);
        assert!(kf.flags.is_session_start);

        // A tiny move under threshold → pose only, no keyframe.
        let out = s
            .on_frame("front", frame_at([0.05, 0.0, 0.0]), 100)
            .unwrap();
        assert!(out.keyframe.is_none());

        // A move past the translation threshold → second keyframe.
        let out = s.on_frame("front", frame_at([0.7, 0.0, 0.0]), 200).unwrap();
        let kf = out.keyframe.expect("baseline move is a keyframe");
        assert_eq!(kf.kf_id, 1);
        assert!(!kf.flags.is_session_start);
    }

    #[test]
    fn multi_camera_flow_runs_per_camera_selection() {
        let mut s = CaptureSession::new(config(vec![
            cam("front", CameraRole::Primary, true),
            cam("down", CameraRole::Down, true),
            cam("back", CameraRole::Back, false),
        ]));
        s.start("sess-b".into());
        assert_eq!(s.enabled_camera_count(), 2);

        // Each enabled camera selects its own first keyframe independently.
        let f = s.on_frame("front", frame_at([0.0, 0.0, 0.0]), 0).unwrap();
        assert_eq!(f.keyframe.unwrap().camera_role, CameraRole::Primary);
        let d = s.on_frame("down", frame_at([0.0, 0.0, 0.0]), 0).unwrap();
        let dkf = d
            .keyframe
            .expect("down camera selects its own first keyframe");
        assert_eq!(dkf.camera_role, CameraRole::Down);
        assert_eq!(dkf.camera_id, "down");
        // is_session_start marks the SESSION's first keyframe, not each camera's
        // first: the down camera's keyframe is the session's second, so false.
        assert!(!dkf.flags.is_session_start);

        // A disabled camera yields nothing.
        assert!(s.on_frame("back", frame_at([0.0, 0.0, 0.0]), 0).is_none());
        // An unknown camera yields nothing.
        assert!(s.on_frame("nope", frame_at([0.0, 0.0, 0.0]), 0).is_none());

        // Two keyframes (one per enabled camera), and is_session_start only on
        // the very first keyframe of the session.
        assert_eq!(s.status().keyframes, 2);
    }

    #[test]
    fn on_frame_before_start_returns_none() {
        let mut s = CaptureSession::new(config(vec![cam("front", CameraRole::Primary, true)]));
        // State is Idle; no frames accepted.
        assert!(s.on_frame("front", frame_at([0.0, 0.0, 0.0]), 0).is_none());
        assert_eq!(s.state(), CaptureState::Idle);
    }

    #[test]
    fn status_reflects_count_cameras_and_state() {
        let mut s = CaptureSession::new(config(vec![
            cam("front", CameraRole::Primary, true),
            cam("down", CameraRole::Down, false),
        ]));
        assert_eq!(s.status().state, CaptureState::Idle);

        s.start("sess-c".into());
        s.on_frame("front", frame_at([0.0, 0.0, 0.0]), 0);
        s.on_frame("front", frame_at([1.0, 0.0, 0.0]), 100);

        let st = s.status();
        assert_eq!(st.state, CaptureState::Capturing);
        assert_eq!(st.keyframes, 2);
        assert_eq!(st.camera_count, 1);
        assert_eq!(st.session_id, "sess-c");
        // Two frames 100 ms apart → ~10 Hz ingest.
        assert!(
            (st.ingest_rate_hz - 10.0).abs() < 0.5,
            "rate {}",
            st.ingest_rate_hz
        );
    }

    #[test]
    fn first_keyframe_marks_session_start_only_once() {
        let mut s = CaptureSession::new(config(vec![cam("front", CameraRole::Primary, true)]));
        s.start("sess-d".into());
        let first = s
            .on_frame("front", frame_at([0.0, 0.0, 0.0]), 0)
            .unwrap()
            .keyframe
            .unwrap();
        assert!(first.flags.is_session_start);
        let second = s
            .on_frame("front", frame_at([1.0, 0.0, 0.0]), 100)
            .unwrap()
            .keyframe
            .unwrap();
        assert!(!second.flags.is_session_start);
    }

    #[test]
    fn lifecycle_transitions_are_guarded() {
        let mut s = CaptureSession::new(config(vec![cam("front", CameraRole::Primary, true)]));
        // finalize/pause are no-ops while idle.
        s.finalize();
        assert_eq!(s.state(), CaptureState::Idle);

        s.start("sess-e".into());
        s.pause();
        assert_eq!(s.state(), CaptureState::Paused);
        // No frames accepted while paused.
        assert!(s.on_frame("front", frame_at([0.0, 0.0, 0.0]), 0).is_none());
        s.resume();
        assert_eq!(s.state(), CaptureState::Capturing);

        s.finalize();
        assert_eq!(s.state(), CaptureState::Finalizing);
        // No frames accepted while finalizing.
        assert!(s.on_frame("front", frame_at([0.0, 0.0, 0.0]), 0).is_none());
        s.mark_bagged();
        assert_eq!(s.state(), CaptureState::Bagged);
    }
}
