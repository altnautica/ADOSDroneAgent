//! The battery engine's feed: sample the state snapshot, hot-reload the
//! `battery:` config, and record every anomaly transition in the logging store.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ados_protocol::logd::emitter::IngestEmitter;
use ados_protocol::logd::{Fields, Level, Value as FieldValue};
use parking_lot::Mutex;
use tokio::sync::oneshot;

use super::engine::TransitionState;
use super::rules::Severity;
use super::{epoch_ms, AnomalyTransition, BatteryConfig, BatteryEngine};
use crate::ipc::StateIpcClient;

/// The sample cadence. The state snapshot arrives at ~10 Hz; 2 Hz is ample
/// for rates measured over seconds and sizes the engine's 300 s window.
const TICK: Duration = Duration::from_millis(500);

/// The event kind every transition is recorded under.
const EVENT_KIND: &str = "battery.anomaly";

/// Stops the battery task on shutdown and joins it.
pub struct BatteryTaskHandle {
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl BatteryTaskHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

/// Load the `battery:` config at `config_path`, build the engine, and start the
/// task that feeds it from `state`. Returns the engine the route reads and the
/// task's stop handle. Must be called inside a tokio runtime.
pub fn spawn(
    state: StateIpcClient,
    config_path: PathBuf,
) -> (Arc<Mutex<BatteryEngine>>, BatteryTaskHandle) {
    let feed = Feed::new(state, config_path);
    let engine = Arc::clone(&feed.engine);
    let (stop_tx, stop_rx) = oneshot::channel();
    let join = tokio::spawn(run(feed, IngestEmitter::new("ados-control"), stop_rx));
    (
        engine,
        BatteryTaskHandle {
            stop: Some(stop_tx),
            join,
        },
    )
}

async fn run(mut feed: Feed, emitter: IngestEmitter, mut stop: oneshot::Receiver<()>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tick.tick() => {}
        }
        for transition in feed.step() {
            record(&emitter, &transition);
        }
    }
}

/// One tick's worth of state: the engine, and what the last tick already saw.
struct Feed {
    state: StateIpcClient,
    engine: Arc<Mutex<BatteryEngine>>,
    config_path: PathBuf,
    config_mtime: Option<SystemTime>,
    last_arrival: Option<Instant>,
}

impl Feed {
    fn new(state: StateIpcClient, config_path: PathBuf) -> Self {
        // The mtime is read before the file, so an edit landing between the
        // two is picked up on the first tick rather than missed.
        let config_mtime = mtime(&config_path);
        let config = BatteryConfig::load_from(&config_path);
        Self {
            state,
            engine: Arc::new(Mutex::new(BatteryEngine::new(config))),
            config_path,
            config_mtime,
            last_arrival: None,
        }
    }

    /// Reload the config if the file changed, then ingest the snapshot if it is
    /// one this feed has not seen.
    fn step(&mut self) -> Vec<AnomalyTransition> {
        let current = mtime(&self.config_path);
        if current != self.config_mtime {
            self.config_mtime = current;
            let config = BatteryConfig::load_from(&self.config_path);
            tracing::info!(enabled = config.enabled, "battery config reloaded");
            self.engine.lock().set_config(config);
        }
        let Some((arrival, snapshot)) = self.state.snapshot_at() else {
            return Vec::new();
        };
        if self.last_arrival == Some(arrival) {
            return Vec::new();
        }
        self.last_arrival = Some(arrival);
        self.engine.lock().ingest(epoch_ms_at(arrival), &snapshot)
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The wall-clock epoch milliseconds of a monotonic instant in the past.
fn epoch_ms_at(at: Instant) -> i64 {
    let wall = SystemTime::now()
        .checked_sub(at.elapsed())
        .unwrap_or_else(SystemTime::now);
    epoch_ms(wall)
}

/// Record a transition in the logging store. A raise logs at the anomaly's
/// severity; a clear is a recovery and logs at info.
fn record(emitter: &IngestEmitter, t: &AnomalyTransition) {
    let level = match (t.state, t.severity) {
        (TransitionState::Cleared, _) => Level::Info,
        (TransitionState::Raised, Severity::Critical) => Level::Error,
        (TransitionState::Raised, Severity::Warning) => Level::Warn,
    };
    let mut detail = Fields::new();
    detail.insert("rule".to_string(), FieldValue::from(t.rule.as_str()));
    detail.insert("pack_id".to_string(), FieldValue::from(t.pack_id));
    detail.insert("state".to_string(), FieldValue::from(t.state.as_str()));
    detail.insert("value".to_string(), FieldValue::from(t.value));
    detail.insert("threshold".to_string(), FieldValue::from(t.threshold));
    emitter.emit_event(EVENT_KIND, level, detail);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config_file(dir: &Path, yaml: &str, modified: SystemTime) -> PathBuf {
        let path = dir.join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        path
    }

    #[test]
    fn a_fresh_snapshot_feeds_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateIpcClient::disconnected();
        let mut feed = Feed::new(state.clone(), dir.path().join("config.yaml"));
        assert!(feed.step().is_empty());
        assert_eq!(feed.engine.lock().snapshot(0).updated_at_ms, 0);

        state.set_snapshot_for_test(json!({
            "mavlink_alive": true,
            "batteries": [{"id": 0, "cell_voltages": [3.2, 3.2, 3.2], "remaining_pct": 20}],
        }));
        let raised = feed.step();
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].state, TransitionState::Raised);
        let engine = feed.engine.lock();
        let health = engine.snapshot(epoch_ms(SystemTime::now()));
        assert!(!health.stale);
        assert_eq!(health.packs.len(), 1);
    }

    #[test]
    fn an_edited_config_file_is_applied_on_the_next_tick() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = SystemTime::now() - Duration::from_secs(60);
        let path = config_file(dir.path(), "battery:\n  low_cell_mv: 3600\n", t0);
        let mut feed = Feed::new(StateIpcClient::disconnected(), path.clone());
        assert_eq!(feed.engine.lock().config().low_cell_mv, 3600);

        config_file(
            dir.path(),
            "battery:\n  low_cell_mv: 3700\n",
            t0 + Duration::from_secs(1),
        );
        feed.step();
        assert_eq!(feed.engine.lock().config().low_cell_mv, 3700);

        // A deleted file reverts to the defaults.
        std::fs::remove_file(&path).unwrap();
        feed.step();
        assert_eq!(feed.engine.lock().config(), &BatteryConfig::default());
    }
}
