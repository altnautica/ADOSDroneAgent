//! The runtime hardware collector: a periodic in-process producer that samples
//! sysfs / proc at per-signal-class cadences and pushes one [`HwSnapshot`] frame
//! plus the key numeric signals as individual [`TelemetryFrame`] metrics into the
//! same bounded ingest channel the socket producers use.
//!
//! It is the runtime counterpart the boot-time hardware probe does not have: the
//! probe answers "does this device exist" once at boot; this loop answers "what
//! is the temperature / per-core frequency / power rail / pressure / per-iface
//! counter / USB speed / throttle bit doing right now", continuously, at the
//! cadence each signal needs.
//!
//! Threading: the collector runs as one tokio task. The sysfs/proc reads are
//! tiny but synchronous, so each sampling pass runs inside `spawn_blocking` and
//! the resulting frames are sent on the async channel; the runtime is never
//! blocked on file IO. The one subprocess-backed signal (the Pi throttle bitfield
//! via `vcgencmd`) is itself async and bounded with a timeout.
//!
//! Graceful-skip is the rule throughout: a missing or unreadable node (no hwmon,
//! no PSI on an old kernel, no `vcgencmd` off the Pi) is recorded as absent for
//! that field and never aborts a tick. The collector tracks how many signal
//! classes produced no data on the last pass so the absence is observable.
//!
//! Scope note: this collector reads file-based signals only. The WFB radio link
//! statistics (RSSI / SNR / FEC / bitrate) are deliberately NOT read here; they
//! arrive from the radio statistics sidecar on a separate path. Keeping the
//! WiFi-station read out of this collector is a deliberate boundary, not an
//! omission.
//!
//! Module layout: this file holds the collector state, its constructor and
//! accessors, and the async [`run_collector`] loop. The synchronous sampling
//! pass — [`Collector::tick`] and the per-signal-class fold methods — lives in
//! [`tick`]; the pure shaping helpers (metric append, key sanitizer, throttle
//! fold, channel emit) live in [`helpers`]. Each per-class sampler already lives
//! in its own submodule (`cpu`, `disk`, `memory`, ...).

pub mod cpu;
pub mod disk;
pub mod helpers;
pub mod memory;
pub mod net;
pub mod npu;
pub mod power;
pub mod pss;
pub mod reader;
pub mod regdomain;
pub mod sched;
pub mod soc;
pub mod thermal;
pub mod throttle;
pub mod tick;
pub mod usb;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use ados_protocol::logd::{HwSnapshot, IngestFrame, TelemetryFrame};

use self::cpu::ProcStat;
use self::helpers::{emit, fold_service_memory, fold_throttle};
use self::pss::{resolve_ados_unit_pids, sample_service_pss};
use self::soc::{detect_soc, SocFamily, SocInfo};
use self::throttle::read_throttle;

// --- cadences (config constants) ----------------------------------------
//
// Each signal class ticks at its own period. Cheap, fast-moving signals sample
// fast; expensive or slow-moving ones sample slow. The scheduler base tick is
// the fastest cadence; a class fires when its own period has elapsed since it
// last fired.

/// Thermal zones + hwmon temperatures. Thermal transients are fast and are the
/// canary for a throttle, so this is one of the fast classes.
pub const THERMAL_CADENCE: Duration = Duration::from_millis(200);
/// Per-core frequency + governor + utilization. Frequency capping shows a
/// throttle as it happens.
pub const FREQ_UTIL_CADENCE: Duration = Duration::from_millis(200);
/// hwmon voltage / current / power rails. Catches a rail sag during a brownout.
pub const POWER_CADENCE: Duration = Duration::from_millis(500);
/// Memory + pressure-stall. Memory pressure builds over seconds.
pub const MEM_PSI_CADENCE: Duration = Duration::from_secs(1);
/// Per-interface network counters. One second is fine for rate derivation.
pub const NET_CADENCE: Duration = Duration::from_secs(1);
/// Disk I/O + page faults. Derived rates.
pub const DISK_SCHED_CADENCE: Duration = Duration::from_secs(1);
/// USB enumeration speed + topology. Changes only on hotplug.
pub const USB_CADENCE: Duration = Duration::from_secs(10);
/// NPU utilization (Rockchip RKNPU debugfs). Load tracks inference bursts, so a
/// sub-second cadence keeps the reading responsive without much cost; absent on a
/// board with no NPU (the class then reads "unavailable", never a fake 0).
pub const NPU_CADENCE: Duration = Duration::from_millis(500);
/// Pi throttle bitfield. The subprocess is cheap; the throttle bit must be
/// caught promptly.
pub const THROTTLE_CADENCE: Duration = Duration::from_secs(1);
/// Regulatory domain per phy (`iw reg get`). Changes only on a reg-set; a slow
/// cadence matched to the USB pass keeps the subprocess cost negligible.
pub const REGDOMAIN_CADENCE: Duration = Duration::from_secs(10);
/// Per-service proportional memory (PSS). The `systemctl`-resolve + `smaps_rollup`
/// read is subprocess-backed, like the throttle / reg-domain passes, so it runs on
/// the async side. Memory footprints drift over seconds-to-minutes, so a 10 s
/// cadence keeps the per-service series useful while the subprocess cost stays low.
pub const PSS_CADENCE: Duration = Duration::from_secs(10);
/// Headline summary metrics (CPU utilization, available memory, disk used,
/// primary temperature). One per second is the at-a-glance health cadence; the
/// underlying per-class reads run faster, this just derives the canonical
/// summary keys from their most recent values.
pub const SUMMARY_CADENCE: Duration = Duration::from_secs(1);

/// The scheduler base tick: the greatest common cadence that lets every class
/// fire close to its own period. The fast classes (thermal, freq/util) fire on
/// nearly every base tick.
pub const BASE_TICK: Duration = Duration::from_millis(100);

/// The default production filesystem root.
pub const DEFAULT_ROOT: &str = "/";

/// A point-in-time sample for the cumulative counters whose rate is derived at
/// read time. Retained between ticks so per-core utilization (the one
/// rate the collector computes inline, for the quick-glance metric) is correct.
#[derive(Debug, Clone, Default)]
struct Baselines {
    /// The previous `/proc/stat` sample, for per-core + aggregate utilization.
    proc_stat: Option<ProcStat>,
}

/// The collector's mutable state across ticks: the injectable root, the detected
/// SoC, the per-class next-fire deadlines, the rate baselines, and the count of
/// signal classes that produced nothing on the last pass.
pub struct Collector {
    root: PathBuf,
    soc: SocInfo,
    baselines: Baselines,
    /// Per-class next-fire instants.
    next_thermal: Instant,
    next_freq_util: Instant,
    next_power: Instant,
    next_mem_psi: Instant,
    next_net: Instant,
    next_disk_sched: Instant,
    next_usb: Instant,
    next_npu: Instant,
    next_throttle: Instant,
    next_regdomain: Instant,
    next_pss: Instant,
    next_summary: Instant,
    /// Latest aggregate CPU utilization percentage, cached from the freq/util
    /// class so the 1 Hz summary can emit `cpu.utilization_pct` without a second
    /// `/proc/stat` read. `None` until the first delta is available.
    last_cpu_util_all: Option<f64>,
    /// Latest `(total, available)` memory bytes, cached from the memory class so
    /// the 1 Hz summary can derive `mem.available_pct`. `None` until read.
    last_mem: Option<(u64, u64)>,
    /// Count of signal classes that yielded no readings on the most recent tick
    /// (a board without hwmon, a kernel without PSI, a non-Pi board, ...). Lets
    /// the absence be observed without turning a graceful skip into an error.
    unavailable_classes: u32,
}

impl Collector {
    /// Build a collector rooted at `root`, detecting the SoC once up front.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let soc = detect_soc(&root);
        let now = Instant::now();
        Self {
            root,
            soc,
            baselines: Baselines::default(),
            next_thermal: now,
            next_freq_util: now,
            next_power: now,
            next_mem_psi: now,
            next_net: now,
            next_disk_sched: now,
            next_usb: now,
            next_npu: now,
            next_throttle: now,
            next_regdomain: now,
            next_pss: now,
            next_summary: now,
            last_cpu_util_all: None,
            last_mem: None,
            unavailable_classes: 0,
        }
    }

    /// The detected SoC family.
    pub fn soc_family(&self) -> SocFamily {
        self.soc.family
    }

    /// The number of signal classes that produced no readings on the most recent
    /// tick.
    pub fn unavailable_classes(&self) -> u32 {
        self.unavailable_classes
    }
}

/// What one [`Collector::tick`] produced: the snapshot, its metric frames, and
/// whether the (separately-read) subprocess-backed classes are due this tick.
struct TickOutput {
    snapshot: HwSnapshot,
    metrics: Vec<TelemetryFrame>,
    throttle_due: bool,
    regdomain_due: bool,
    pss_due: bool,
}

/// Run the hardware collector until `shutdown` resolves.
///
/// The collector owns a clone of the daemon's ingest sender and samples at
/// [`BASE_TICK`], firing each signal class on its own cadence. Each pass runs the
/// synchronous sysfs/proc reads inside `spawn_blocking` so the tokio runtime is
/// never blocked on file IO; the Pi throttle flags (subprocess) are read on the
/// async side and folded in. On `shutdown` the loop returns immediately; the
/// daemon then drops the sender so the writer drains.
///
/// `root` is the injectable filesystem root: `/` in production, a fixture tree in
/// a test.
pub async fn run_collector(
    root: impl Into<PathBuf>,
    tx: mpsc::Sender<IngestFrame>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut collector = Collector::new(root);
    tracing::info!(
        soc = ?collector.soc_family(),
        "hardware collector started"
    );
    let mut ticker = tokio::time::interval(BASE_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("hardware collector stopping");
                break;
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                // Run the synchronous sampling pass off the runtime; move the
                // collector in and back out so its cross-tick state persists.
                let (mut output, mut moved) = match tokio::task::spawn_blocking(move || {
                    let out = collector.tick(now);
                    (out, collector)
                })
                .await
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!(error = %e, "hardware sampling pass failed to join");
                        return;
                    }
                };

                // Read the Pi throttle flags on the async side when due, then fold
                // them into the same snapshot + metric set.
                if output.throttle_due {
                    moved.mark_throttle_fired(now);
                    if let Some(t) = read_throttle(moved.soc_family()).await {
                        let ts = output.snapshot.ts_us;
                        let mut throttle_metrics = Vec::new();
                        fold_throttle(t, ts, &mut output.snapshot, &mut throttle_metrics);
                        output.metrics.extend(throttle_metrics);
                    }
                }

                // Read the regulatory domain on the async side when due (`iw reg
                // get` is a subprocess, like the throttle read) and fold the
                // per-phy country / DFS-region signals into the same snapshot.
                if output.regdomain_due {
                    moved.mark_regdomain_fired(now);
                    let reg = self::regdomain::read_regdomain().await;
                    for (key, value) in reg {
                        output.snapshot.signals.insert(key, value);
                    }
                }

                // Sample per-service proportional memory on the async side when
                // due: the unit→pid resolve is a bounded `systemctl` subprocess
                // (like the throttle / reg-domain reads) and the `smaps_rollup`
                // reads are tiny. Best-effort throughout — no running unit means
                // no per-service rows, not an error.
                if output.pss_due {
                    moved.mark_pss_fired(now);
                    let units = resolve_ados_unit_pids().await;
                    if !units.is_empty() {
                        let services = sample_service_pss(&units);
                        let ts = output.snapshot.ts_us;
                        let mut pss_metrics = Vec::new();
                        fold_service_memory(
                            &services,
                            ts,
                            &mut output.snapshot,
                            &mut pss_metrics,
                        );
                        output.metrics.extend(pss_metrics);
                    }
                }
                collector = moved;

                let metric_frames: Vec<IngestFrame> = output
                    .metrics
                    .into_iter()
                    .map(IngestFrame::Telemetry)
                    .collect();
                emit(&tx, output.snapshot, metric_frames);
            }
        }
    }
}

#[cfg(test)]
mod tests;
