//! The multi-stage post-flight reconstruct pipeline.
//!
//! A typical deliverable is a chain: COLMAP poses (SfM pre-pass), then a splat
//! train over the posed frames (the trainer exports the deliverable artifact
//! directly). Each stage is its own [`crate::JobRecord`] so it appears as a row
//! in the Forge job/pipeline rail; the stages are linked by `derived_from`
//! lineage so the Outputs tab can show what an artifact was built from.
//!
//! The runner is stateless over the store: it submits stage 0, and on each
//! stage completion it submits the next stage with the prior stage's output as
//! its input and its `derived_from`. The chaining state (pipeline id, stage
//! index, input uri, derived_from) rides the job params the store already
//! persists, so no schema column is added.

use serde::{Deserialize, Serialize};

use crate::{ComputeError, ComputeJobKind, ComputeJobState, JobRecord, JobStore, Output};

/// One stage of a reconstruct pipeline. Every stage runs as a `Reconstruct`
/// job; the stage selects the backend and what it consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PipelineStage {
    /// SfM pre-pass: camera poses + a sparse cloud (COLMAP).
    Colmap,
    /// Splat / dense training over the posed frames; the trainer exports the
    /// deliverable artifact directly.
    Train,
}

impl PipelineStage {
    /// The reconstruct backend hint this stage drives (the `backend` job param).
    pub fn backend_hint(self) -> &'static str {
        match self {
            Self::Colmap => "colmap",
            Self::Train => "brush",
        }
    }
}

/// A planned reconstruct pipeline over one dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: String,
    pub dataset_id: String,
    pub stages: Vec<PipelineStage>,
}

impl Pipeline {
    /// The canonical post-flight chain: COLMAP poses -> splat train (the trainer
    /// exports the deliverable).
    pub fn post_flight(id: impl Into<String>, dataset_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            dataset_id: dataset_id.into(),
            stages: vec![PipelineStage::Colmap, PipelineStage::Train],
        }
    }

    /// Build the `Reconstruct` job for `index`. The params carry the pipeline
    /// chaining (`pipeline.{id,index,stage}`), the stage's `backend` hint, the
    /// prior stage's output as `input_uri`, and `derived_from` (the prior output
    /// id) so the lineage is queryable from the job. Returns `None` when `index`
    /// is past the last stage.
    pub fn job_for_stage(
        &self,
        index: usize,
        input_uri: Option<&str>,
        derived_from: Option<&str>,
        now_ms: i64,
    ) -> Option<JobRecord> {
        let stage = *self.stages.get(index)?;
        let params = serde_json::json!({
            "pipeline": { "id": self.id, "index": index, "stage": stage },
            "backend": stage.backend_hint(),
            "input_uri": input_uri,
            "derived_from": derived_from,
        });
        Some(JobRecord {
            id: format!("{}-s{index}", self.id),
            kind: ComputeJobKind::Reconstruct,
            dataset_id: Some(self.dataset_id.clone()),
            state: ComputeJobState::Queued,
            progress: 0.0,
            params,
            result_ref: None,
            error: None,
            created_ms: now_ms,
            updated_ms: now_ms,
        })
    }
}

/// The local input a downstream stage chains on. The artifact server rewrites a
/// completed output's `uri` to a fetchable HTTP URL for the GCS while preserving
/// the original `file://` path as `meta.local_uri`; a pipeline stage must consume
/// the local file, not the HTTP URL, so prefer `local_uri` when present.
pub fn chain_input_uri(output: &Output) -> &str {
    output
        .meta
        .get("local_uri")
        .and_then(|v| v.as_str())
        .unwrap_or(&output.uri)
}

/// The stage index a job belongs to, read from its pipeline params. `None` for a
/// job that is not part of a pipeline.
pub fn stage_index_of(job: &JobRecord) -> Option<usize> {
    job.params
        .get("pipeline")?
        .get("index")?
        .as_u64()
        .map(|n| n as usize)
}

/// Drives a [`Pipeline`] over a job store: submit the first stage, then advance
/// to the next stage as each completes.
pub struct PipelineRunner<'a> {
    store: &'a JobStore,
}

impl<'a> PipelineRunner<'a> {
    pub fn new(store: &'a JobStore) -> Self {
        Self { store }
    }

    /// Submit the pipeline's first stage. Returns the queued job.
    pub fn start(&self, pipeline: &Pipeline, now_ms: i64) -> Result<JobRecord, ComputeError> {
        let job = pipeline
            .job_for_stage(0, None, None, now_ms)
            .ok_or_else(|| ComputeError::NotFound(format!("empty pipeline {}", pipeline.id)))?;
        self.store.submit_job(&job)?;
        Ok(job)
    }

    /// A pipeline stage has completed; submit the next stage (if any), feeding it
    /// the completed stage's output as input and `derived_from`. Returns the
    /// newly-queued next job, or `None` when the pipeline is finished. The
    /// completed job must carry a pipeline stage index (else `None`).
    pub fn advance(
        &self,
        pipeline: &Pipeline,
        completed_job: &JobRecord,
        output: &Output,
        now_ms: i64,
    ) -> Result<Option<JobRecord>, ComputeError> {
        let Some(index) = stage_index_of(completed_job) else {
            return Ok(None);
        };
        let next = index + 1;
        match pipeline.job_for_stage(
            next,
            Some(chain_input_uri(output)),
            Some(&output.id),
            now_ms,
        ) {
            Some(job) => {
                self.store.submit_job(&job)?;
                Ok(Some(job))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dataset;

    fn output(id: &str, job_id: &str, kind: &str, uri: &str, now: i64) -> Output {
        Output::new(id.into(), job_id.into(), kind.into(), uri.into(), now)
    }

    fn store_with_dataset(id: &str) -> JobStore {
        let store = JobStore::open_in_memory().unwrap();
        store
            .insert_dataset(&Dataset {
                id: id.into(),
                kind: "bag".into(),
                created_ms: 0,
                meta: serde_json::json!({}),
            })
            .unwrap();
        store
    }

    #[test]
    fn post_flight_pipeline_links_colmap_then_train() {
        let p = Pipeline::post_flight("pl-1", "ds-1");
        assert_eq!(p.stages, vec![PipelineStage::Colmap, PipelineStage::Train]);
        let s0 = p.job_for_stage(0, None, None, 10).unwrap();
        assert_eq!(s0.id, "pl-1-s0");
        assert_eq!(stage_index_of(&s0), Some(0));
        assert_eq!(s0.params["backend"], "colmap");
        assert_eq!(s0.params["pipeline"]["stage"], "colmap");
        // past the last stage is None
        assert!(p.job_for_stage(2, None, None, 10).is_none());
    }

    #[test]
    fn runner_chains_stages_with_derived_from_lineage() {
        let store = store_with_dataset("ds-1");
        let runner = PipelineRunner::new(&store);
        let pipeline = Pipeline::post_flight("pl-1", "ds-1");

        // Stage 0 queued.
        let s0 = runner.start(&pipeline, 10).unwrap();
        assert_eq!(s0.id, "pl-1-s0");
        assert_eq!(
            store.get_job("pl-1-s0").unwrap().unwrap().state,
            ComputeJobState::Queued
        );

        // Stage 0 completes with an output; advance submits stage 1, derived from it.
        let out0 = output(
            "pl-1-s0-out",
            "pl-1-s0",
            "pointcloud",
            "file:///w/colmap.ply",
            20,
        );
        let s1 = runner.advance(&pipeline, &s0, &out0, 30).unwrap().unwrap();
        assert_eq!(s1.id, "pl-1-s1");
        assert_eq!(s1.params["backend"], "brush");
        assert_eq!(s1.params["input_uri"], "file:///w/colmap.ply");
        assert_eq!(s1.params["derived_from"], "pl-1-s0-out");
        assert_eq!(stage_index_of(&s1), Some(1));

        // Stage 1 (the train) is the last stage; once it completes the pipeline
        // is finished — advance has no next stage to submit.
        let out1 = output("pl-1-s1-out", "pl-1-s1", "splat", "file:///w/train.ply", 40);
        assert!(runner.advance(&pipeline, &s1, &out1, 50).unwrap().is_none());
    }

    #[test]
    fn advance_chains_on_the_local_uri_when_the_output_was_rewritten() {
        // A completed output whose uri was rewritten to an HTTP artifact URL still
        // chains the next stage on the preserved local file:// path.
        let store = store_with_dataset("ds-1");
        let runner = PipelineRunner::new(&store);
        let pipeline = Pipeline::post_flight("pl-1", "ds-1");
        let s0 = runner.start(&pipeline, 10).unwrap();

        let mut out0 = output(
            "pl-1-s0-out",
            "pl-1-s0",
            "pointcloud",
            "http://node.local:8092/artifacts/ds-1/colmap",
            20,
        );
        out0.meta = serde_json::json!({ "local_uri": "file:///w/ds-1/colmap" });
        let s1 = runner.advance(&pipeline, &s0, &out0, 30).unwrap().unwrap();
        // The train stage trains on the local file, not the HTTP URL.
        assert_eq!(s1.params["input_uri"], "file:///w/ds-1/colmap");
    }

    #[test]
    fn advance_on_a_non_pipeline_job_is_none() {
        let store = store_with_dataset("ds-1");
        let runner = PipelineRunner::new(&store);
        let pipeline = Pipeline::post_flight("pl-1", "ds-1");
        let plain = JobRecord {
            id: "plain".into(),
            kind: ComputeJobKind::Reconstruct,
            dataset_id: Some("ds-1".into()),
            state: ComputeJobState::Completed,
            progress: 1.0,
            params: serde_json::json!({}),
            result_ref: None,
            error: None,
            created_ms: 0,
            updated_ms: 0,
        };
        let out = output("o", "plain", "splat", "file:///w/x.ply", 1);
        assert!(runner
            .advance(&pipeline, &plain, &out, 2)
            .unwrap()
            .is_none());
    }
}
