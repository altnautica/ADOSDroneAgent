//! The live (in-flight) reconstruction session.
//!
//! The post-flight pipeline ([`crate::pipeline`]) trains a deliverable from a
//! finished bag. The live session is the other path: keyframes arrive over the
//! relay as the drone flies, the node trains a splat incrementally (about 50
//! steps per new keyframe), and it pushes SPZ deltas to the GCS Live World. This
//! module owns the session state machine and the delta production seam; the
//! incremental trainer is a backend behind [`DeltaProducer`], mocked in CI.

use serde::{Deserialize, Serialize};

use ados_protocol::atlas::SplatDescriptor;

/// Steps the incremental trainer advances per ingested keyframe.
const STEPS_PER_KEYFRAME: u64 = 50;
/// Gaussians the mock trainer adds per ingested keyframe.
const GAUSSIANS_PER_KEYFRAME: u64 = 1200;

/// The live-session lifecycle. `pairing` while the drone is connecting, `ready`
/// once the worker is allocated, `active` while training on incoming keyframes,
/// `paused` when the operator (or a link drop) halts ingest, `ended` terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LiveSessionState {
    Pairing,
    Ready,
    Active,
    Paused,
    Ended,
}

/// A compressed splat delta the live trainer emits per training-step batch. The
/// `bytes` are an SPZ delta frame in production; the mock produces a small
/// deterministic payload so the stream lane and Live World are exercised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplatDelta {
    pub bytes: Vec<u8>,
    pub gaussian_count: u64,
    pub step: u64,
}

/// Produces a splat delta for the current trainer state. The real producer
/// compresses the trainer's new/changed gaussians to an SPZ frame; the mock
/// keeps the session testable with no GPU.
pub trait DeltaProducer: Send + Sync {
    fn produce(&self, gaussian_count: u64, step: u64) -> SplatDelta;
}

/// A no-GPU delta producer: a deterministic synthetic SPZ frame whose header
/// carries the gaussian count + step, so the stream lane and Live World render
/// real, changing values with no trainer.
#[derive(Debug, Default, Clone, Copy)]
pub struct MockDeltaProducer;

impl DeltaProducer for MockDeltaProducer {
    fn produce(&self, gaussian_count: u64, step: u64) -> SplatDelta {
        // A tiny deterministic "SPZ-ish" payload: a 4-byte magic + the counts.
        let mut bytes = Vec::with_capacity(20);
        bytes.extend_from_slice(b"SPZ0");
        bytes.extend_from_slice(&gaussian_count.to_le_bytes());
        bytes.extend_from_slice(&step.to_le_bytes());
        SplatDelta {
            bytes,
            gaussian_count,
            step,
        }
    }
}

/// One live reconstruction session on the compute node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveSession {
    pub id: String,
    pub state: LiveSessionState,
    pub keyframes_ingested: u64,
    pub gaussian_count: u64,
    pub step: u64,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl LiveSession {
    /// A new session in `pairing` (the drone is connecting; no worker yet).
    pub fn new(id: impl Into<String>, now_ms: i64) -> Self {
        Self {
            id: id.into(),
            state: LiveSessionState::Pairing,
            keyframes_ingested: 0,
            gaussian_count: 0,
            step: 0,
            created_ms: now_ms,
            updated_ms: now_ms,
        }
    }

    /// Whether `from -> to` is a legal transition. The session walks
    /// pairing -> ready -> active, toggles active <-> paused, and any non-terminal
    /// state can end. It never rewinds (e.g. ended -> active is illegal).
    pub fn can_transition(from: LiveSessionState, to: LiveSessionState) -> bool {
        use LiveSessionState::*;
        matches!(
            (from, to),
            (Pairing, Ready)
                | (Pairing, Ended)
                | (Ready, Active)
                | (Ready, Ended)
                | (Active, Paused)
                | (Active, Ended)
                | (Paused, Active)
                | (Paused, Ended)
        )
    }

    /// Apply a state transition. Returns `true` when applied, `false` when the
    /// transition is illegal (the state is unchanged). Stamps `updated_ms` on a
    /// successful transition.
    pub fn try_transition(&mut self, to: LiveSessionState, now_ms: i64) -> bool {
        if !Self::can_transition(self.state, to) {
            return false;
        }
        self.state = to;
        self.updated_ms = now_ms;
        true
    }

    /// Ingest one keyframe: advance the incremental trainer and emit a splat
    /// delta + the current splat descriptor. Only an `active` session ingests;
    /// otherwise the keyframe is dropped and `None` is returned (a paused or
    /// ended session does not train).
    pub fn ingest_keyframe(
        &mut self,
        producer: &dyn DeltaProducer,
        url: Option<String>,
        now_ms: i64,
    ) -> Option<(SplatDescriptor, SplatDelta)> {
        if self.state != LiveSessionState::Active {
            return None;
        }
        self.keyframes_ingested += 1;
        self.step += STEPS_PER_KEYFRAME;
        self.gaussian_count += GAUSSIANS_PER_KEYFRAME;
        self.updated_ms = now_ms;

        let delta = producer.produce(self.gaussian_count, self.step);
        let descriptor = SplatDescriptor {
            gaussian_count: self.gaussian_count,
            step: self.step,
            url,
            handle: Some(self.id.clone()),
        };
        Some((descriptor, delta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_session_starts_pairing() {
        let s = LiveSession::new("ls-1", 100);
        assert_eq!(s.state, LiveSessionState::Pairing);
        assert_eq!(s.keyframes_ingested, 0);
        assert_eq!(s.gaussian_count, 0);
    }

    #[test]
    fn legal_transitions_walk_pairing_to_active_and_end() {
        let mut s = LiveSession::new("ls-1", 0);
        assert!(s.try_transition(LiveSessionState::Ready, 1));
        assert!(s.try_transition(LiveSessionState::Active, 2));
        assert!(s.try_transition(LiveSessionState::Paused, 3));
        assert!(s.try_transition(LiveSessionState::Active, 4));
        assert!(s.try_transition(LiveSessionState::Ended, 5));
        assert_eq!(s.state, LiveSessionState::Ended);
        assert_eq!(s.updated_ms, 5);
    }

    #[test]
    fn illegal_transitions_are_rejected_and_do_not_rewind() {
        let mut s = LiveSession::new("ls-1", 0);
        // pairing -> active is illegal (must go through ready)
        assert!(!s.try_transition(LiveSessionState::Active, 1));
        assert_eq!(s.state, LiveSessionState::Pairing);
        // walk to ended, then ended -> active is illegal (never rewinds)
        s.try_transition(LiveSessionState::Ready, 2);
        s.try_transition(LiveSessionState::Active, 3);
        s.try_transition(LiveSessionState::Ended, 4);
        assert!(!s.try_transition(LiveSessionState::Active, 5));
        assert_eq!(s.state, LiveSessionState::Ended);
    }

    #[test]
    fn only_an_active_session_ingests_keyframes() {
        let mut s = LiveSession::new("ls-1", 0);
        // pairing: dropped
        assert!(s.ingest_keyframe(&MockDeltaProducer, None, 1).is_none());
        assert_eq!(s.keyframes_ingested, 0);

        s.try_transition(LiveSessionState::Ready, 2);
        s.try_transition(LiveSessionState::Active, 3);

        let (desc, delta) = s
            .ingest_keyframe(&MockDeltaProducer, Some("spz://ls-1".into()), 4)
            .unwrap();
        assert_eq!(s.keyframes_ingested, 1);
        assert_eq!(s.step, STEPS_PER_KEYFRAME);
        assert_eq!(s.gaussian_count, GAUSSIANS_PER_KEYFRAME);
        assert_eq!(desc.gaussian_count, GAUSSIANS_PER_KEYFRAME);
        assert_eq!(desc.step, STEPS_PER_KEYFRAME);
        assert_eq!(desc.url.as_deref(), Some("spz://ls-1"));
        assert_eq!(desc.handle.as_deref(), Some("ls-1"));
        assert_eq!(delta.gaussian_count, GAUSSIANS_PER_KEYFRAME);
        assert_eq!(&delta.bytes[0..4], b"SPZ0");

        // A second keyframe grows the trainer monotonically.
        let (desc2, _) = s.ingest_keyframe(&MockDeltaProducer, None, 5).unwrap();
        assert_eq!(desc2.step, 2 * STEPS_PER_KEYFRAME);
        assert_eq!(desc2.gaussian_count, 2 * GAUSSIANS_PER_KEYFRAME);

        // Paused: ingest stops.
        s.try_transition(LiveSessionState::Paused, 6);
        assert!(s.ingest_keyframe(&MockDeltaProducer, None, 7).is_none());
        assert_eq!(s.keyframes_ingested, 2);
    }
}
