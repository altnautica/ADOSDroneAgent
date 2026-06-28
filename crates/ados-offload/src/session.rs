//! The streaming offload session.
//!
//! A behaviour opens a session in one of three modes and streams frames to the
//! compute node; the node streams detections and/or drift-corrected poses back.
//! The session does not move frames itself (the transport is injected by the
//! consumer) — it owns the SAFETY: it tracks each return stream's freshness and
//! runs the lock-state gate so a behaviour commands only on fresh, locked
//! results and stops the moment any required stream goes stale or the link
//! drops. The fast control loop and the guided-setpoint math stay on the drone;
//! this gates the slow remote layer that corrects it.

use serde::{Deserialize, Serialize};

use crate::freshness::{FreshnessGate, GateState, LockGate, LockState};

/// What the node returns for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OffloadMode {
    /// Detections + tracking (a target / TargetRef). Consumers: follow, orbit,
    /// inspection / SAR detect.
    VisionOnly,
    /// Drift-corrected poses + occupancy. Consumers: navigation, avoidance,
    /// the live world model.
    SlamOnly,
    /// Both, from one frame stream — an NPU-less drone on a full autonomous
    /// mission. Any required stream going stale trips the lock.
    Full,
}

/// A snapshot of a session's safety state for one cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStatus {
    /// The lock state after this cycle.
    pub lock: LockState,
    /// The detection/target stream's freshness.
    pub target: GateState,
    /// The pose stream's freshness.
    pub pose: GateState,
    /// Whether the behaviour may command the FC this cycle (locked AND every
    /// stream the mode requires is fresh). When false, the behaviour stops/holds.
    pub commanding: bool,
}

/// A streaming perception/SLAM offload session with per-stream freshness gates
/// and a lock-state safety gate.
#[derive(Debug, Clone)]
pub struct OffloadSession {
    mode: OffloadMode,
    target_gate: FreshnessGate,
    pose_gate: FreshnessGate,
    lock: LockGate,
}

impl OffloadSession {
    /// A session in `mode` with the target-age and pose-age freshness budgets.
    pub fn new(mode: OffloadMode, target_budget_ms: i64, pose_budget_ms: i64) -> Self {
        Self {
            mode,
            target_gate: FreshnessGate::new(target_budget_ms),
            pose_gate: FreshnessGate::new(pose_budget_ms),
            lock: LockGate::new(),
        }
    }

    pub fn mode(&self) -> OffloadMode {
        self.mode
    }

    /// A detection/target result arrived from the node. `now_ms` is the local
    /// monotonic time of arrival (the same clock [`tick`](Self::tick) reads), not
    /// the result's own timestamp — see [`FreshnessGate`].
    pub fn on_detection(&mut self, now_ms: i64) {
        self.target_gate.record(now_ms);
    }

    /// A drift-corrected pose arrived from the node on `atlas.pose.offload`.
    /// `now_ms` is the local monotonic time of arrival.
    pub fn on_pose(&mut self, now_ms: i64) {
        self.pose_gate.record(now_ms);
    }

    /// The link state changed (a drop trips every stream to lost).
    pub fn set_link(&mut self, up: bool) {
        self.target_gate.set_link(up);
        self.pose_gate.set_link(up);
    }

    /// Acquire (or re-acquire) the lock — an operator designate / click-to-track.
    /// The only way out of a lost lock.
    pub fn designate(&mut self) {
        self.lock.lock();
    }

    /// Release the lock (back to idle).
    pub fn drop_lock(&mut self) {
        self.lock.unlock();
    }

    pub fn lock_state(&self) -> LockState {
        self.lock.state()
    }

    /// Whether every return stream the mode requires is fresh at `now_ms`.
    fn streams_usable(&self, now_ms: i64) -> bool {
        match self.mode {
            OffloadMode::VisionOnly => self.target_gate.is_usable(now_ms),
            OffloadMode::SlamOnly => self.pose_gate.is_usable(now_ms),
            OffloadMode::Full => {
                self.target_gate.is_usable(now_ms) && self.pose_gate.is_usable(now_ms)
            }
        }
    }

    /// Advance the session one cycle. Updates the lock from the stream freshness
    /// (a locked behaviour whose required stream went stale drops to lost and
    /// must be re-designated) and returns the safety snapshot the behaviour acts
    /// on. `commanding` is true only when locked AND fresh.
    pub fn tick(&mut self, now_ms: i64) -> SessionStatus {
        let usable = self.streams_usable(now_ms);
        self.lock.update(usable);
        SessionStatus {
            lock: self.lock.state(),
            target: self.target_gate.state(now_ms),
            pose: self.pose_gate.state(now_ms),
            // Command only when locked AND the required streams are fresh this
            // cycle. A still-acquiring lock (locked, no fresh reading yet) holds.
            commanding: self.lock.is_locked() && usable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_session_commands_when_locked_and_both_streams_fresh() {
        let mut s = OffloadSession::new(OffloadMode::Full, 500, 500);
        s.designate();
        s.on_detection(1000);
        s.on_pose(1000);
        let status = s.tick(1100);
        assert_eq!(status.lock, LockState::Locked);
        assert!(status.commanding);
    }

    #[test]
    fn a_full_session_stops_when_one_required_stream_goes_stale() {
        let mut s = OffloadSession::new(OffloadMode::Full, 500, 500);
        s.designate();
        s.on_detection(1000);
        s.on_pose(1000);
        assert!(s.tick(1100).commanding);
        // The pose stream stops updating; it goes stale past the budget.
        s.on_detection(1500); // detections keep coming...
        let status = s.tick(1700); // ...but the pose is now 700ms old (> 500)
        assert_eq!(status.pose, GateState::Stale);
        assert_eq!(status.lock, LockState::Lost);
        assert!(!status.commanding);
    }

    #[test]
    fn a_link_drop_stops_the_session() {
        let mut s = OffloadSession::new(OffloadMode::VisionOnly, 500, 500);
        s.designate();
        s.on_detection(1000);
        assert!(s.tick(1100).commanding);
        s.set_link(false);
        let status = s.tick(1150);
        assert_eq!(status.lock, LockState::Lost);
        assert!(!status.commanding);
    }

    #[test]
    fn a_session_never_auto_re_acquires_after_a_stale_drop() {
        let mut s = OffloadSession::new(OffloadMode::VisionOnly, 500, 500);
        s.designate();
        s.on_detection(1000);
        assert!(s.tick(1100).commanding);
        // Go stale -> lost.
        assert_eq!(s.tick(2000).lock, LockState::Lost);
        // Fresh detections resume, but the lock must NOT come back by itself.
        s.on_detection(2100);
        s.on_detection(2200);
        let status = s.tick(2250);
        assert_eq!(status.target, GateState::Fresh);
        assert_eq!(status.lock, LockState::Lost); // still lost
        assert!(!status.commanding);
        // Only a re-designate re-acquires.
        s.designate();
        s.on_detection(2300);
        assert!(s.tick(2350).commanding);
    }

    #[test]
    fn slam_only_gates_on_the_pose_stream() {
        let mut s = OffloadSession::new(OffloadMode::SlamOnly, 500, 500);
        s.designate();
        // A detection without a pose does not make a SLAM-only session command.
        s.on_detection(1000);
        assert!(!s.tick(1100).commanding);
        s.on_pose(1100);
        assert!(s.tick(1200).commanding);
    }

    #[test]
    fn a_session_never_auto_re_acquires_after_a_link_drop() {
        // The link path closed the same way as the stale path: a recovered link
        // + fresh results must NOT re-acquire a dropped lock on its own.
        let mut s = OffloadSession::new(OffloadMode::VisionOnly, 500, 500);
        s.designate();
        s.on_detection(1000);
        assert!(s.tick(1100).commanding);
        s.set_link(false);
        assert_eq!(s.tick(1150).lock, LockState::Lost);
        // Link recovers, fresh detections resume — but the lock stays Lost.
        s.set_link(true);
        s.on_detection(1200);
        let status = s.tick(1250);
        assert_eq!(status.target, GateState::Fresh);
        assert_eq!(status.lock, LockState::Lost);
        assert!(!status.commanding);
    }

    #[test]
    fn a_full_session_holds_while_only_one_stream_is_fresh() {
        // Full needs BOTH streams; with only the target fresh it stays acquiring
        // (locked, holding) and never commands — it does not trip to Lost (it has
        // not yet tracked) and it does not command on half the data.
        let mut s = OffloadSession::new(OffloadMode::Full, 500, 500);
        s.designate();
        s.on_detection(1000); // target only, no pose yet
        let status = s.tick(1100);
        assert_eq!(status.lock, LockState::Locked); // still acquiring
        assert!(!status.commanding);
    }

    #[test]
    fn an_un_designated_session_never_commands() {
        let mut s = OffloadSession::new(OffloadMode::Full, 500, 500);
        s.on_detection(1000);
        s.on_pose(1000);
        // Fresh streams, but no designate -> idle, not commanding.
        let status = s.tick(1100);
        assert_eq!(status.lock, LockState::Unlocked);
        assert!(!status.commanding);
    }
}
