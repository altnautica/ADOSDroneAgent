//! The compute node as one unit: the scheduler (the work) plus the cluster (the
//! master/slave view) plus the node's worker capacity, with a `tick` a service
//! loop drives and a `heartbeat` a paired drone or GCS reads. The daemon, the
//! REST job API, mDNS pairing, and the real heartbeat transport wrap this; the
//! engine itself does no I/O so it stays testable.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    Cluster, ClusterDescriptor, ComputeError, ComputeJobState, ComputeRole, JobOutcome, Scheduler,
};

/// The small status a compute node advertises (the seam the cloud + GCS
/// heartbeat fills in later). Carries the cluster view so a paired client sees
/// the whole master/slave cluster, not just the master.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeHeartbeat {
    pub role: ComputeRole,
    pub cluster: ClusterDescriptor,
    pub queue_depth: u32,
    pub active_jobs: u32,
    pub workers_idle: u32,
    /// Live streaming perception-offload sessions (a node streaming detections to
    /// N drones reports N). Distinct from `active_jobs` (queued/running one-shot
    /// jobs); a continuous session is not a queued job. Additive: `#[serde(default)]`
    /// so a reader talking to an older node (no field) reads 0.
    #[serde(default)]
    pub active_sessions: u32,
}

/// One compute node: its scheduler, its cluster view, and its worker count.
pub struct Engine {
    scheduler: Scheduler,
    cluster: Cluster,
    workers: u32,
    /// Live streaming-offload session count, shared with the session manager
    /// (which owns + updates it). `None` when the node runs no session manager
    /// (e.g. tests), reported as 0 in the heartbeat.
    session_counter: Option<Arc<AtomicU32>>,
}

impl Engine {
    /// A node with `workers` worker slots, mastering its own (initially empty)
    /// cluster.
    pub fn new(scheduler: Scheduler, cluster: Cluster, workers: u32) -> Self {
        Self {
            scheduler,
            cluster,
            workers,
            session_counter: None,
        }
    }

    /// Share the streaming-offload session counter so the heartbeat reflects live
    /// offload sessions. The counter is owned + updated by the session manager;
    /// the engine only reads it.
    pub fn set_session_counter(&mut self, counter: Arc<AtomicU32>) {
        self.session_counter = Some(counter);
    }

    /// The scheduler (the API layer submits jobs and reads results through it).
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// The cluster view (slaves register here).
    pub fn cluster_mut(&mut self) -> &mut Cluster {
        &mut self.cluster
    }

    /// Run one unit of work: claim and run the next queued job, if any. A
    /// service loop calls this; the return is the outcome for logging.
    pub fn tick(&self, now_ms: i64) -> Result<Option<JobOutcome>, ComputeError> {
        self.scheduler.run_one(now_ms)
    }

    /// The current node status for the heartbeat. `workers_idle` is the node's
    /// worker count minus the jobs currently running.
    pub fn heartbeat(&self) -> Result<ComputeHeartbeat, ComputeError> {
        // The heartbeat is the hot path a paired drone or GCS polls, so count
        // by state with indexed COUNT(*) queries rather than loading the whole
        // jobs table (params JSON and all) just to count two states.
        let store = self.scheduler.store();
        let queue_depth = store.count_in_state(ComputeJobState::Queued)?;
        let active_jobs = store.count_in_state(ComputeJobState::Running)?;
        let workers_idle = self.workers.saturating_sub(active_jobs);
        let active_sessions = self
            .session_counter
            .as_ref()
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0);
        Ok(ComputeHeartbeat {
            role: self.cluster.role(),
            cluster: self.cluster.descriptor(workers_idle),
            queue_depth,
            active_jobs,
            workers_idle,
            active_sessions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ComputeJobKind, Dataset, JobRecord, JobStore, MockDetector, MockReconstructor,
        SlaveDescriptor,
    };
    use std::sync::Arc;

    fn engine() -> Engine {
        let store = JobStore::open_in_memory().unwrap();
        let scheduler = Scheduler::new(store, Arc::new(MockReconstructor), Arc::new(MockDetector));
        Engine::new(scheduler, Cluster::new_master("node-a"), 2)
    }

    fn queued_reconstruct(id: &str, dataset_id: &str) -> JobRecord {
        JobRecord {
            id: id.into(),
            kind: ComputeJobKind::Reconstruct,
            dataset_id: Some(dataset_id.into()),
            state: ComputeJobState::Queued,
            progress: 0.0,
            params: serde_json::json!({}),
            result_ref: None,
            error: None,
            created_ms: 100,
            updated_ms: 100,
        }
    }

    #[test]
    fn fresh_node_is_an_idle_master() {
        let e = engine();
        let hb = e.heartbeat().unwrap();
        assert_eq!(hb.role, ComputeRole::Master);
        assert_eq!(hb.queue_depth, 0);
        assert_eq!(hb.active_jobs, 0);
        assert_eq!(hb.workers_idle, 2);
        assert_eq!(hb.cluster.master_id, "node-a");
    }

    #[test]
    fn heartbeat_reflects_the_queue_then_drains_on_tick() {
        let e = engine();
        e.scheduler()
            .store()
            .insert_dataset(&Dataset {
                id: "ds-1".into(),
                kind: "bag".into(),
                created_ms: 100,
                meta: serde_json::json!({}),
            })
            .unwrap();
        e.scheduler()
            .store()
            .submit_job(&queued_reconstruct("job-1", "ds-1"))
            .unwrap();

        assert_eq!(e.heartbeat().unwrap().queue_depth, 1);
        let outcome = e.tick(200).unwrap().unwrap();
        assert_eq!(outcome.state, ComputeJobState::Completed);
        let hb = e.heartbeat().unwrap();
        assert_eq!(hb.queue_depth, 0);
        assert_eq!(hb.active_jobs, 0);
        assert_eq!(e.tick(201).unwrap(), None);
    }

    #[test]
    fn slave_capacity_aggregates_into_the_heartbeat() {
        let mut e = engine();
        e.cluster_mut().register_slave(SlaveDescriptor {
            node_id: "node-b".into(),
            accelerators: vec!["cuda:0".into()],
            workers_idle: 4,
            queue_depth: 0,
        });
        let hb = e.heartbeat().unwrap();
        // master idle 2 + slave idle 4 = 6
        assert_eq!(hb.cluster.aggregate_workers_idle, 6);
        assert_eq!(hb.cluster.slaves.len(), 1);
    }

    #[test]
    fn heartbeat_counts_running_jobs_and_idle_workers() {
        let e = engine(); // 2 workers
        let store = e.scheduler().store();
        for (id, st) in [
            ("q", ComputeJobState::Queued),
            ("r1", ComputeJobState::Running),
            ("r2", ComputeJobState::Running),
            ("r3", ComputeJobState::Running),
        ] {
            store.submit_job(&queued_reconstruct(id, "ds")).unwrap();
            if st != ComputeJobState::Queued {
                store.set_job_state(id, st, 0.0, None, None, 1).unwrap();
            }
        }
        let hb = e.heartbeat().unwrap();
        assert_eq!(hb.queue_depth, 1);
        assert_eq!(hb.active_jobs, 3);
        // 3 running > 2 workers, so idle saturates at 0 (never underflows).
        assert_eq!(hb.workers_idle, 0);
    }

    #[test]
    fn heartbeat_reports_active_offload_sessions_from_the_shared_counter() {
        let mut e = engine();
        // No counter set yet: a node with no session manager reports 0, not a panic.
        assert_eq!(e.heartbeat().unwrap().active_sessions, 0);
        let counter = Arc::new(AtomicU32::new(0));
        e.set_session_counter(counter.clone());
        assert_eq!(e.heartbeat().unwrap().active_sessions, 0);
        // Two live streaming sessions surface as active_sessions (distinct from
        // active_jobs, which counts queued/running one-shot jobs).
        counter.fetch_add(2, Ordering::Relaxed);
        assert_eq!(e.heartbeat().unwrap().active_sessions, 2);
        assert_eq!(e.heartbeat().unwrap().active_jobs, 0);
    }
}
