//! Atlas capture ingest: turn the keyframe + capture-state events a drone
//! forwards to the compute node into a reconstruct job.
//!
//! A compute node receives framed `AtlasEvent`s off a bearer (LAN / WFB / cloud)
//! into its event router. This is the seam between that receive side and the job
//! queue: it counts the keyframes of a capture session and, on the terminal
//! `Bagged` state, inserts the dataset + submits a `Reconstruct` job the
//! scheduler picks up — the reconstruct half of the compute receiver service.
//!
//! A malformed capture-state frame off the wire is dropped + logged (never an
//! error that could tear down the ingest loop); `ingest` errors only on a real
//! store fault.

use ados_protocol::atlas::{
    AtlasEvent, CaptureState, CaptureStatus, ATLAS_CAPTURE_STATE_TOPIC, ATLAS_KEYFRAME_TOPIC,
};
use ados_protocol::compute::{ComputeJobKind, ComputeJobState};

use crate::store::{Dataset, JobRecord, JobStore};
use crate::ComputeError;

/// Accumulates a capture session's events and submits a reconstruct job when the
/// session is bagged.
#[derive(Debug, Default)]
pub struct AtlasIngest {
    /// Keyframe events seen this session (received-side delivery count).
    keyframes_seen: u64,
}

impl AtlasIngest {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keyframe events received so far (the received-side delivery proof — the
    /// drone's send is fire-and-forget, so only what the node decodes counts).
    pub fn keyframes_seen(&self) -> u64 {
        self.keyframes_seen
    }

    /// Process one received Atlas event against the job store. Returns the
    /// submitted reconstruct job id when a `Bagged` capture-state triggers it. A
    /// keyframe event bumps the received counter; a malformed capture-state frame
    /// is dropped; other topics are ignored.
    pub fn ingest(
        &mut self,
        event: &AtlasEvent,
        store: &JobStore,
        now_ms: i64,
    ) -> Result<Option<String>, ComputeError> {
        match event.topic.as_str() {
            ATLAS_KEYFRAME_TOPIC => {
                self.keyframes_seen += 1;
                Ok(None)
            }
            ATLAS_CAPTURE_STATE_TOPIC => match CaptureStatus::from_msgpack(&event.payload) {
                Ok(status) if status.state == CaptureState::Bagged => {
                    self.submit_reconstruct(&status, store, now_ms).map(Some)
                }
                Ok(_) => Ok(None),
                Err(e) => {
                    tracing::debug!(error = %e, "atlas_ingest_bad_capture_state");
                    Ok(None)
                }
            },
            _ => Ok(None),
        }
    }

    /// Insert the dataset + submit the reconstruct job for a bagged session.
    /// Idempotent: the dataset/job ids are deterministic per session, so a
    /// re-sent `Bagged` (plausible on the lossy fire-and-forget lane) finds them
    /// already present and is swallowed (a `Conflict` is success, not a store
    /// fault) rather than erroring the receive loop.
    fn submit_reconstruct(
        &self,
        status: &CaptureStatus,
        store: &JobStore,
        now_ms: i64,
    ) -> Result<String, ComputeError> {
        let dataset_id = format!("ds-{}", status.session_id);
        let job_id = format!("recon-{}", status.session_id);
        let dataset = Dataset {
            id: dataset_id.clone(),
            kind: "bag".into(),
            created_ms: now_ms,
            meta: serde_json::json!({
                "keyframes": status.keyframes,
                "cameras": status.camera_count,
                "received_keyframes": self.keyframes_seen,
            }),
        };
        if let Err(e) = store.insert_dataset(&dataset) {
            if !matches!(e, ComputeError::Conflict(_)) {
                return Err(e);
            }
        }
        let job = JobRecord {
            id: job_id.clone(),
            kind: ComputeJobKind::Reconstruct,
            dataset_id: Some(dataset_id),
            state: ComputeJobState::Queued,
            progress: 0.0,
            params: serde_json::json!({}),
            result_ref: None,
            error: None,
            created_ms: now_ms,
            updated_ms: now_ms,
        };
        if let Err(e) = store.submit_job(&job) {
            if !matches!(e, ComputeError::Conflict(_)) {
                return Err(e);
            }
            tracing::debug!(session = %status.session_id, "atlas_ingest_duplicate_bagged");
        }
        Ok(job_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::atlas::VioHealth;

    fn keyframe_event() -> AtlasEvent {
        AtlasEvent {
            topic: ATLAS_KEYFRAME_TOPIC.to_string(),
            payload: vec![0u8; 8],
        }
    }

    fn capture_state(state: CaptureState, keyframes: u64) -> AtlasEvent {
        let status = CaptureStatus {
            session_id: "sess1".into(),
            state,
            keyframes,
            vio_health: VioHealth::Good,
            camera_count: 1,
            ingest_rate_hz: 9.0,
        };
        AtlasEvent {
            topic: ATLAS_CAPTURE_STATE_TOPIC.to_string(),
            payload: status.to_msgpack().unwrap(),
        }
    }

    #[test]
    fn keyframes_then_bagged_submits_one_reconstruct_job() {
        let store = JobStore::open(":memory:").unwrap();
        let mut ingest = AtlasIngest::new();

        for _ in 0..3 {
            assert_eq!(ingest.ingest(&keyframe_event(), &store, 100).unwrap(), None);
        }
        // A non-terminal state does not submit.
        assert_eq!(
            ingest
                .ingest(&capture_state(CaptureState::Capturing, 3), &store, 100)
                .unwrap(),
            None
        );
        // Bagged submits exactly one reconstruct job referencing the dataset.
        let job_id = ingest
            .ingest(&capture_state(CaptureState::Bagged, 3), &store, 200)
            .unwrap()
            .expect("bagged submits a job");
        assert_eq!(job_id, "recon-sess1");
        assert_eq!(ingest.keyframes_seen(), 3);
        let ds = store.get_dataset("ds-sess1").unwrap().expect("dataset");
        assert_eq!(ds.meta["received_keyframes"], 3);
    }

    #[test]
    fn a_resent_bagged_is_idempotent_not_an_error() {
        let store = JobStore::open(":memory:").unwrap();
        let mut ingest = AtlasIngest::new();
        ingest.ingest(&keyframe_event(), &store, 100).unwrap();

        // First Bagged submits the dataset + job.
        let first = ingest
            .ingest(&capture_state(CaptureState::Bagged, 1), &store, 200)
            .unwrap();
        assert_eq!(first.as_deref(), Some("recon-sess1"));

        // A re-sent Bagged on the lossy fire-and-forget lane must NOT error the
        // receive loop: it finds the deterministic ids present and is swallowed,
        // returning the same job id.
        let second = ingest
            .ingest(&capture_state(CaptureState::Bagged, 1), &store, 300)
            .unwrap();
        assert_eq!(second.as_deref(), Some("recon-sess1"));
        // The PK on the deterministic ids guarantees a single dataset + job.
        assert!(store.get_dataset("ds-sess1").unwrap().is_some());
    }

    #[test]
    fn a_malformed_capture_state_is_dropped_not_an_error() {
        let store = JobStore::open(":memory:").unwrap();
        let mut ingest = AtlasIngest::new();
        let bad = AtlasEvent {
            topic: ATLAS_CAPTURE_STATE_TOPIC.to_string(),
            payload: b"not msgpack".to_vec(),
        };
        assert_eq!(ingest.ingest(&bad, &store, 100).unwrap(), None);
    }
}
