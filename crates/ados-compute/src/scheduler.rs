//! The scheduler: claim the next queued job, run it on the right backend, write
//! the output, and record the terminal state.
//!
//! The work is split into three steps so a real backend (which runs for
//! minutes) never holds the engine lock across its run:
//!
//! 1. [`Scheduler::claim_and_prepare`] — atomically claim the oldest queued job
//!    (flipping it to `Running`) and gather its input. Runs under the engine
//!    lock; cheap and store-only.
//! 2. [`Scheduler::run_backend`] — an associated fn (no `&self`, no store) that
//!    runs the reconstructor/detector. The worker calls this WITHOUT the lock,
//!    so a long run does not block the API.
//! 3. [`Scheduler::finalize`] — re-read the job and record the terminal state.
//!    If the job is no longer `Running` (cancelled or removed during the run)
//!    the cancel wins: the backend result is discarded, not written.
//!
//! [`Scheduler::run_one`] composes all three synchronously for the tests and
//! the instant mock path; the daemon drives the three steps itself so it can
//! drop the lock around the backend run.

use std::sync::Arc;

use crate::{
    ComputeError, ComputeJobKind, ComputeJobState, Dataset, Detection, Detector, FrameRef,
    JobRecord, JobStore, Output, Reconstructor,
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

/// The prepared input a backend needs, gathered while the lock is held so the
/// backend run itself touches no store state.
#[derive(Debug)]
pub enum PreparedInput {
    /// A reconstruction over a resolved dataset.
    Reconstruct(Dataset),
    /// A perception or SLAM offload over one frame.
    Offload {
        kind: ComputeJobKind,
        frame: FrameRef,
    },
}

/// The outcome of [`Scheduler::claim_and_prepare`].
#[derive(Debug)]
pub enum Prepared {
    /// The queue was empty; there was nothing to claim.
    Empty,
    /// The claimed job failed at the prepare step (e.g. a missing dataset or a
    /// malformed frame param). Its terminal `Failed` state is already recorded.
    Failed(JobOutcome),
    /// A job is claimed (now `Running`) and ready for the backend.
    Ready {
        job: JobRecord,
        input: PreparedInput,
    },
}

/// What a backend produced, before it is committed to the store. Held by value
/// so it can cross the lock boundary (the backend runs unlocked, finalize
/// commits this under the lock).
#[derive(Debug)]
pub struct BackendResult {
    pub outputs: Vec<Output>,
    pub detections: Vec<Detection>,
    /// `Some` if the backend failed; the message is recorded on the job.
    pub error: Option<String>,
}

/// Ties the store to the backends. Holds the reconstructor and detector behind
/// `Arc` trait objects so the worker can clone a handle, drop the engine lock,
/// and run the backend without holding it.
pub struct Scheduler {
    store: JobStore,
    reconstructor: Arc<dyn Reconstructor>,
    detector: Arc<dyn Detector>,
}

impl Scheduler {
    pub fn new(
        store: JobStore,
        reconstructor: Arc<dyn Reconstructor>,
        detector: Arc<dyn Detector>,
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

    /// Clone the backend handles so the worker can run them after dropping the
    /// engine lock.
    pub fn backends(&self) -> (Arc<dyn Reconstructor>, Arc<dyn Detector>) {
        (self.reconstructor.clone(), self.detector.clone())
    }

    /// Claim the oldest queued job (flipping it to `Running`) and gather the
    /// input its backend needs. Returns [`Prepared::Empty`] when the queue is
    /// empty. A job that cannot be prepared (missing dataset, malformed frame)
    /// is failed here and returned as [`Prepared::Failed`] — bad input fails the
    /// job, it never propagates, so one bad job never stalls the worker. Run
    /// this under the engine lock; it is store-only and cheap.
    pub fn claim_and_prepare(&self, now_ms: i64) -> Result<Prepared, ComputeError> {
        let job = match self.store.claim_next_queued(now_ms)? {
            Some(j) => j,
            None => return Ok(Prepared::Empty),
        };

        match job.kind {
            ComputeJobKind::Reconstruct => {
                let dataset = match job.dataset_id.as_deref() {
                    Some(id) => self.store.get_dataset(id)?,
                    None => None,
                };
                let Some(dataset) = dataset else {
                    return Ok(Prepared::Failed(self.fail(
                        &job.id,
                        "reconstruct job has no dataset",
                        now_ms,
                    )?));
                };
                Ok(Prepared::Ready {
                    job,
                    input: PreparedInput::Reconstruct(dataset),
                })
            }
            ComputeJobKind::PerceptionOffload | ComputeJobKind::SlamOffload => {
                // The frame to process rides the job params on this lane; absent
                // one, a default frame keeps the mock path exercised. A
                // malformed `frame` is bad input: fail the job (recorded).
                let frame: FrameRef = match job.params.get("frame") {
                    Some(v) => match serde_json::from_value(v.clone()) {
                        Ok(f) => f,
                        Err(e) => {
                            return Ok(Prepared::Failed(self.fail(
                                &job.id,
                                &format!("invalid frame param: {e}"),
                                now_ms,
                            )?));
                        }
                    },
                    None => FrameRef {
                        camera_id: "front".into(),
                        width: 1280,
                        height: 720,
                        ts_ms: now_ms,
                    },
                };
                let kind = job.kind;
                Ok(Prepared::Ready {
                    job,
                    input: PreparedInput::Offload { kind, frame },
                })
            }
        }
    }

    /// Run the backend for a prepared job. An associated fn (no `&self`, no
    /// store access) so the worker can call it WITHOUT the engine lock — the
    /// whole point of the claim/run/finalize split. A backend error is captured
    /// in [`BackendResult::error`] rather than propagating, so finalize records
    /// it as a failed job.
    pub fn run_backend(
        reconstructor: &dyn Reconstructor,
        detector: &dyn Detector,
        job: &JobRecord,
        input: &PreparedInput,
        now_ms: i64,
    ) -> BackendResult {
        match input {
            PreparedInput::Reconstruct(dataset) => {
                match reconstructor.reconstruct(dataset, &job.params) {
                    Ok(out) => {
                        let gaussian_count = out.gaussian_count;
                        // Capture the honest backend name before `out` is moved
                        // into the Output below; `mock` marks a placeholder.
                        let backend = out.backend;
                        let mut output = Output::new(
                            format!("{}-out", job.id),
                            job.id.clone(),
                            out.kind,
                            out.uri,
                            now_ms,
                        );
                        // Surface the backend's result metadata to clients. The
                        // `backend` key lets the GCS badge a `mock` artifact as a
                        // placeholder instead of a real world model.
                        output.meta = serde_json::json!({ "gaussian_count": gaussian_count, "backend": backend });
                        BackendResult {
                            outputs: vec![output],
                            detections: Vec::new(),
                            error: None,
                        }
                    }
                    Err(e) => BackendResult {
                        outputs: Vec::new(),
                        detections: Vec::new(),
                        error: Some(e.to_string()),
                    },
                }
            }
            PreparedInput::Offload { kind, frame } => {
                // A SLAM offload returns poses; a perception offload returns
                // detections. The artifact KIND reflects the job kind while the
                // detections ride the result.
                let out_kind = if matches!(kind, ComputeJobKind::SlamOffload) {
                    "pose"
                } else {
                    "detection"
                };
                // Pixels arrive on the streaming transport (wired next); the
                // job-params lane carries only frame metadata, so pass None. A
                // real detector errors without pixels rather than fabricating a
                // box; the mock ignores them.
                match detector.infer(frame, None) {
                    Ok(detections) => {
                        let output = Output::new(
                            format!("{}-out", job.id),
                            job.id.clone(),
                            out_kind.into(),
                            format!("offload://{out_kind}/{}", job.id),
                            now_ms,
                        );
                        BackendResult {
                            outputs: vec![output],
                            detections,
                            error: None,
                        }
                    }
                    Err(e) => BackendResult {
                        outputs: Vec::new(),
                        detections: Vec::new(),
                        error: Some(e.to_string()),
                    },
                }
            }
        }
    }

    /// Record the terminal state for a job whose backend has run. Re-reads the
    /// job first: if it is no longer `Running` (cancelled or removed while the
    /// backend ran), the cancel WINS — the backend result is discarded and the
    /// current state is returned, never overwritten. Otherwise a backend error
    /// fails the job, and a success inserts every output and completes the job
    /// with the first output's uri as the result ref. Run this under the lock.
    pub fn finalize(
        &self,
        job: &JobRecord,
        result: BackendResult,
        now_ms: i64,
    ) -> Result<JobOutcome, ComputeError> {
        // The backend ran without the lock; a cancel (or a retention purge)
        // could have landed in the meantime. If the job is no longer Running,
        // do not overwrite its terminal state — the cancel wins.
        match self.store.get_job(&job.id)? {
            Some(current) if current.state != ComputeJobState::Running => {
                return Ok(JobOutcome {
                    job_id: job.id.clone(),
                    state: current.state,
                    outputs: Vec::new(),
                    detections: Vec::new(),
                });
            }
            None => {
                return Ok(JobOutcome {
                    job_id: job.id.clone(),
                    state: ComputeJobState::Cancelled,
                    outputs: Vec::new(),
                    detections: Vec::new(),
                });
            }
            Some(_) => {}
        }

        if let Some(message) = result.error {
            self.store.set_job_state(
                &job.id,
                ComputeJobState::Failed,
                0.0,
                None,
                Some(&message),
                now_ms,
            )?;
            return Ok(JobOutcome {
                job_id: job.id.clone(),
                state: ComputeJobState::Failed,
                outputs: Vec::new(),
                detections: Vec::new(),
            });
        }

        for output in &result.outputs {
            self.store.insert_output(output)?;
        }
        let result_ref = result.outputs.first().map(|o| o.uri.clone());
        self.store.set_job_state(
            &job.id,
            ComputeJobState::Completed,
            1.0,
            result_ref.as_deref(),
            None,
            now_ms,
        )?;
        Ok(JobOutcome {
            job_id: job.id.clone(),
            state: ComputeJobState::Completed,
            outputs: result.outputs,
            detections: result.detections,
        })
    }

    /// Claim and run the next queued job synchronously, all in one call. Returns
    /// `None` when the queue is empty. This is the instant path the tests and
    /// the engine `tick` use; the daemon drives `claim_and_prepare` /
    /// `run_backend` / `finalize` itself so it can drop the lock around the
    /// backend run.
    pub fn run_one(&self, now_ms: i64) -> Result<Option<JobOutcome>, ComputeError> {
        match self.claim_and_prepare(now_ms)? {
            Prepared::Empty => Ok(None),
            Prepared::Failed(outcome) => Ok(Some(outcome)),
            Prepared::Ready { job, input } => {
                let result =
                    Self::run_backend(&*self.reconstructor, &*self.detector, &job, &input, now_ms);
                Ok(Some(self.finalize(&job, result, now_ms)?))
            }
        }
    }

    /// Mark a job failed with `message` and return the failed outcome. Used by
    /// the prepare step for bad input.
    fn fail(&self, job_id: &str, message: &str, now_ms: i64) -> Result<JobOutcome, ComputeError> {
        self.store.set_job_state(
            job_id,
            ComputeJobState::Failed,
            0.0,
            None,
            Some(message),
            now_ms,
        )?;
        Ok(JobOutcome {
            job_id: job_id.to_string(),
            state: ComputeJobState::Failed,
            outputs: Vec::new(),
            detections: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MockDetector, MockReconstructor};

    fn scheduler() -> Scheduler {
        Scheduler::new(
            JobStore::open_in_memory().unwrap(),
            Arc::new(MockReconstructor),
            Arc::new(MockDetector),
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

    fn with_dataset(s: &Scheduler, id: &str) {
        s.store()
            .insert_dataset(&Dataset {
                id: id.into(),
                kind: "bag".into(),
                created_ms: 100,
                meta: serde_json::json!({ "cameras": 1 }),
            })
            .unwrap();
    }

    #[test]
    fn empty_queue_runs_nothing() {
        assert!(scheduler().run_one(1).unwrap().is_none());
    }

    #[test]
    fn reconstruct_job_runs_queue_to_worker_to_output() {
        let s = scheduler();
        with_dataset(&s, "ds-1");
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
    fn reconstruct_output_carries_gaussian_count_meta() {
        let s = scheduler();
        with_dataset(&s, "ds-1");
        s.store()
            .submit_job(&queued(
                "job-1",
                ComputeJobKind::Reconstruct,
                Some("ds-1"),
                serde_json::json!({}),
            ))
            .unwrap();

        let outcome = s.run_one(200).unwrap().unwrap();
        // The backend's gaussian_count rides the output meta (the MockReconstructor reports 1000).
        assert_eq!(outcome.outputs[0].meta["gaussian_count"], 1000);
        // The honest backend name rides the same meta so a client can badge a
        // placeholder (the mock path here reports `mock`).
        assert_eq!(outcome.outputs[0].meta["backend"], "mock");
        // And both persisted to the store, not just the in-memory outcome.
        let outs = s.store().outputs_for_job("job-1").unwrap();
        assert_eq!(outs[0].meta["gaussian_count"], 1000);
        assert_eq!(outs[0].meta["backend"], "mock");
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
        assert_eq!(outcome.outputs[0].uri, "offload://pose/job-slam");
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

    #[test]
    fn cancel_of_a_running_job_wins() {
        let s = scheduler();
        with_dataset(&s, "ds-1");
        s.store()
            .submit_job(&queued(
                "job-1",
                ComputeJobKind::Reconstruct,
                Some("ds-1"),
                serde_json::json!({}),
            ))
            .unwrap();

        // Claim the job: it is now Running, modeling the moment a long backend
        // run begins (without the lock).
        let (job, input) = match s.claim_and_prepare(200).unwrap() {
            Prepared::Ready { job, input } => (job, input),
            other => panic!("expected a ready job, got {other:?}"),
        };
        assert_eq!(
            s.store().get_job("job-1").unwrap().unwrap().state,
            ComputeJobState::Running
        );

        // A cancel lands while the backend is running.
        assert!(s.store().cancel_job("job-1", 210).unwrap());

        // The backend finishes and we finalize — the cancel must win.
        let (reconstructor, detector) = s.backends();
        let result = Scheduler::run_backend(&*reconstructor, &*detector, &job, &input, 220);
        let outcome = s.finalize(&job, result, 220).unwrap();

        assert_eq!(outcome.state, ComputeJobState::Cancelled);
        assert!(outcome.outputs.is_empty());
        // The store holds Cancelled, not Completed, and no output was written.
        let job = s.store().get_job("job-1").unwrap().unwrap();
        assert_eq!(job.state, ComputeJobState::Cancelled);
        assert!(s.store().outputs_for_job("job-1").unwrap().is_empty());
    }
}
