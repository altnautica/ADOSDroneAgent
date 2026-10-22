//! The per-pass module runtime: each `autonomy` RC module runs its own serial
//! transport (TX + RX + liveness watchdog) under an inject-only merge, and the
//! heartbeat folds every module's status into the sidecar's `modules[]`.
//!
//! The pilot module is the lane the pass itself drives; its status comes from
//! the pass's own transport. An autonomy module is independent of it: a dead
//! autonomy port respawns on its own cadence and never tears the pilot down.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::cmdsock::{ModuleTarget, ModuleTargets};
use crate::modules::{fresh_link, ModuleConfig, ModuleRole, ModuleStatus};
use crate::transport::OobQueue;
use ados_crsf_codec::telemetry::LinkStatistics;

/// One autonomy module's live state, shared between its runner and the
/// heartbeat / command socket.
pub(super) struct AutonomyLane {
    pub module: ModuleConfig,
    /// The inject-only merge its transmitter reads.
    pub merge: Arc<Mutex<SourceMerge>>,
    pub telemetry: Arc<Mutex<TelemetryState>>,
    /// True while the module's serial transport is open.
    pub connected: Arc<AtomicBool>,
}

impl AutonomyLane {
    pub fn new(module: ModuleConfig) -> Self {
        Self {
            module,
            merge: Arc::new(Mutex::new(SourceMerge::new(ChannelSourceMode::Inject))),
            telemetry: Arc::new(Mutex::new(TelemetryState::default())),
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn status(&self, now: Instant) -> ModuleStatus {
        let link = fresh_link(&*self.telemetry.lock().await, now);
        ModuleStatus::new(
            &self.module,
            self.connected.load(Ordering::Relaxed),
            link.as_ref(),
        )
    }
}

/// The autonomy lanes for a resolved module set.
pub(super) fn autonomy_lanes(modules: &[ModuleConfig]) -> Vec<Arc<AutonomyLane>> {
    modules
        .iter()
        .filter(|m| m.role == ModuleRole::Autonomy)
        .map(|m| Arc::new(AutonomyLane::new(m.clone())))
        .collect()
}

/// The command socket's module addressing for this pass.
pub(super) fn module_targets(
    pilot: Option<&ModuleConfig>,
    pilot_connected: Arc<AtomicBool>,
    autonomy: &[Arc<AutonomyLane>],
) -> ModuleTargets {
    ModuleTargets {
        pilot_id: pilot.map(|m| m.id.clone()),
        pilot_connected,
        autonomy: autonomy
            .iter()
            .map(|lane| ModuleTarget {
                id: lane.module.id.clone(),
                merge: lane.merge.clone(),
                connected: lane.connected.clone(),
            })
            .collect(),
    }
}

/// Serve the command socket for the whole pass; the server stops when the
/// returned guard drops (every pass exit).
pub(super) fn serve_command_socket(state: CmdState) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        let sock = crate::paths::run_path("crsf-cmd.sock");
        if let Err(e) = cmdsock::serve(state, Path::new(&sock)).await {
            tracing::warn!(error = %e, "crsf command socket failed");
        }
    }))
}

/// Aborts the task it holds when dropped.
pub(super) struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Every module's status, pilot first. `pilot_link` must be fresh.
pub(super) async fn module_statuses(
    pilot: Option<&ModuleConfig>,
    pilot_connected: bool,
    pilot_link: Option<&LinkStatistics>,
    autonomy: &[Arc<AutonomyLane>],
    now: Instant,
) -> Vec<ModuleStatus> {
    let mut out = Vec::with_capacity(autonomy.len() + 1);
    if let Some(pilot) = pilot {
        out.push(ModuleStatus::new(pilot, pilot_connected, pilot_link));
    }
    for lane in autonomy {
        out.push(lane.status(now).await);
    }
    out
}

/// Publish the `unconfigured` body (the pilot transport is not open) with the
/// module list still reporting every autonomy module honestly.
pub(super) async fn publish_unconfigured(
    latest_status: &Mutex<serde_json::Value>,
    metrics: &ados_protocol::logd::emitter::IngestEmitter,
    pilot: Option<&ModuleConfig>,
    autonomy: &[Arc<AutonomyLane>],
) {
    let modules = module_statuses(pilot, false, None, autonomy, Instant::now()).await;
    let inputs = StatsInputs {
        modules: &modules,
        ..StatsInputs::default()
    };
    *latest_status.lock().await = build_stats_value(LaneState::Unconfigured, &inputs);
    write_stats_sidecar(LaneState::Unconfigured, &inputs, Some(metrics));
}

/// Drive one autonomy module for the life of the pass: open its port, run the
/// transmitter + receiver + liveness watchdog, and respawn on any exit until
/// the pass ends.
pub(super) async fn run_autonomy_module(
    lane: Arc<AutonomyLane>,
    rate_hz: u16,
    mut pass_scope: watch::Receiver<bool>,
) {
    let id = lane.module.id.as_str();
    let port = lane.module.port.as_str();
    loop {
        if *pass_scope.borrow() {
            return;
        }
        let Some(stream) = open_serial(port, CRSF_BAUD) else {
            tracing::warn!(module = id, device = port, "crsf_module_serial_open_failed");
            tokio::select! {
                _ = pass_scope.wait_for(|over| *over) => return,
                _ = tokio::time::sleep(RETRY_INTERVAL) => continue,
            }
        };
        tracing::info!(
            module = id,
            device = port,
            baud = CRSF_BAUD,
            rate_hz,
            "crsf_module_serial_open"
        );
        let (read_half, write_half) = tokio::io::split(stream);
        let counters = Arc::new(WireCounters::default());
        // Parameter writes address the pilot module only; this queue stays
        // empty and exists because the transmitter drains one per tick.
        let oob = Arc::new(OobQueue::default());
        let cancel = Shutdown::new();
        let mut tx_task = tokio::spawn(run_tx(
            write_half,
            lane.merge.clone(),
            rate_hz,
            counters.clone(),
            oob,
            cancel.clone(),
        ));
        let mut rx_task = tokio::spawn(run_rx(
            read_half,
            lane.telemetry.clone(),
            counters.clone(),
            cancel.clone(),
        ));
        let mut watchdog_task = tokio::spawn(crate::watchdog::tx_liveness_watchdog(
            counters,
            lane.merge.clone(),
            cancel.clone(),
        ));
        lane.connected.store(true, Ordering::Relaxed);
        let reason = tokio::select! {
            _ = pass_scope.wait_for(|over| *over) => "pass_end",
            _ = &mut tx_task => "tx_exit",
            _ = &mut rx_task => "rx_exit",
            _ = &mut watchdog_task => "tx_stalled",
        };
        lane.connected.store(false, Ordering::Relaxed);
        cancel.trigger();
        tx_task.abort();
        rx_task.abort();
        watchdog_task.abort();
        if reason == "pass_end" {
            return;
        }
        tracing::warn!(module = id, reason, "crsf_module_respawning");
        tokio::select! {
            _ = pass_scope.wait_for(|over| *over) => return,
            _ = tokio::time::sleep(RETRY_INTERVAL) => {}
        }
    }
}
