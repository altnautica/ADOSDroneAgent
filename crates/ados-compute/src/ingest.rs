//! Atlas capture ingest: turn the keyframe + capture-state events a drone
//! forwards to the compute node into a reconstructor-ingestible dataset and a
//! reconstruct job.
//!
//! A compute node receives framed `AtlasEvent`s off a bearer (LAN / WFB / cloud)
//! into its event router. This is the seam between that receive side and the job
//! queue. Two things happen here:
//!
//! - A keyframe event carries the image bytes plus the pose and intrinsics; the
//!   [`KeyframePersister`] streams the image to disk and records its frame. This
//!   touches no store state, so it runs WITHOUT the engine lock.
//! - The terminal `Bagged` capture-state finalizes the on-disk dataset (writes
//!   `transforms.json`) and produces the dataset + `Reconstruct` job for the
//!   caller to submit under its store lock. The dataset carries `input_path` so a
//!   backend trains on the real images, not the empty default.
//!
//! A malformed frame is dropped + logged (never an error that could tear down the
//! receive loop); only a real filesystem fault propagates from [`AtlasIngest::step`].

use ados_protocol::atlas::{
    AtlasEvent, CaptureState, CaptureStatus, KeyframeEnvelope, ATLAS_CAPTURE_STATE_TOPIC,
    ATLAS_KEYFRAME_TOPIC,
};
use ados_protocol::compute::{ComputeJobKind, ComputeJobState};

use crate::keyframe_persister::{dataset_id_for, KeyframePersister};
use crate::store::{Dataset, JobRecord, JobStore};
use crate::ComputeError;

/// The reconstruct backend a captured session defaults to. The keyframes carry
/// VIO / FC poses, so the gaussian-splat trainer trains directly on the written
/// `transforms.json` with no structure-from-motion pre-pass. The hint resolves to
/// the real tool when it is installed, else the mock backend (CI / no-GPU).
const DEFAULT_RECONSTRUCT_BACKEND: &str = "brush";

/// Accumulates a capture session's events: persists each keyframe's image and, on
/// the bagged terminal state, finalizes the dataset and yields the reconstruct job.
#[derive(Debug)]
pub struct AtlasIngest {
    /// Keyframe events seen this session (received-side delivery count).
    keyframes_seen: u64,
    persister: KeyframePersister,
}

impl AtlasIngest {
    /// An ingest that persists datasets under `work_root` (the same root the
    /// reconstructor reads `input_path` from and the artifact server serves).
    pub fn new(work_root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            keyframes_seen: 0,
            persister: KeyframePersister::new(work_root),
        }
    }

    /// Keyframe events received so far (the received-side delivery proof — the
    /// drone's send is fire-and-forget, so only what the node decodes counts).
    pub fn keyframes_seen(&self) -> u64 {
        self.keyframes_seen
    }

    /// Process one received Atlas event WITHOUT touching the store. A keyframe is
    /// persisted to disk (and the received counter bumped); a `Bagged`
    /// capture-state finalizes the dataset and returns the dataset + reconstruct
    /// job for the caller to submit under its store lock (see
    /// [`submit_reconstruct_job`]). Other topics, non-terminal states, and
    /// malformed payloads return `None`. Only a filesystem fault on the manifest
    /// write propagates.
    pub fn step(
        &mut self,
        event: &AtlasEvent,
        now_ms: i64,
    ) -> std::io::Result<Option<(Dataset, JobRecord)>> {
        match event.topic.as_str() {
            ATLAS_KEYFRAME_TOPIC => {
                self.keyframes_seen += 1;
                match KeyframeEnvelope::from_msgpack(&event.payload) {
                    Ok(kf) => {
                        if let Err(e) = self.persister.persist(&kf) {
                            tracing::warn!(error = %e, "atlas_keyframe_persist_failed");
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "atlas_keyframe_decode_failed"),
                }
                Ok(None)
            }
            ATLAS_CAPTURE_STATE_TOPIC => match CaptureStatus::from_msgpack(&event.payload) {
                Ok(status) if status.state == CaptureState::Bagged => {
                    self.bag(&status, now_ms).map(Some)
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

    /// Finalize a bagged session: write `transforms.json` and build the dataset +
    /// reconstruct job. The dataset carries `input_path` when at least one
    /// keyframe was persisted, so the backend trains on the real images; an empty
    /// session still submits a job (it fails honestly at the backend rather than
    /// silently vanishing).
    fn bag(
        &mut self,
        status: &CaptureStatus,
        now_ms: i64,
    ) -> std::io::Result<(Dataset, JobRecord)> {
        let dataset_id = dataset_id_for(&status.session_id);
        let input_path = self.persister.finalize(&dataset_id)?;

        let mut meta = serde_json::json!({
            "keyframes": status.keyframes,
            "cameras": status.camera_count,
            "received_keyframes": self.keyframes_seen,
        });
        if let Some(path) = &input_path {
            meta["input_path"] = serde_json::Value::String(path.to_string_lossy().into_owned());
        }

        let dataset = Dataset {
            id: dataset_id.clone(),
            kind: "bag".into(),
            created_ms: now_ms,
            meta,
        };
        let job = JobRecord {
            id: format!("recon-{}", status.session_id),
            kind: ComputeJobKind::Reconstruct,
            dataset_id: Some(dataset_id),
            state: ComputeJobState::Queued,
            progress: 0.0,
            params: serde_json::json!({ "backend": DEFAULT_RECONSTRUCT_BACKEND }),
            result_ref: None,
            error: None,
            created_ms: now_ms,
            updated_ms: now_ms,
        };
        Ok((dataset, job))
    }
}

/// Submit a bagged session's dataset + reconstruct job to the store, idempotently.
/// The dataset and job ids are deterministic per session, so a re-sent `Bagged`
/// (plausible on the lossy fire-and-forget lane) finds them already present and is
/// swallowed (a `Conflict` is success, not a store fault) rather than erroring the
/// receive loop. Run under the engine lock; the store write is brief. Returns the
/// reconstruct job id.
pub fn submit_reconstruct_job(
    store: &JobStore,
    dataset: &Dataset,
    job: &JobRecord,
) -> Result<String, ComputeError> {
    if let Err(e) = store.insert_dataset(dataset) {
        if !matches!(e, ComputeError::Conflict(_)) {
            return Err(e);
        }
    }
    if let Err(e) = store.submit_job(job) {
        if !matches!(e, ComputeError::Conflict(_)) {
            return Err(e);
        }
        tracing::debug!(job = %job.id, "atlas_ingest_duplicate_bagged");
    }
    Ok(job.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::atlas::{
        CameraIntrinsics, CameraRole, Distortion, ImageEncoding, KeyframeFlags, KeyframeImage,
        KeyframeTier, Pose, PoseSource, VioHealth,
    };

    fn real_keyframe_event(session: &str, kf_id: u64) -> AtlasEvent {
        let kf = KeyframeEnvelope {
            session_id: session.into(),
            kf_id,
            ts_unix_ms: 1_700_000_000_000 + kf_id as i64,
            camera_id: "front".into(),
            camera_role: CameraRole::Primary,
            tier: KeyframeTier::Full,
            image: KeyframeImage {
                encoding: ImageEncoding::Jpeg,
                width: 1280,
                height: 720,
                bytes: vec![0xFF, 0xD8, 0xFF, kf_id as u8],
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
                t: [0.0, 0.0, 0.0],
                cov: None,
            },
            pose_source: PoseSource::LocalVio,
            global_anchor: None,
            imu_window: Vec::new(),
            flags: KeyframeFlags::default(),
        };
        AtlasEvent {
            topic: ATLAS_KEYFRAME_TOPIC.to_string(),
            payload: kf.to_msgpack().unwrap(),
        }
    }

    fn capture_state(session: &str, state: CaptureState, keyframes: u64) -> AtlasEvent {
        let status = CaptureStatus {
            session_id: session.into(),
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
    fn keyframes_persist_then_bagged_yields_a_job_with_input_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(":memory:").unwrap();
        let mut ingest = AtlasIngest::new(dir.path());

        for kf_id in 0..3 {
            assert_eq!(
                ingest
                    .step(&real_keyframe_event("sess1", kf_id), 100)
                    .unwrap(),
                None
            );
        }
        // A non-terminal state does not bag.
        assert_eq!(
            ingest
                .step(&capture_state("sess1", CaptureState::Capturing, 3), 100)
                .unwrap(),
            None
        );

        // Bagged yields the dataset + job; the caller submits it.
        let (dataset, job) = ingest
            .step(&capture_state("sess1", CaptureState::Bagged, 3), 200)
            .unwrap()
            .expect("bagged yields a dataset + job");
        assert_eq!(job.id, "recon-sess1");
        assert_eq!(job.params["backend"], "brush");
        assert_eq!(ingest.keyframes_seen(), 3);

        // The dataset carries input_path pointing at the written dataset dir, and
        // that dir holds the manifest + the three images.
        let input_path = dataset.meta["input_path"].as_str().unwrap();
        assert_eq!(input_path, dir.path().join("ds-sess1").to_string_lossy());
        assert!(std::path::Path::new(input_path)
            .join("transforms.json")
            .exists());
        assert!(std::path::Path::new(input_path)
            .join("images/0.jpg")
            .exists());
        assert_eq!(dataset.meta["received_keyframes"], 3);

        // Submitting it lands the dataset + job in the store.
        let job_id = submit_reconstruct_job(&store, &dataset, &job).unwrap();
        assert_eq!(job_id, "recon-sess1");
        assert!(store.get_dataset("ds-sess1").unwrap().is_some());
        assert!(store.get_job("recon-sess1").unwrap().is_some());
    }

    #[test]
    fn a_resent_bagged_submit_is_idempotent_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(":memory:").unwrap();
        let mut ingest = AtlasIngest::new(dir.path());
        ingest.step(&real_keyframe_event("s2", 0), 100).unwrap();

        let (d1, j1) = ingest
            .step(&capture_state("s2", CaptureState::Bagged, 1), 200)
            .unwrap()
            .unwrap();
        assert_eq!(
            submit_reconstruct_job(&store, &d1, &j1).unwrap(),
            "recon-s2"
        );

        // A re-sent Bagged on the lossy lane: finalize is idempotent (same dir),
        // and the store submit swallows the Conflict, returning the same job id.
        let (d2, j2) = ingest
            .step(&capture_state("s2", CaptureState::Bagged, 1), 300)
            .unwrap()
            .unwrap();
        assert_eq!(
            submit_reconstruct_job(&store, &d2, &j2).unwrap(),
            "recon-s2"
        );
        assert!(store.get_dataset("ds-s2").unwrap().is_some());
    }

    #[test]
    fn a_malformed_capture_state_is_dropped_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::new(dir.path());
        let bad = AtlasEvent {
            topic: ATLAS_CAPTURE_STATE_TOPIC.to_string(),
            payload: b"not msgpack".to_vec(),
        };
        assert!(ingest.step(&bad, 100).unwrap().is_none());
    }

    #[test]
    fn a_malformed_keyframe_is_counted_but_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::new(dir.path());
        // An undecodable keyframe payload still counts as a received delivery, but
        // nothing is persisted, so a bag with only bad keyframes has no input_path.
        let bad = AtlasEvent {
            topic: ATLAS_KEYFRAME_TOPIC.to_string(),
            payload: vec![0u8; 8],
        };
        assert!(ingest.step(&bad, 100).unwrap().is_none());
        assert_eq!(ingest.keyframes_seen(), 1);

        let (dataset, job) = ingest
            .step(&capture_state("badkf", CaptureState::Bagged, 1), 200)
            .unwrap()
            .unwrap();
        assert_eq!(job.id, "recon-badkf");
        // No frame persisted -> no input_path (the backend fails honestly later).
        assert!(dataset.meta.get("input_path").is_none());
        assert_eq!(dataset.meta["received_keyframes"], 1);
    }
}
