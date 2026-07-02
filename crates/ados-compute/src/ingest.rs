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

use std::collections::{HashMap, HashSet};

use ados_protocol::atlas::{
    AtlasEvent, CaptureState, CaptureStatus, KeyframeEnvelope, ATLAS_CAPTURE_STATE_TOPIC,
    ATLAS_KEYFRAME_TOPIC,
};
use ados_protocol::compute::{ComputeJobKind, ComputeJobState};

use crate::keyframe_persister::{dataset_id_for, KeyframePersister};
use crate::session::{LiveReconstructConfig, LiveReconstructDriver};
use crate::store::{Dataset, JobRecord, JobStore};
use crate::ComputeError;

/// The reconstruct backend a captured session defaults to. The keyframes carry
/// VIO / FC poses, so the gaussian-splat trainer trains directly on the written
/// `transforms.json` with no structure-from-motion pre-pass. The hint resolves to
/// the real tool when it is installed, else the mock backend (CI / no-GPU).
const DEFAULT_RECONSTRUCT_BACKEND: &str = "brush";

/// Per-session live-reconstruction state: the cadence driver plus the in-flight
/// cycle's job id (so the caller can poll the store and release the
/// skip-while-running guard) and the distinct cameras seen (for the snapshot's
/// camera count).
#[derive(Debug)]
struct SessionLive {
    driver: LiveReconstructDriver,
    /// The job id of the periodic cycle currently in flight, if any.
    in_flight_job: Option<String>,
    /// Distinct camera ids seen this session (the snapshot's `cameras` count).
    camera_ids: HashSet<String>,
}

impl SessionLive {
    fn new(config: LiveReconstructConfig, now_ms: i64) -> Self {
        Self {
            driver: LiveReconstructDriver::new(config, now_ms),
            in_flight_job: None,
            camera_ids: HashSet::new(),
        }
    }
}

/// Accumulates a capture session's events: persists each keyframe's image and, on
/// the bagged terminal state, finalizes the dataset and yields the reconstruct
/// job. When live reconstruction is enabled, it also tracks a per-session cadence
/// and, on a tick, snapshots the growing dataset into fresh periodic reconstruct
/// jobs ([`due_reconstructs`](Self::due_reconstructs)) so the world model updates
/// during the flight, not only at the end.
#[derive(Debug)]
pub struct AtlasIngest {
    /// Keyframe events seen this session (received-side delivery count).
    keyframes_seen: u64,
    persister: KeyframePersister,
    /// Live-reconstruction cadence config (disabled by default → final bag only).
    live_config: LiveReconstructConfig,
    /// Per-session live state, keyed by session id. Empty when live reconstruction
    /// is disabled (the sessions are never tracked, so the path is byte-unchanged).
    sessions: HashMap<String, SessionLive>,
    /// The capturing drone's device id, keyed by session id, learned from each
    /// event's `device_id` (the drone-side forwarder stamps it on egress). Stamped
    /// into the dataset + reconstruct job so the compute→cloud producer can
    /// attribute the world model to the drone that captured it. Empty when no event
    /// carried an id (a pre-attribution capture), and the job then omits it rather
    /// than asserting a wrong one.
    session_devices: HashMap<String, String>,
}

impl AtlasIngest {
    /// An ingest that persists datasets under `work_root` (the same root the
    /// reconstructor reads `input_path` from and the artifact server serves), with
    /// live reconstruction disabled (final-bag reconstruct only).
    pub fn new(work_root: impl Into<std::path::PathBuf>) -> Self {
        Self::with_live_config(work_root, LiveReconstructConfig::default())
    }

    /// An ingest with an explicit live-reconstruction cadence. When
    /// `live_config.enabled`, an active session is snapshotted and reconstructed
    /// on the cadence (see [`due_reconstructs`](Self::due_reconstructs)); when
    /// disabled, the node reconstructs only the final bag.
    pub fn with_live_config(
        work_root: impl Into<std::path::PathBuf>,
        live_config: LiveReconstructConfig,
    ) -> Self {
        Self {
            keyframes_seen: 0,
            persister: KeyframePersister::new(work_root),
            live_config,
            sessions: HashMap::new(),
            session_devices: HashMap::new(),
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
                        self.note_device(&kf.session_id, event.device_id.as_deref());
                        match self.persister.persist(&kf) {
                            Ok(()) => self.note_persisted(&kf, now_ms),
                            Err(e) => tracing::warn!(error = %e, "atlas_keyframe_persist_failed"),
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "atlas_keyframe_decode_failed"),
                }
                Ok(None)
            }
            ATLAS_CAPTURE_STATE_TOPIC => match CaptureStatus::from_msgpack(&event.payload) {
                Ok(status) if status.state == CaptureState::Bagged => {
                    self.note_device(&status.session_id, event.device_id.as_deref());
                    let result = self.bag(&status, now_ms)?;
                    // The session is over: drop its live + device state. Any periodic
                    // cycle still running finishes on its own in the worker; the final
                    // bag reconstruct is the authoritative full-set output.
                    self.sessions.remove(&status.session_id);
                    self.session_devices.remove(&status.session_id);
                    Ok(Some(result))
                }
                Ok(status) => {
                    self.note_device(&status.session_id, event.device_id.as_deref());
                    Ok(None)
                }
                Err(e) => {
                    tracing::debug!(error = %e, "atlas_ingest_bad_capture_state");
                    Ok(None)
                }
            },
            _ => Ok(None),
        }
    }

    /// Record the capturing drone's device id for a session from an event's
    /// `device_id`. A no-op for an absent or empty id, so a session is never
    /// attributed to an empty drone (the job then omits `device_id` rather than
    /// asserting a wrong one).
    fn note_device(&mut self, session_id: &str, device_id: Option<&str>) {
        if let Some(dev) = device_id {
            if !dev.is_empty() {
                self.session_devices
                    .insert(session_id.to_string(), dev.to_string());
            }
        }
    }

    /// Record a successfully-persisted keyframe against its session's live
    /// cadence. A no-op when live reconstruction is disabled (no per-session state
    /// is tracked, so the disabled path is byte-unchanged).
    fn note_persisted(&mut self, kf: &KeyframeEnvelope, now_ms: i64) {
        if !self.live_config.enabled {
            return;
        }
        let live = self
            .sessions
            .entry(kf.session_id.clone())
            .or_insert_with(|| SessionLive::new(self.live_config, now_ms));
        live.driver.note_keyframe();
        live.camera_ids.insert(kf.camera_id.clone());
    }

    /// The `(session_id, job_id)` of every session with a periodic cycle in
    /// flight. The caller polls the store for each job's state and, for the ones
    /// that reached a terminal state (or vanished), calls
    /// [`note_cycle_finished`](Self::note_cycle_finished) to release the
    /// skip-while-running guard.
    pub fn in_flight_cycles(&self) -> Vec<(String, String)> {
        self.sessions
            .iter()
            .filter_map(|(session, live)| {
                live.in_flight_job.clone().map(|job| (session.clone(), job))
            })
            .collect()
    }

    /// Release the skip-while-running guard for `session`: its in-flight cycle's
    /// job reached a terminal state (or was purged). A no-op for an unknown or
    /// idle session.
    pub fn note_cycle_finished(&mut self, session_id: &str) {
        if let Some(live) = self.sessions.get_mut(session_id) {
            live.driver.note_cycle_finished();
            live.in_flight_job = None;
        }
    }

    /// For every active session whose cadence is due, snapshot the keyframes
    /// captured so far (a real, full reconstruct of the growing set — NOT
    /// incremental training) and return the `(Dataset, JobRecord)` for the caller
    /// to submit. A due session's skip-while-running guard is armed here, so it
    /// cannot become due again until its job finishes (coalesce, never pile up).
    ///
    /// Each cycle gets its own dataset id (`<dataset>-c<cycle>`) so its artifact
    /// lands in its own directory, while the dataset's `input_path` points at the
    /// shared, growing image directory + the latest snapshot manifest. The worker
    /// runs these exactly like a final-bag reconstruct, so each completed cycle is
    /// a fresh world-model artifact the GCS polls off the per-job output list.
    ///
    /// Only a filesystem fault from writing a snapshot manifest propagates.
    pub fn due_reconstructs(&mut self, now_ms: i64) -> std::io::Result<Vec<(Dataset, JobRecord)>> {
        // Two phases so the persister snapshot (an immutable borrow) and the
        // driver update (a mutable borrow) do not overlap on `self`.
        let due: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, live)| live.driver.due(now_ms))
            .map(|(session, _)| session.clone())
            .collect();

        let mut jobs = Vec::new();
        for session_id in due {
            let dataset_id = dataset_id_for(&session_id);
            let Some(snapshot_dir) = self.persister.snapshot(&dataset_id)? else {
                // No frames on disk yet (the cadence gate makes this unlikely);
                // skip without arming a cycle.
                continue;
            };
            // Read the capturing drone before the mutable session borrow (disjoint
            // fields; sequential keeps it obvious).
            let device_id = self.session_devices.get(&session_id).cloned();
            let live = self
                .sessions
                .get_mut(&session_id)
                .expect("the due session is present");
            let cycle = live.driver.begin_cycle(now_ms);
            let keyframes = live.driver.persisted();
            let cameras = live.camera_ids.len() as u64;

            let cycle_dataset_id = format!("{dataset_id}-c{cycle}");
            let job_id = format!("recon-{session_id}-c{cycle}");
            live.in_flight_job = Some(job_id.clone());

            let mut meta = serde_json::json!({
                "session_id": session_id.clone(),
                "input_path": snapshot_dir.to_string_lossy(),
                "keyframes": keyframes,
                "cameras": cameras,
                "live": true,
                "cycle": cycle,
            });
            // The job carries its capturing session AND the capturing drone so the
            // compute→cloud producer can attribute the world model to the drone
            // (`cmd_atlasJobs.deviceId`). Both stay absent when unknown rather than
            // asserting a wrong id.
            let mut params = serde_json::json!({
                "backend": DEFAULT_RECONSTRUCT_BACKEND,
                "session_id": session_id.clone(),
            });
            if let Some(dev) = &device_id {
                meta["device_id"] = serde_json::Value::String(dev.clone());
                params["device_id"] = serde_json::Value::String(dev.clone());
            }
            let dataset = Dataset {
                id: cycle_dataset_id.clone(),
                kind: "live_snapshot".into(),
                created_ms: now_ms,
                meta,
            };
            let job = JobRecord {
                id: job_id,
                kind: ComputeJobKind::Reconstruct,
                dataset_id: Some(cycle_dataset_id),
                state: ComputeJobState::Queued,
                progress: 0.0,
                params,
                result_ref: None,
                error: None,
                created_ms: now_ms,
                updated_ms: now_ms,
            };
            jobs.push((dataset, job));
        }
        Ok(jobs)
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
        let device_id = self.session_devices.get(&status.session_id).cloned();

        let mut meta = serde_json::json!({
            "keyframes": status.keyframes,
            "cameras": status.camera_count,
            "received_keyframes": self.keyframes_seen,
        });
        if let Some(path) = &input_path {
            meta["input_path"] = serde_json::Value::String(path.to_string_lossy().into_owned());
        }

        // The job carries its capturing session AND the capturing drone so the
        // compute→cloud producer can attribute the world model to the drone
        // (`cmd_atlasJobs.deviceId`). Both stay absent when unknown rather than
        // asserting a wrong id.
        let mut params = serde_json::json!({
            "backend": DEFAULT_RECONSTRUCT_BACKEND,
            "session_id": status.session_id.clone(),
        });
        if let Some(dev) = &device_id {
            meta["device_id"] = serde_json::Value::String(dev.clone());
            params["device_id"] = serde_json::Value::String(dev.clone());
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
            params,
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
        AtlasEvent::new(ATLAS_KEYFRAME_TOPIC, None, kf.to_msgpack().unwrap())
    }

    /// A keyframe event stamped with a capturing-drone id (the egress shape).
    fn keyframe_event_from(session: &str, kf_id: u64, device: &str) -> AtlasEvent {
        let mut ev = real_keyframe_event(session, kf_id);
        ev.device_id = Some(device.to_string());
        ev
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
        AtlasEvent::new(
            ATLAS_CAPTURE_STATE_TOPIC,
            None,
            status.to_msgpack().unwrap(),
        )
    }

    /// A capture-state event stamped with a capturing-drone id (the egress shape).
    fn capture_state_from(
        session: &str,
        state: CaptureState,
        keyframes: u64,
        device: &str,
    ) -> AtlasEvent {
        let mut ev = capture_state(session, state, keyframes);
        ev.device_id = Some(device.to_string());
        ev
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
    fn the_capturing_drone_device_id_reaches_the_dataset_and_job() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::new(dir.path());

        // The stamped keyframe + bag events carry the drone id; it lands on both
        // the dataset meta and the job params so the compute→cloud producer can
        // attribute the world model to the capturing drone.
        ingest
            .step(&keyframe_event_from("sD", 0, "drone-42"), 100)
            .unwrap();
        let (dataset, job) = ingest
            .step(
                &capture_state_from("sD", CaptureState::Bagged, 1, "drone-42"),
                200,
            )
            .unwrap()
            .expect("bagged yields a dataset + job");
        assert_eq!(dataset.meta["device_id"], "drone-42");
        assert_eq!(job.params["device_id"], "drone-42");
        assert_eq!(job.params["session_id"], "sD");
    }

    #[test]
    fn an_unstamped_session_omits_the_device_id_never_asserting_a_wrong_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::new(dir.path());
        // No event carried a device id (a pre-attribution capture): the job omits
        // device_id rather than writing an empty/wrong one.
        ingest.step(&real_keyframe_event("sN", 0), 100).unwrap();
        let (dataset, job) = ingest
            .step(&capture_state("sN", CaptureState::Bagged, 1), 200)
            .unwrap()
            .unwrap();
        assert!(dataset.meta.get("device_id").is_none());
        assert!(job.params.get("device_id").is_none());
    }

    #[test]
    fn the_live_cadence_stamps_the_capturing_drone_on_periodic_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::with_live_config(dir.path(), live(3, 2));
        for kf in 0..3 {
            ingest
                .step(&keyframe_event_from("sL", kf, "drone-9"), 100)
                .unwrap();
        }
        let jobs = ingest.due_reconstructs(200).unwrap();
        assert_eq!(jobs.len(), 1);
        let (dataset, job) = &jobs[0];
        assert_eq!(dataset.meta["device_id"], "drone-9");
        assert_eq!(job.params["device_id"], "drone-9");
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
        let bad = AtlasEvent::new(ATLAS_CAPTURE_STATE_TOPIC, None, b"not msgpack".to_vec());
        assert!(ingest.step(&bad, 100).unwrap().is_none());
    }

    fn live(every: u64, min: u64) -> LiveReconstructConfig {
        LiveReconstructConfig {
            enabled: true,
            every_keyframes: every,
            interval_ms: i64::MAX,
            min_keyframes: min,
        }
    }

    #[test]
    fn live_disabled_produces_no_periodic_reconstructs() {
        // The default config is disabled (opt-in), so no per-session state is
        // tracked and the cadence never fires — the node reconstructs the bag only.
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::new(dir.path());
        for kf in 0..10 {
            ingest.step(&real_keyframe_event("off", kf), 100).unwrap();
        }
        assert!(ingest.due_reconstructs(1_000_000).unwrap().is_empty());
        assert!(ingest.in_flight_cycles().is_empty());
    }

    #[test]
    fn live_enabled_snapshots_a_real_reconstruct_job_on_the_cadence() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::with_live_config(dir.path(), live(3, 2));

        // Below the count trigger: not due.
        ingest.step(&real_keyframe_event("live", 0), 100).unwrap();
        ingest.step(&real_keyframe_event("live", 1), 100).unwrap();
        assert!(ingest.due_reconstructs(100).unwrap().is_empty());

        // The 3rd keyframe hits the cadence: one real reconstruct job over the
        // snapshot of the keyframes captured so far.
        ingest.step(&real_keyframe_event("live", 2), 100).unwrap();
        let jobs = ingest.due_reconstructs(200).unwrap();
        assert_eq!(
            jobs.len(),
            1,
            "the cadence produced one periodic reconstruct"
        );
        let (dataset, job) = &jobs[0];
        assert_eq!(dataset.id, "ds-live-c0");
        assert_eq!(dataset.kind, "live_snapshot");
        assert_eq!(dataset.meta["live"], true);
        assert_eq!(dataset.meta["keyframes"], 3);
        assert_eq!(dataset.meta["cameras"], 1);
        assert_eq!(dataset.meta["cycle"], 0);
        // The snapshot manifest the reconstruct reads exists on disk (real input).
        let input_path = dataset.meta["input_path"].as_str().unwrap();
        assert_eq!(input_path, dir.path().join("ds-live").to_string_lossy());
        assert!(std::path::Path::new(input_path)
            .join("transforms.json")
            .exists());
        assert_eq!(job.id, "recon-live-c0");
        assert_eq!(job.kind, ComputeJobKind::Reconstruct);
        assert_eq!(job.dataset_id.as_deref(), Some("ds-live-c0"));
        assert_eq!(job.params["backend"], "brush");

        // The cycle is in flight (skip-while-running armed).
        assert_eq!(
            ingest.in_flight_cycles(),
            vec![("live".to_string(), "recon-live-c0".to_string())]
        );
    }

    #[test]
    fn live_skips_while_a_cycle_runs_then_resumes_after_it_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::with_live_config(dir.path(), live(3, 2));
        for kf in 0..3 {
            ingest.step(&real_keyframe_event("s", kf), 100).unwrap();
        }
        // First cycle becomes due and is armed.
        assert_eq!(ingest.due_reconstructs(200).unwrap().len(), 1);

        // More keyframes arrive while the cycle runs: NOT due (coalesce, never
        // pile up), so no second job is submitted on top of the running one.
        for kf in 3..6 {
            ingest.step(&real_keyframe_event("s", kf), 300).unwrap();
        }
        assert!(
            ingest.due_reconstructs(400).unwrap().is_empty(),
            "a cycle in flight blocks the next"
        );

        // The job finishes (the caller polled the store and released the guard).
        ingest.note_cycle_finished("s");
        assert!(ingest.in_flight_cycles().is_empty());

        // Now the next cycle (over the new keyframes) is due.
        let jobs = ingest.due_reconstructs(500).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].1.id, "recon-s-c1", "the cycle index advanced");
        assert_eq!(jobs[0].0.meta["keyframes"], 6);
    }

    #[test]
    fn the_bagged_state_drops_the_live_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::with_live_config(dir.path(), live(3, 2));
        for kf in 0..3 {
            ingest.step(&real_keyframe_event("b", kf), 100).unwrap();
        }
        // A live cycle is armed.
        assert_eq!(ingest.due_reconstructs(200).unwrap().len(), 1);
        assert!(!ingest.in_flight_cycles().is_empty());

        // The terminal bag yields the final reconstruct and clears the session,
        // so the cadence stops (no further periodic jobs for a finished session).
        let (_dataset, job) = ingest
            .step(&capture_state("b", CaptureState::Bagged, 3), 300)
            .unwrap()
            .expect("the bag yields the final reconstruct");
        assert_eq!(job.id, "recon-b");
        assert!(ingest.in_flight_cycles().is_empty());
        assert!(ingest.due_reconstructs(1_000_000).unwrap().is_empty());
    }

    #[test]
    fn a_malformed_keyframe_is_counted_but_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let mut ingest = AtlasIngest::new(dir.path());
        // An undecodable keyframe payload still counts as a received delivery, but
        // nothing is persisted, so a bag with only bad keyframes has no input_path.
        let bad = AtlasEvent::new(ATLAS_KEYFRAME_TOPIC, None, vec![0u8; 8]);
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
