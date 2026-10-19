//! The battery engine's feed: sample the state snapshot and the rig's INA226
//! gauge, hot-reload the `battery:` config (the gauge binding included), and
//! record every anomaly transition in the logging store.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ados_protocol::logd::emitter::IngestEmitter;
use ados_protocol::logd::{Fields, Level, Value as FieldValue};
use parking_lot::Mutex;
use tokio::sync::oneshot;

use super::engine::TransitionState;
use super::ina226::{GaugeBinding, I2cRegisters, Ina226};
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
/// task that feeds it from `state` and from the gauge the board dict at
/// `board_path` binds. Returns the engine the route reads and the task's stop
/// handle. Must be called inside a tokio runtime.
pub fn spawn(
    state: StateIpcClient,
    config_path: PathBuf,
    board_path: PathBuf,
) -> (Arc<Mutex<BatteryEngine>>, BatteryTaskHandle) {
    let feed = Feed::new(state, config_path, board_path, super::ina226::open_bus);
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
        // A step is blocking I/O end to end: the config file, the board dict,
        // and the gauge's I2C bring-up and register reads. It runs on the
        // blocking pool so a slow card or a slow bus never stalls a reactor
        // worker; the feed moves in and comes back with the transitions.
        let stepped = tokio::task::spawn_blocking(move || {
            let transitions = feed.step();
            (feed, transitions)
        })
        .await;
        let (next, transitions) = match stepped {
            Ok(stepped) => stepped,
            Err(e) => {
                tracing::error!(error = %e, "battery feed step failed; battery task stopped");
                return;
            }
        };
        feed = next;
        for transition in transitions {
            record(&emitter, &transition);
        }
    }
}

/// Opens the I2C device a binding names; injectable so tests use a fake bus.
type OpenBus = fn(&GaugeBinding) -> std::io::Result<Box<dyn I2cRegisters>>;

/// The gauge. `Pending` until a binding source exists, then bound or absent
/// until the config's gauge binding changes; an absent or rejected gauge logs
/// one warning and never produces a pack.
enum Gauge {
    Pending,
    Absent,
    Bound { device: Ina226, failing: bool },
}

/// The config's `battery.gauge` binding as last read.
type ConfigGauge = Result<Option<GaugeBinding>, String>;

/// One tick's worth of state: the engine, and what the last tick already saw.
struct Feed {
    state: StateIpcClient,
    engine: Arc<Mutex<BatteryEngine>>,
    config_path: PathBuf,
    config_mtime: Option<SystemTime>,
    config_gauge: ConfigGauge,
    last_arrival: Option<Instant>,
    board_path: PathBuf,
    open_bus: OpenBus,
    gauge: Gauge,
}

impl Feed {
    fn new(
        state: StateIpcClient,
        config_path: PathBuf,
        board_path: PathBuf,
        open_bus: OpenBus,
    ) -> Self {
        // The mtime is read before the file, so an edit landing between the
        // two is picked up on the first tick rather than missed.
        let config_mtime = mtime(&config_path);
        let config = BatteryConfig::load_from(&config_path);
        let config_gauge = GaugeBinding::from_config_file(&config_path);
        Self {
            state,
            engine: Arc::new(Mutex::new(BatteryEngine::new(config))),
            config_path,
            config_mtime,
            config_gauge,
            last_arrival: None,
            board_path,
            open_bus,
            gauge: Gauge::Pending,
        }
    }

    /// Reload the config if the file changed, read the gauge, then ingest the
    /// snapshot if it is one this feed has not seen.
    fn step(&mut self) -> Vec<AnomalyTransition> {
        let current = mtime(&self.config_path);
        if current != self.config_mtime {
            self.config_mtime = current;
            let config = BatteryConfig::load_from(&self.config_path);
            tracing::info!(enabled = config.enabled, "battery config reloaded");
            self.engine.lock().set_config(config);
            let config_gauge = GaugeBinding::from_config_file(&self.config_path);
            if config_gauge != self.config_gauge {
                // A changed binding rebinds from scratch on this tick.
                self.config_gauge = config_gauge;
                self.gauge = Gauge::Pending;
            }
        }
        let mut transitions = self.step_gauge();
        let Some((arrival, snapshot)) = self.state.snapshot_at() else {
            return transitions;
        };
        if self.last_arrival == Some(arrival) {
            return transitions;
        }
        self.last_arrival = Some(arrival);
        transitions.extend(self.engine.lock().ingest(epoch_ms_at(arrival), &snapshot));
        transitions
    }

    /// Resolve the gauge once a binding source exists, then take one reading.
    fn step_gauge(&mut self) -> Vec<AnomalyTransition> {
        if matches!(self.gauge, Gauge::Pending) {
            self.gauge = resolve_gauge(&self.config_gauge, &self.board_path, self.open_bus);
        }
        let Gauge::Bound { device, failing } = &mut self.gauge else {
            return Vec::new();
        };
        match device.read() {
            Ok(reading) => {
                if std::mem::take(failing) {
                    tracing::info!("battery_gauge_read_recovered");
                }
                let sample = reading.to_sample(epoch_ms(SystemTime::now()));
                self.engine.lock().ingest_gauge(sample)
            }
            Err(e) => {
                if !*failing {
                    tracing::warn!(error = %e, "battery_gauge_read_failed");
                }
                *failing = true;
                Vec::new()
            }
        }
    }
}

/// Bind the config's `battery.gauge`, else the board dict's gauge. `Pending`
/// while neither names one and the board dict is not yet written.
fn resolve_gauge(config_gauge: &ConfigGauge, board_path: &Path, open_bus: OpenBus) -> Gauge {
    let binding = match config_gauge {
        Ok(Some(binding)) => Ok(Some(binding.clone())),
        Err(reason) => Err(format!("battery.gauge: {reason}")),
        Ok(None) => match std::fs::read_to_string(board_path) {
            Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| format!("board unparseable: {e}"))
                .and_then(|board| GaugeBinding::from_board(&board)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Gauge::Pending,
            Err(e) => Err(format!("board unreadable: {e}")),
        },
    };
    let binding = match binding {
        Ok(Some(binding)) => binding,
        Ok(None) => {
            tracing::warn!(reason = "no battery gauge binding", "battery_gauge_absent");
            return Gauge::Absent;
        }
        Err(reason) => {
            tracing::warn!(reason = %reason, "battery_gauge_absent");
            return Gauge::Absent;
        }
    };
    let bound = open_bus(&binding)
        .map_err(|e| format!("open /dev/i2c-{}: {e}", binding.i2c_bus))
        .and_then(|bus| Ina226::bring_up(bus, &binding));
    match bound {
        Ok(device) => {
            tracing::info!(
                i2c_bus = binding.i2c_bus,
                addr = binding.addr,
                shunt_ohm = binding.shunt_ohm,
                max_current_a = binding.max_current_a,
                "battery_gauge_bound"
            );
            Gauge::Bound {
                device,
                failing: false,
            }
        }
        Err(reason) => {
            tracing::warn!(
                i2c_bus = binding.i2c_bus,
                addr = binding.addr,
                reason = %reason,
                "battery_gauge_absent"
            );
            Gauge::Absent
        }
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
#[path = "task_gauge_tests.rs"]
mod gauge_tests;
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::battery::ina226::tests::FakeRegs;

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

    /// The real bus opener is never reached in tests: the board dict is absent.
    fn no_bus(_: &GaugeBinding) -> std::io::Result<Box<dyn I2cRegisters>> {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    fn fake_ina226(_: &GaugeBinding) -> std::io::Result<Box<dyn I2cRegisters>> {
        Ok(Box::new(FakeRegs::ina226()))
    }

    fn not_ina226(_: &GaugeBinding) -> std::io::Result<Box<dyn I2cRegisters>> {
        let mut fake = FakeRegs::ina226();
        fake.regs
            .insert(crate::battery::ina226::REG_MANUFACTURER_ID, 0x1234);
        Ok(Box::new(fake))
    }

    /// How long the slow fake holds every register access.
    const SLOW_REG: Duration = Duration::from_millis(150);

    /// A genuine INA226 behind a bus that takes [`SLOW_REG`] per access.
    struct SlowRegs(FakeRegs);

    impl I2cRegisters for SlowRegs {
        fn read_reg(&mut self, reg: u8) -> std::io::Result<u16> {
            std::thread::sleep(SLOW_REG);
            self.0.read_reg(reg)
        }

        fn write_reg(&mut self, reg: u8, value: u16) -> std::io::Result<()> {
            std::thread::sleep(SLOW_REG);
            self.0.write_reg(reg, value)
        }
    }

    fn slow_ina226(_: &GaugeBinding) -> std::io::Result<Box<dyn I2cRegisters>> {
        Ok(Box::new(SlowRegs(FakeRegs::ina226())))
    }

    /// On a single-threaded runtime, a gauge access made on the reactor would
    /// freeze every other task for the whole bring-up and read (eight slow
    /// accesses). The step runs on the blocking pool, so a short timer on the
    /// same runtime still fires on time, and the reading still lands.
    #[tokio::test]
    async fn a_slow_gauge_never_stalls_the_reactor() {
        let dir = tempfile::tempdir().unwrap();
        let feed = feed_with(dir.path(), Some(gauge_board()), slow_ina226);
        let engine = Arc::clone(&feed.engine);
        let (stop_tx, stop_rx) = oneshot::channel();
        let started = Instant::now();
        let task = tokio::spawn(run(feed, IngestEmitter::new("ados-control"), stop_rx));
        // Hand the reactor to the task so it takes its first tick and starts
        // the step before the timer below is measured.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            started.elapsed() < SLOW_REG * 2,
            "the reactor stalled for {:?} behind the gauge",
            started.elapsed()
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while engine
                .lock()
                .snapshot(epoch_ms(SystemTime::now()))
                .packs
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the gauge reading lands from the blocking pool");
        let _ = stop_tx.send(());
        task.await.unwrap();
    }

    fn feed_with(dir: &Path, board: Option<serde_json::Value>, open: OpenBus) -> Feed {
        let board_path = dir.join("board.json");
        if let Some(board) = board {
            std::fs::write(&board_path, board.to_string()).unwrap();
        }
        Feed::new(
            StateIpcClient::disconnected(),
            dir.join("config.yaml"),
            board_path,
            open,
        )
    }

    fn gauge_board() -> serde_json::Value {
        json!({"battery_gauge": {"type": "ina226", "i2c_bus": 1, "addr": 64,
                                 "shunt_ohm": 0.0005, "max_current_a": 80}})
    }

    fn wire(feed: &Feed) -> serde_json::Value {
        let engine = feed.engine.lock();
        serde_json::to_value(engine.snapshot(epoch_ms(SystemTime::now()))).unwrap()
    }

    #[test]
    fn a_bound_ina226_feeds_a_pack_with_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut feed = feed_with(dir.path(), Some(gauge_board()), fake_ina226);
        feed.step();
        let body = wire(&feed);
        let packs = body["packs"].as_array().unwrap();
        assert_eq!(packs.len(), 1);
        let pack = &packs[0];
        assert_eq!(pack["source"], "ina226");
        assert_eq!(pack["id"], u64::from(crate::battery::engine::GAUGE_PACK_ID));
        assert_eq!(pack["voltage_v"], 16.8);
        assert_eq!(pack["current_a"], 20.0);
        assert_eq!(pack["power_w"], 335.999);
        assert!(pack["remaining_pct"].is_null());
        assert_eq!(body["stale"], false);
    }

    #[test]
    fn no_binding_or_a_rejected_device_yields_no_pack() {
        let dir = tempfile::tempdir().unwrap();
        // Board dict not written yet: the gauge stays pending.
        let mut pending = feed_with(dir.path(), None, fake_ina226);
        pending.step();
        assert!(matches!(pending.gauge, Gauge::Pending));
        assert!(wire(&pending)["packs"].as_array().unwrap().is_empty());

        let dir = tempfile::tempdir().unwrap();
        let mut absent = feed_with(dir.path(), Some(json!({"name": "board"})), fake_ina226);
        absent.step();
        assert!(matches!(absent.gauge, Gauge::Absent));
        assert!(wire(&absent)["packs"].as_array().unwrap().is_empty());

        let dir = tempfile::tempdir().unwrap();
        let mut rejected = feed_with(dir.path(), Some(gauge_board()), not_ina226);
        rejected.step();
        assert!(matches!(rejected.gauge, Gauge::Absent));
        assert!(wire(&rejected)["packs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_fresh_snapshot_feeds_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateIpcClient::disconnected();
        let mut feed = Feed::new(
            state.clone(),
            dir.path().join("config.yaml"),
            dir.path().join("board.json"),
            no_bus,
        );
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
        let mut feed = Feed::new(
            StateIpcClient::disconnected(),
            path.clone(),
            dir.path().join("board.json"),
            no_bus,
        );
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
