//! The scheduler: claim the next queued job, run it on the right backend, write
//! the output, and record the terminal state. `run_one` processes a single job
//! (the unit the tests and a worker loop both call); a service wraps it in a
//! loop. One worker is modeled here; a multi-accelerator node runs several.

use crate::{
    ComputeError, ComputeJobKind, ComputeJobState, Detection, Detector, FrameRef, JobStore, Output,
    Reconstructor,
};

/// The result of running one job.
#[derive(Debug, Clone, PartialEq)]
pub struct JobOutcome {
    pub job_id: String,
    pub state: ComputeJobState,
    pub outputs: Vec<Output>,
    /// Detections returned by a perception/SLAM offload job (empty otherwise).
    pub detections: Vec<Detection>,
}

/// Ties the store to the backends. Holds the reconstructor and detector behind
/// trait objects so the mock and a real backend are interchangeable.
pub struct Scheduler {
    store: JobStore,
    reconstructor: Box<dyn Reconstructor>,
    detector: Box<dyn Detector>,
}

impl Scheduler {
    pub fn new(
        store: JobStore,
        reconstructor: Box<dyn Reconstructor>,
        detector: Box<dyn Detector>,
    ) -> Self {
        Self {
            store,
            reconstructor,
            detector,
        }
    }

    /// Borrow the store (the API layer reads jobs/outputs through it).
    pub fn store(&self) -> &JobStore {
        &self.store
    }

    /// Claim and run the next queued job. Returns `None` when the queue is
    /// empty. A backend error fails the job (recorded), it does not propagate,
    /// so one bad job never stalls the worker; store errors do propagate.
    pub fn run_one(&self, now_ms: i64) -> Result<Option<JobOutcome>, ComputeError> {
        let job = match self.store.next_queued_job()? {
            Some(j) => j,
            None => return Ok(None),
        };
        self.store
            .set_job_state(&job.id, ComputeJobState::Running, 0.0, None, None, now_ms)?;

        match job.kind {
            ComputeJobKind::Reconstruct => {
                self.run_reconstruct(&job.id, &job.dataset_id, &job.params, now_ms)
            }
            ComputeJobKind::PerceptionOffload | ComputeJobKind::SlamOffload => {
                self.run_offload(&job.id, job.kind, &job.params, now_ms)
            }
        }
    }

    fn run_reconstruct(
        &self,
        job_id: &str,
        dataset_id: &Option<String>,
        params: &serde_json::Value,
        now_ms: i64,
    ) -> Result<Option<JobOutcome>, ComputeError> {
        let dataset = match dataset_id.as_deref() {
            Some(id) => self.store.get_dataset(id)?,
            None => None,
        };
        let Some(dataset) = dataset else {
            return self.fail(job_id, "reconstruct job has no dataset", now_ms);
        };

        match self.reconstructor.reconstruct(&dataset, params) {
            Ok(out) => {
                let output = Output {
                    id: format!("{job_id}-out"),
                    job_id: job_id.to_string(),
                    kind: out.kind,
                    uri: out.uri.clone(),
                    created_ms: now_ms,
                };
                self.store.insert_output(&output)?;
                self.store.set_job_state(
                    job_id,
                    ComputeJobState::Completed,
                    1.0,
                    Some(&out.uri),
                    None,
                    now_ms,
                )?;
                Ok(Some(JobOutcome {
                    job_id: job_id.to_string(),
                    state: ComputeJobState::Completed,
                    outputs: vec![output],
                    detections: Vec::new(),
                }))
            }
            Err(e) => self.fail(job_id, &e.to_string(), now_ms),
        }
    }

    fn run_offload(
        &self,
        job_id: &str,
        kind: ComputeJobKind,
        params: &serde_json::Value,
        now_ms: i64,
    ) -> Result<Option<JobOutcome>, ComputeError> {
        // The frame to process rides the job params on this lane; absent one, a
        // default frame keeps the mock path exercised. A malformed `frame` is
        // bad input: fail the job (recorded), never propagate, per the run_one
        // contract (mirrors the missing-dataset path in run_reconstruct).
        let frame: FrameRef = match params.get("frame") {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(f) => f,
                Err(e) => return self.fail(job_id, &format!("invalid frame param: {e}"), now_ms),
            },
            None => FrameRef {
                camera_id: "front".into(),
                width: 1280,
                height: 720,
                ts_ms: now_ms,
            },
        };

        // A SLAM offload returns poses; a perception offload returns detections.
        // The mock detector stands in for both real backends, so the artifact
        // KIND reflects the job kind while the detections ride the outcome.
        let out_kind = match kind {
            ComputeJobKind::SlamOffload => "pose",
            _ => "detection",
        };

        match self.detector.infer(&frame) {
            Ok(detections) => {
                let output = Output {
                    id: format!("{job_id}-out"),
                    job_id: job_id.to_string(),
                    kind: out_kind.into(),
                    uri: format!("mock://{out_kind}/{job_id}"),
                    created_ms: now_ms,
                };
                self.store.insert_output(&output)?;
                self.store.set_job_state(
                    job_id,
                    ComputeJobState::Completed,
                    1.0,
                    Some(&output.uri),
                    None,
                    now_ms,
                )?;
                Ok(Some(JobOutcome {
                    job_id: job_id.to_string(),
                    state: ComputeJobState::Completed,
                    outputs: vec![output],
                    detections,
                }))
            }
            Err(e) => self.fail(job_id, &e.to_string(), now_ms),
        }
    }

    fn fail(
        &self,
        job_id: &str,
        message: &str,
        now_ms: i64,
    ) -> Result<Option<JobOutcome>, ComputeError> {
        self.store.set_job_state(
            job_id,
            ComputeJobState::Failed,
            0.0,
            None,
            Some(message),
            now_ms,
        )?;
        Ok(Some(JobOutcome {
            job_id: job_id.to_string(),
            state: ComputeJobState::Failed,
            outputs: Vec::new(),
            detections: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dataset, JobRecord, MockDetector, MockReconstructor};

    fn scheduler() -> Scheduler {
        Scheduler::new(
            JobStore::open_in_memory().unwrap(),
            Box::new(MockReconstructor),
            Box::new(MockDetector),
        )
    }

    fn queued(
        id: &str,
        kind: ComputeJobKind,
        dataset_id: Option<&str>,
        params: serde_json::Value,
    ) -> JobRecord {
        JobRecord {
            id: id.into(),
            kind,
            dataset_id: dataset_id.map(Into::into),
            state: ComputeJobState::Queued,
            progress: 0.0,
            params,
            result_ref: None,
            error: None,
            created_ms: 100,
            updated_ms: 100,
        }
    }

    #[test]
    fn empty_queue_runs_nothing() {
        assert!(scheduler().run_one(1).unwrap().is_none());
    }

    #[test]
    fn reconstruct_job_runs_queue_to_worker_to_output() {
        let s = scheduler();
        s.store()
            .insert_dataset(&Dataset {
                id: "ds-1".into(),
                kind: "bag".into(),
                created_ms: 100,
                meta: serde_json::json!({ "cameras": 1 }),
            })
            .unwrap();
        s.store()
            .submit_job(&queued(
                "job-1",
                ComputeJobKind::Reconstruct,
                Some("ds-1"),
                serde_json::json!({}),
            ))
            .unwrap();

        let outcome = s.run_one(200).unwrap().unwrap();
        assert_eq!(outcome.state, ComputeJobState::Completed);
        assert_eq!(outcome.outputs.len(), 1);
        assert_eq!(outcome.outputs[0].kind, "splat");
        assert_eq!(outcome.outputs[0].uri, "mock://splat/ds-1");

        // The store reflects the terminal state and the artifact.
        let job = s.store().get_job("job-1").unwrap().unwrap();
        assert_eq!(job.state, ComputeJobState::Completed);
        assert_eq!(job.result_ref.as_deref(), Some("mock://splat/ds-1"));
        assert_eq!(s.store().outputs_for_job("job-1").unwrap().len(), 1);
    }

    #[test]
    fn perception_offload_returns_a_detection() {
        let s = scheduler();
        s.store()
            .submit_job(&queued(
                "job-off",
                ComputeJobKind::PerceptionOffload,
                None,
                serde_json::json!({ "frame": { "camera_id": "front", "width": 640, "height": 480, "ts_ms": 7 } }),
            ))
            .unwrap();

        let outcome = s.run_one(300).unwrap().unwrap();
        assert_eq!(outcome.state, ComputeJobState::Completed);
        assert_eq!(outcome.detections.len(), 1);
        assert_eq!(outcome.detections[0].class, "object");
        assert_eq!(outcome.detections[0].track_id, Some(7));
        assert_eq!(outcome.outputs[0].kind, "detection");
    }

    #[test]
    fn reconstruct_without_dataset_fails_the_job_not_the_worker() {
        let s = scheduler();
        s.store()
            .submit_job(&queued(
                "job-bad",
                ComputeJobKind::Reconstruct,
                Some("missing"),
                serde_json::json!({}),
            ))
            .unwrap();
        let outcome = s.run_one(200).unwrap().unwrap();
        assert_eq!(outcome.state, ComputeJobState::Failed);
        let job = s.store().get_job("job-bad").unwrap().unwrap();
        assert_eq!(job.state, ComputeJobState::Failed);
        assert!(job.error.is_some());
        // The worker is still usable for the next job.
        assert!(s.run_one(201).unwrap().is_none());
    }

    #[test]
    fn offload_with_malformed_frame_fails_the_job_not_the_worker() {
        let s = scheduler();
        // A `frame` that is present but malformed is bad input: it must fail the
        // job (recorded), not propagate and orphan the job in Running.
        s.store()
            .submit_job(&queued(
                "job-bad-frame",
                ComputeJobKind::PerceptionOffload,
                None,
                serde_json::json!({ "frame": { "width": "oops" } }),
            ))
            .unwrap();
        let outcome = s.run_one(200).unwrap().unwrap();
        assert_eq!(outcome.state, ComputeJobState::Failed);
        let job = s.store().get_job("job-bad-frame").unwrap().unwrap();
        assert_eq!(job.state, ComputeJobState::Failed);
        assert!(job
            .error
            .as_deref()
            .unwrap()
            .contains("invalid frame param"));
        // The worker is not stalled.
        assert!(s.run_one(201).unwrap().is_none());
    }

    #[test]
    fn slam_offload_produces_a_pose_output() {
        let s = scheduler();
        s.store()
            .submit_job(&queued(
                "job-slam",
                ComputeJobKind::SlamOffload,
                None,
                serde_json::json!({}),
            ))
            .unwrap();
        let outcome = s.run_one(300).unwrap().unwrap();
        assert_eq!(outcome.state, ComputeJobState::Completed);
        // A SLAM offload's artifact is a pose, not a detection.
        assert_eq!(outcome.outputs[0].kind, "pose");
        assert_eq!(outcome.outputs[0].uri, "mock://pose/job-slam");
    }

    #[test]
    fn jobs_run_in_fifo_order() {
        let s = scheduler();
        for (id, t) in [("job-2", 200), ("job-1", 100)] {
            s.store()
                .insert_dataset(&Dataset {
                    id: format!("ds-{id}"),
                    kind: "bag".into(),
                    created_ms: t,
                    meta: serde_json::json!({}),
                })
                .unwrap();
            let mut j = queued(
                id,
                ComputeJobKind::Reconstruct,
                Some(&format!("ds-{id}")),
                serde_json::json!({}),
            );
            j.created_ms = t;
            j.updated_ms = t;
            s.store().submit_job(&j).unwrap();
        }
        // job-1 (created 100) before job-2 (created 200).
        assert_eq!(s.run_one(300).unwrap().unwrap().job_id, "job-1");
        assert_eq!(s.run_one(301).unwrap().unwrap().job_id, "job-2");
    }
}
