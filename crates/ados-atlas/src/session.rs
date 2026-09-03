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
    KeyframeFlags, KeyframeImage, KeyframeTier, Pose, PoseDescriptor, PoseSource, TimeAlignment,
    VioHealth,
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
    /// Row-major 6x6 covariance of `pose`. Empty when the producer could state
    /// none, which makes the keyframe fail
    /// [`KeyframeEnvelope::validate`](ados_protocol::atlas::KeyframeEnvelope::validate)
    /// downstream rather than silently reconstructing without a prior sigma.
    pub pose_cov: Vec<f64>,
    pub pose_source: PoseSource,
    /// How well this frame and its pose are time-paired.
    pub time: TimeAlignment,
    pub global_anchor: Option<GlobalAnchor>,
    pub imu_window: Vec<ImuSample>,
}

/// The pose side of one frame, with no image and no intrinsics. The
/// overwhelming majority of frames are not keyframes, and building a whole
/// [`FrameInput`] with an empty [`KeyframeImage`] and a resolved intrinsics
/// matrix purely to reach the pose stream was a per-frame allocation for
/// nothing.
#[derive(Debug, Clone)]
pub struct PoseInput {
    pub pose: Pose,
    pub global_anchor: Option<GlobalAnchor>,
}

/// The result of ingesting one frame: a pose descriptor for the live pose
/// stream when the pose cadence is due, plus a keyframe when the camera's
/// selector fired.
#[derive(Debug, Clone)]
pub struct CaptureOutput {
    /// `None` when the ~10 Hz pose cadence is not due yet. The frame was still
    /// accepted and counted; only the publish is decimated.
    pub pose: Option<PoseDescriptor>,
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
    // Honesty state surfaced on `status()`.
    anchored: bool,
    pose_tier: PoseSource,
    dropped_keyframes: u64,
    // Pose-publish decimation. ONE clock for the whole rig, not one per camera:
    // the rig has a single pose, so an N-camera rig must not publish N times the
    // documented rate onto the same bus that carries multi-MB keyframes.
    pose_interval_ms: i64,
    last_pose_pub_ms: Option<i64>,
}

impl CaptureSession {
    /// Build an idle session for the given config. No keyframes flow until
    /// [`start`](Self::start) moves it to capturing.
    pub fn new(config: CaptureConfig) -> Self {
        Self::with_pose_interval(config, 100)
    }

    /// Build an idle session that publishes at most one pose descriptor per
    /// `pose_interval_ms` across the whole rig.
    pub fn with_pose_interval(config: CaptureConfig, pose_interval_ms: i64) -> Self {
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
            anchored: false,
            pose_tier: PoseSource::LocalVio,
            dropped_keyframes: 0,
            pose_interval_ms: pose_interval_ms.max(0),
            last_pose_pub_ms: None,
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
        self.dropped_keyframes = 0;
        self.last_pose_pub_ms = None;
        // The anchor is per-session: a new session must not inherit the previous
        // run's latch, or its first keyframes would be selected before this
        // run's anchor exists.
        self.anchored = false;
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

    /// Record whether the session's geo anchor is latched. Keyframe selection is
    /// REFUSED until it is.
    ///
    /// Before the first 3D fix a pose translation is `[0, 0, alt_rel]` at the
    /// local origin; once the anchor latches, translations become ENU metres from
    /// it. A session that captured across that transition hands the
    /// reconstructor a set of keyframes at the origin and another set hundreds of
    /// metres away, under one session id — which it reads as a teleport, not as
    /// two frames of reference. Refusing to select until the anchor exists costs
    /// the first few seconds of a capture and removes the failure entirely.
    pub fn set_anchored(&mut self, anchored: bool) {
        self.anchored = anchored;
    }

    /// Record which producer filled the pose being tagged, surfaced on the
    /// capture status so a silent switch to offloaded SLAM is visible.
    pub fn set_pose_tier(&mut self, tier: PoseSource) {
        self.pose_tier = tier;
    }

    /// Record that a keyframe the capture path produced was not delivered (the
    /// bus pruned a slow subscriber). Surfaced on the capture status: without it
    /// the keyframe count claims frames the reconstruction never received.
    pub fn note_keyframe_dropped(&mut self, dropped: u64) {
        self.dropped_keyframes = self.dropped_keyframes.saturating_add(dropped);
    }

    /// Whether the session-wide keyframe cap has stopped selection.
    pub fn is_capped(&self) -> bool {
        self.at_keyframe_cap()
    }

    /// The current capture state.
    pub fn state(&self) -> CaptureState {
        self.state
    }

    /// Count of enabled cameras (1 to N). The fusion layer keys off this.
    pub fn enabled_camera_count(&self) -> u32 {
        self.config.enabled_camera_count()
    }

    /// Whether the session has reached its configured session-wide keyframe cap.
    /// A cap of `0` means unlimited. This is a SESSION-level gate: the per-camera
    /// selector is count-blind and would keep re-crossing its motion/time
    /// thresholds on a repeating flight path (an orbit revisits the same viewpoints
    /// every lap), so an uncapped session on a loop accumulates keyframes without
    /// bound. Both [`would_select`](Self::would_select) and
    /// [`on_frame`](Self::on_frame) consult this before the per-camera decision so
    /// the peek and the commit can never disagree.
    fn at_keyframe_cap(&self) -> bool {
        let cap = self.config.selection.max_keyframes;
        cap > 0 && self.kf_count >= cap
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
            capped: self.at_keyframe_cap(),
            anchored: self.anchored,
            pose_tier: self.pose_tier,
            dropped_keyframes: self.dropped_keyframes,
        }
    }

    /// Non-mutating peek: whether the next [`on_frame`](Self::on_frame) for this
    /// camera at this pose and time WOULD produce a keyframe. Mirrors
    /// `on_frame`'s gates (capturing + anchored + camera enabled + the per-camera
    /// selector), so the capture service can skip the expensive keyframe image
    /// encode when the frame would only contribute to the pose stream. Records
    /// nothing.
    pub fn would_select(&self, camera_id: &str, pose: &Pose, ts_ms: i64) -> bool {
        if self.state != CaptureState::Capturing {
            return false;
        }
        // No world frame yet: see `set_anchored`.
        if !self.anchored {
            return false;
        }
        // Session-wide keyframe cap reached: no camera selects again this session.
        if self.at_keyframe_cap() {
            return false;
        }
        let enabled = self
            .config
            .cameras
            .iter()
            .any(|c| c.id == camera_id && c.enabled);
        if !enabled {
            return false;
        }
        match self.selectors.get(camera_id) {
            Some(sel) => sel.peek_select(pose, ts_ms, &self.config.selection),
            // A camera with no selector yet: its first frame always selects.
            None => true,
        }
    }

    /// Ingest the POSE side of one frame — no image, no intrinsics, no keyframe.
    /// The path for the overwhelming majority of frames, which only feed the
    /// live pose stream.
    ///
    /// Returns `None` when the session is not capturing, the camera is not
    /// enabled, or the rig-wide pose cadence is not due yet.
    pub fn on_pose_only(
        &mut self,
        camera_id: &str,
        input: PoseInput,
        ts_ms: i64,
    ) -> Option<PoseDescriptor> {
        if !self.accept_frame(camera_id, ts_ms) {
            return None;
        }
        self.due_pose(input.pose, input.global_anchor, ts_ms)
    }

    /// Whether this frame is accepted for ingest, recording it in the rate
    /// measurement when it is. Shared by both ingest paths so they cannot
    /// disagree on what "accepted" means.
    fn accept_frame(&mut self, camera_id: &str, ts_ms: i64) -> bool {
        if self.state != CaptureState::Capturing {
            return false;
        }
        let enabled = self
            .config
            .cameras
            .iter()
            .any(|c| c.id == camera_id && c.enabled);
        if !enabled {
            return false;
        }
        // Record the ingest for the rate measurement (every accepted frame, not
        // only keyframes — the rate is the camera feed rate).
        self.frame_count += 1;
        self.first_frame_ms.get_or_insert(ts_ms);
        self.last_frame_ms = Some(ts_ms);
        true
    }

    /// A pose descriptor when the rig-wide cadence is due, else `None`.
    ///
    /// One clock for the whole rig: the rig has a single pose, so an N-camera
    /// set must not publish N descriptors per interval. A non-monotonic `ts_ms`
    /// (a source that re-stamps or replays) is treated as due rather than
    /// wedging the cadence forever.
    fn due_pose(
        &mut self,
        pose: Pose,
        anchor: Option<GlobalAnchor>,
        ts_ms: i64,
    ) -> Option<PoseDescriptor> {
        let due = match self.last_pose_pub_ms {
            None => true,
            Some(last) => ts_ms < last || ts_ms - last >= self.pose_interval_ms,
        };
        if !due {
            return None;
        }
        self.last_pose_pub_ms = Some(ts_ms);
        Some(PoseDescriptor {
            pose,
            anchor,
            ts_ms,
        })
    }

    /// Ingest one frame for `camera_id`, image included. Returns `None` when the
    /// session is not capturing or the camera is not enabled. Otherwise it
    /// produces a pose descriptor when the rig-wide pose cadence is due and,
    /// when the camera's selector fires, a full keyframe (with
    /// `is_session_start` set on the first keyframe of the session).
    pub fn on_frame(
        &mut self,
        camera_id: &str,
        frame: FrameInput,
        ts_ms: i64,
    ) -> Option<CaptureOutput> {
        // Capture the role before the mutable accept, so an unknown or disabled
        // camera yields nothing and never touches the rate counters.
        let role = match self.config.cameras.iter().find(|c| c.id == camera_id) {
            Some(c) if c.enabled => c.role,
            _ => return None,
        };
        if !self.accept_frame(camera_id, ts_ms) {
            return None;
        }

        // Per-camera keyframe selection, gated by the anchor latch and the
        // session-wide cap first so it mirrors `would_select`. Once at the cap we
        // short-circuit BEFORE touching the per-camera selector: the pose stream
        // and Capturing state continue (the live-capture model is preserved),
        // only keyframe selection stops.
        let selected = if !self.anchored || self.at_keyframe_cap() {
            false
        } else {
            self.selectors
                .entry(camera_id.to_string())
                .or_default()
                .should_select(&frame.pose, ts_ms, &self.config.selection)
        };

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
                pose: frame.pose.clone(),
                pose_cov: frame.pose_cov,
                pose_source: frame.pose_source,
                time: frame.time,
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

        let pose = self.due_pose(frame.pose, frame.global_anchor, ts_ms);
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
            id: id.to_string(),
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

    fn anchor() -> GlobalAnchor {
        GlobalAnchor {
            lat: 12.97,
            lon: 77.59,
            alt_m: 920.0,
            yaw_rad: 0.0,
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
                calibrated: true,
            },
            pose: Pose {
                r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                t,
                cov: None,
            },
            pose_cov: vec![0.25; 36],
            pose_source: PoseSource::LocalVio,
            time: TimeAlignment::unmeasured(),
            global_anchor: Some(anchor()),
            imu_window: vec![ImuSample {
                t_ms: 1,
                gyro: [0.0, 0.0, 0.0],
                accel: [0.0, 0.0, 9.81],
            }],
        }
    }

    fn pose_at(t: [f64; 3]) -> PoseInput {
        PoseInput {
            pose: Pose {
                r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                t,
                cov: None,
            },
            global_anchor: Some(anchor()),
        }
    }

    /// A started, ANCHORED session: the state every capture reaches once the
    /// first 3D fix lands, and the precondition for keyframe selection.
    fn started(config: CaptureConfig, id: &str) -> CaptureSession {
        let mut s = CaptureSession::new(config);
        s.start(id.into());
        s.set_anchored(true);
        s
    }

    #[test]
    fn single_camera_flow_produces_keyframes() {
        let mut s = started(
            config(vec![cam("front", CameraRole::Primary, true)]),
            "sess-a",
        );

        // First frame → first keyframe.
        let out = s.on_frame("front", frame_at([0.0, 0.0, 0.0]), 0).unwrap();
        let kf = out.keyframe.expect("first frame is a keyframe");
        assert_eq!(kf.kf_id, 0);
        assert_eq!(kf.session_id, "sess-a");
        assert_eq!(kf.camera_role, CameraRole::Primary);
        assert_eq!(kf.tier, KeyframeTier::Full);
        assert!(kf.flags.is_session_start);
        // The reconstruction inputs the envelope now carries ride through.
        assert_eq!(kf.pose_cov.len(), 36);
        assert_eq!(kf.time, TimeAlignment::unmeasured());

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
        let mut s = started(
            config(vec![
                cam("front", CameraRole::Primary, true),
                cam("down", CameraRole::Down, true),
                cam("back", CameraRole::Back, false),
            ]),
            "sess-b",
        );
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
    fn would_select_mirrors_on_frame_keyframe_decision() {
        let mut s = CaptureSession::new(config(vec![
            cam("front", CameraRole::Primary, true),
            cam("down", CameraRole::Down, false),
        ]));
        // Idle: nothing selects.
        assert!(!s.would_select("front", &frame_at([0.0, 0.0, 0.0]).pose, 0));
        s.start("sess-w".into());
        s.set_anchored(true);
        // A disabled / unknown camera never selects.
        assert!(!s.would_select("down", &frame_at([0.0, 0.0, 0.0]).pose, 0));
        assert!(!s.would_select("nope", &frame_at([0.0, 0.0, 0.0]).pose, 0));
        // First frame: peek says select; committing via on_frame agrees.
        assert!(s.would_select("front", &frame_at([0.0, 0.0, 0.0]).pose, 0));
        assert!(s
            .on_frame("front", frame_at([0.0, 0.0, 0.0]), 0)
            .unwrap()
            .keyframe
            .is_some());
        // A tiny move: peek says no, and on_frame produces no keyframe.
        assert!(!s.would_select("front", &frame_at([0.05, 0.0, 0.0]).pose, 100));
        assert!(s
            .on_frame("front", frame_at([0.05, 0.0, 0.0]), 100)
            .unwrap()
            .keyframe
            .is_none());
        // A baseline move: peek says yes, and on_frame produces a keyframe.
        assert!(s.would_select("front", &frame_at([0.7, 0.0, 0.0]).pose, 200));
        assert!(s
            .on_frame("front", frame_at([0.7, 0.0, 0.0]), 200)
            .unwrap()
            .keyframe
            .is_some());
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
        let mut s = started(
            config(vec![
                cam("front", CameraRole::Primary, true),
                cam("down", CameraRole::Down, false),
            ]),
            "sess-c",
        );
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
        let mut s = started(
            config(vec![cam("front", CameraRole::Primary, true)]),
            "sess-d",
        );
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
        s.set_anchored(true);
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

    #[test]
    fn session_wide_keyframe_cap_stops_selecting_past_the_cap() {
        // A capped session stops laying down keyframes once the cap is reached,
        // even as more frames arrive that would each cross the motion threshold —
        // the fix for a looping flight (an orbit revisits the same viewpoints each
        // lap) that would otherwise re-cross the per-camera thresholds forever and
        // bloat the dataset without bound.
        let capped = CaptureConfig {
            cameras: vec![cam("front", CameraRole::Primary, true)],
            profile: CaptureProfile::Orbit,
            selection: SelectionParams {
                max_keyframes: 3,
                ..SelectionParams::default()
            },
        };
        let mut s = started(capped, "sess-cap");
        assert!(!s.status().capped, "not capped before any keyframe");

        // Ten frames, each 1.0 m past the last (well over the 0.5 m threshold) so
        // the per-camera selector would otherwise fire on every one.
        for i in 0..10 {
            s.on_frame("front", frame_at([i as f64, 0.0, 0.0]), i * 100);
        }
        // Exactly the cap, never more, no matter how many frames arrive.
        assert_eq!(s.status().keyframes, 3);

        // The pose stream keeps flowing past the cap (the live-capture model is
        // preserved): an eleventh frame still yields an output, just no keyframe.
        let out = s
            .on_frame("front", frame_at([11.0, 0.0, 0.0]), 1100)
            .unwrap();
        assert!(out.keyframe.is_none());
        // The peek agrees with the commit: at the cap it never selects.
        assert!(!s.would_select("front", &frame_at([12.0, 0.0, 0.0]).pose, 1200));
        assert_eq!(s.status().keyframes, 3);
    }

    #[test]
    fn the_cap_is_reported_so_a_frozen_count_is_not_read_as_live_progress() {
        // The regression: at the cap, state stays `Capturing` and `keyframes`
        // stops moving, so nothing in the status changed and the transition was
        // never republished. The operator surface then showed "capturing" against
        // a frozen count forever, indistinguishable from a stalled camera.
        let capped = CaptureConfig {
            cameras: vec![cam("front", CameraRole::Primary, true)],
            profile: CaptureProfile::Orbit,
            selection: SelectionParams {
                max_keyframes: 2,
                ..SelectionParams::default()
            },
        };
        let mut s = started(capped, "sess-capflag");
        s.on_frame("front", frame_at([0.0, 0.0, 0.0]), 0);
        assert!(!s.status().capped);
        s.on_frame("front", frame_at([1.0, 0.0, 0.0]), 100);
        assert!(s.status().capped, "the cap is reported the moment it bites");
        assert!(s.is_capped());
    }

    #[test]
    fn zero_keyframe_cap_is_unlimited() {
        // The default cap of 0 preserves today's behaviour: no session-wide bound,
        // so every moving frame past the first still lays down a keyframe.
        let uncapped = CaptureConfig {
            cameras: vec![cam("front", CameraRole::Primary, true)],
            profile: CaptureProfile::Orbit,
            selection: SelectionParams::default(), // max_keyframes == 0
        };
        assert_eq!(uncapped.selection.max_keyframes, 0);
        let mut s = started(uncapped, "sess-unlimited");
        for i in 0..50 {
            s.on_frame("front", frame_at([i as f64, 0.0, 0.0]), i * 100);
        }
        assert_eq!(s.status().keyframes, 50);
        assert!(!s.status().capped);
    }

    #[test]
    fn no_keyframe_is_selected_before_the_anchor_latches() {
        // The regression: before the first 3D fix a pose translation is
        // [0, 0, alt_rel] at the local origin; after it, translations are ENU
        // metres from the anchor. A session that captured across that transition
        // handed the reconstructor keyframes at the origin AND keyframes hundreds
        // of metres away under one session id, which reads as a teleport.
        let mut s = CaptureSession::new(config(vec![cam("front", CameraRole::Primary, true)]));
        s.start("sess-anchor".into());
        assert!(!s.status().anchored);

        // Frames arrive; the pose stream flows, but nothing is selected.
        for i in 0..5 {
            let out = s
                .on_frame("front", frame_at([i as f64, 0.0, 0.0]), i * 100)
                .expect("the frame is still accepted and counted");
            assert!(
                out.keyframe.is_none(),
                "no keyframe may be selected without a world frame"
            );
        }
        assert_eq!(s.status().keyframes, 0);
        assert!(!s.would_select("front", &frame_at([9.0, 0.0, 0.0]).pose, 900));

        // The anchor latches; selection starts immediately.
        s.set_anchored(true);
        assert!(s.status().anchored);
        let out = s.on_frame("front", frame_at([9.0, 0.0, 0.0]), 900).unwrap();
        assert!(out.keyframe.is_some(), "selection starts once anchored");
    }

    #[test]
    fn a_new_session_does_not_inherit_the_previous_anchor_latch() {
        let mut s = started(config(vec![cam("front", CameraRole::Primary, true)]), "one");
        assert!(s.status().anchored);
        s.start("two".into());
        let inherited = s.status().anchored;
        assert!(
            !inherited,
            "the anchor is per-session; inheriting it would select before this run has a world frame"
        );
    }

    #[test]
    fn the_pose_stream_is_decimated_to_one_publish_per_interval_for_the_whole_rig() {
        // The regression: a pose descriptor was produced per accepted frame per
        // camera, so a 3-camera rig at 30 fps published 90 poses/sec onto the same
        // 16-deep bus that carries multi-MB keyframes — against a documented
        // ~10 Hz contract, and raising keyframe eviction pressure threefold.
        //
        // The invariant that matters is that the publish count is bounded by the
        // TIME span and the interval, and is INDEPENDENT of how many cameras the
        // rig has. The exact count on a 33 ms frame grid is not 10: a publish can
        // only land on a frame boundary, so the effective period is 132 ms.
        fn publishes(cameras: &[&str], frames: i64, frame_ms: i64) -> usize {
            let cfg = config(
                cameras
                    .iter()
                    .map(|id| cam(id, CameraRole::Primary, true))
                    .collect(),
            );
            let mut s = CaptureSession::with_pose_interval(cfg, 100);
            s.start("sess-decimate".into());
            s.set_anchored(true);
            let mut n = 0usize;
            for i in 0..frames {
                let ts = i * frame_ms;
                for camera in cameras {
                    if s.on_pose_only(camera, pose_at([0.0, 0.0, 0.0]), ts)
                        .is_some()
                    {
                        n += 1;
                    }
                }
            }
            n
        }

        // ~1 s at 30 fps.
        let one_cam = publishes(&["front"], 30, 33);
        let three_cam = publishes(&["front", "down", "back"], 30, 33);
        assert_eq!(
            one_cam, three_cam,
            "the rig has ONE pose, so the publish rate must not scale with camera count"
        );
        // Bounded by the span and the interval, nowhere near the 90 frames fed in.
        let span_ms = 29 * 33;
        let ceiling = (span_ms / 100) as usize + 1;
        assert!(
            three_cam <= ceiling,
            "expected at most {ceiling} rig-wide pose publishes over {span_ms} ms, got {three_cam}"
        );
        assert!(
            three_cam >= 7,
            "the cadence must still run at roughly the documented rate, got {three_cam}"
        );
    }

    #[test]
    fn a_pose_only_frame_is_still_counted_in_the_ingest_rate() {
        // The pose-only path must not become a second definition of "accepted":
        // the ingest rate is the camera feed rate, so a non-keyframe frame counts.
        let mut s = started(
            config(vec![cam("front", CameraRole::Primary, true)]),
            "sess-rate",
        );
        for i in 0..11 {
            s.on_pose_only("front", pose_at([0.0, 0.0, 0.0]), i * 100);
        }
        let st = s.status();
        assert_eq!(st.keyframes, 0, "pose-only never selects a keyframe");
        assert!(
            (st.ingest_rate_hz - 10.0).abs() < 0.5,
            "rate {}",
            st.ingest_rate_hz
        );
        // A disabled/unknown camera is refused on this path too.
        assert!(s
            .on_pose_only("nope", pose_at([0.0, 0.0, 0.0]), 2000)
            .is_none());
    }

    #[test]
    fn undelivered_keyframes_are_reported_on_the_status() {
        let mut s = started(
            config(vec![cam("front", CameraRole::Primary, true)]),
            "sess-drop",
        );
        assert_eq!(s.status().dropped_keyframes, 0);
        s.note_keyframe_dropped(2);
        s.note_keyframe_dropped(1);
        assert_eq!(
            s.status().dropped_keyframes,
            3,
            "a keyframe count that claims frames nothing received is a false reading"
        );
    }

    #[test]
    fn the_active_pose_tier_is_reported() {
        let mut s = started(
            config(vec![cam("front", CameraRole::Primary, true)]),
            "sess-tier",
        );
        assert_eq!(s.status().pose_tier, PoseSource::LocalVio);
        s.set_pose_tier(PoseSource::OffloadedSlam);
        assert_eq!(
            s.status().pose_tier,
            PoseSource::OffloadedSlam,
            "a silent switch to offloaded SLAM must be visible on the status"
        );
    }
}
