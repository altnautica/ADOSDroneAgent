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

pub mod cpu;
pub mod disk;
pub mod memory;
pub mod net;
pub mod power;
pub mod reader;
pub mod regdomain;
pub mod sched;
pub mod soc;
pub mod thermal;
pub mod throttle;
pub mod usb;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rmpv::Value as MpVal;
use tokio::sync::{mpsc, oneshot};

use ados_protocol::logd::{HwSnapshot, IngestFrame, TelemetryFrame};

use crate::writer::now_us;

use self::cpu::{read_cpufreq, read_proc_stat, util_pct, ProcStat};
use self::disk::{read_diskstats, read_fs_usage};
use self::memory::{read_meminfo, read_pressure};
use self::net::read_iface_stats;
use self::power::{read_power_rails, RailKind};
use self::sched::{read_loadavg, read_vmstat};
use self::soc::{detect_soc, SocFamily, SocInfo};
use self::thermal::{read_hwmon_temps, read_thermal_zones};
use self::throttle::{read_throttle, Throttle};

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
/// Pi throttle bitfield. The subprocess is cheap; the throttle bit must be
/// caught promptly.
pub const THROTTLE_CADENCE: Duration = Duration::from_secs(1);
/// Regulatory domain per phy (`iw reg get`). Changes only on a reg-set; a slow
/// cadence matched to the USB pass keeps the subprocess cost negligible.
pub const REGDOMAIN_CADENCE: Duration = Duration::from_secs(10);
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
    next_throttle: Instant,
    next_regdomain: Instant,
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
            next_throttle: now,
            next_regdomain: now,
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

    /// Run one collector tick at `now`: read every class whose cadence is due,
    /// fold the readings into one [`HwSnapshot`], and accumulate the per-signal
    /// [`TelemetryFrame`] metrics. The file IO done here is synchronous, so a
    /// caller runs the whole pass on a blocking thread.
    ///
    /// Returns the snapshot plus the metric frames it produced. The Pi throttle
    /// flags are NOT read here (they need an async subprocess); the caller folds
    /// them in. The snapshot timestamp is taken once so the snapshot row and its
    /// metrics share an instant.
    fn tick(&mut self, now: Instant) -> TickOutput {
        let ts = now_us();
        let mut snap = HwSnapshot::new(ts);
        let mut metrics: Vec<TelemetryFrame> = Vec::new();
        let mut unavailable = 0u32;

        // soc.compat is constant; carry it on every snapshot for the read edge.
        if !self.soc.compat.is_empty() {
            snap.signals.insert(
                "soc.compat".to_string(),
                MpVal::from(self.soc.compat.clone()),
            );
        }

        // --- thermal (zones + hwmon temps) -----------------------------
        if now >= self.next_thermal {
            self.next_thermal = now + THERMAL_CADENCE;
            let zones = read_thermal_zones(&self.root);
            let hwmon = read_hwmon_temps(&self.root);
            if zones.is_empty() && hwmon.is_empty() {
                unavailable += 1;
            }
            // The first zone is the quick-glance primary temperature.
            if let Some(primary) = zones.first() {
                snap.signals
                    .insert("thermal.primary_c".to_string(), MpVal::from(primary.c));
                push_metric(&mut metrics, ts, "thermal.primary_c", primary.c as f64, &[]);
            }
            for z in &zones {
                let key = format!("thermal.{}_c", sanitize(&z.name));
                snap.signals.insert(key.clone(), MpVal::from(z.c));
                push_metric(&mut metrics, ts, &key, z.c as f64, &[("zone", &z.name)]);
            }
            for t in &hwmon {
                let key = format!(
                    "thermal.hwmon.{}_{}_c",
                    sanitize(&t.chip),
                    sanitize(&t.label)
                );
                snap.signals.insert(key.clone(), MpVal::from(t.c));
                push_metric(
                    &mut metrics,
                    ts,
                    &key,
                    t.c as f64,
                    &[("chip", &t.chip), ("label", &t.label)],
                );
            }
        }

        // --- per-core frequency + governor + utilization ----------------
        if now >= self.next_freq_util {
            self.next_freq_util = now + FREQ_UTIL_CADENCE;
            let cores = read_cpufreq(&self.root);
            let stat = read_proc_stat(&self.root);
            if cores.is_empty() && stat.aggregate.is_none() {
                unavailable += 1;
            }
            for c in &cores {
                if let Some(khz) = c.freq_khz {
                    let key = format!("cpu.freq.{}", c.core);
                    snap.signals.insert(key.clone(), MpVal::from(khz));
                    push_metric(
                        &mut metrics,
                        ts,
                        &key,
                        khz as f64,
                        &[("core", &c.core.to_string())],
                    );
                }
                if let Some(gov) = &c.governor {
                    snap.signals
                        .insert(format!("cpu.gov.{}", c.core), MpVal::from(gov.clone()));
                }
            }
            // Utilization is a rate: compute it against the previous /proc/stat.
            // The returned aggregate feeds the 1 Hz `cpu.utilization_pct` summary.
            if let Some(util) = self.fold_utilization(&stat, ts, &mut snap, &mut metrics) {
                self.last_cpu_util_all = Some(util);
            }
            self.baselines.proc_stat = Some(stat);
        }

        // --- power rails -------------------------------------------------
        if now >= self.next_power {
            self.next_power = now + POWER_CADENCE;
            let rails = read_power_rails(&self.root);
            if rails.is_empty() {
                unavailable += 1;
            }
            for r in &rails {
                let (unit, mkey) = match r.kind {
                    RailKind::Voltage => ("mv", "mv"),
                    RailKind::Current => ("ma", "ma"),
                    RailKind::Power => ("uw", "uw"),
                };
                let key = format!(
                    "power.{}_{}_{}",
                    sanitize(&r.chip),
                    sanitize(&r.label),
                    mkey
                );
                snap.signals.insert(key.clone(), MpVal::from(r.value));
                push_metric(
                    &mut metrics,
                    ts,
                    &key,
                    r.value as f64,
                    &[("chip", &r.chip), ("label", &r.label), ("unit", unit)],
                );
            }
        }

        // --- memory + PSI ------------------------------------------------
        if now >= self.next_mem_psi {
            self.next_mem_psi = now + MEM_PSI_CADENCE;
            let mem = read_meminfo(&self.root);
            let mut any = false;
            if let Some(total) = mem.total {
                any = true;
                snap.signals
                    .insert("mem.total_bytes".to_string(), MpVal::from(total));
            }
            if let Some(avail) = mem.available {
                any = true;
                let mb = avail / (1024 * 1024);
                snap.signals
                    .insert("mem.avail_bytes".to_string(), MpVal::from(avail));
                push_metric(&mut metrics, ts, "mem.used_mb", used_mb(&mem) as f64, &[]);
                push_metric(&mut metrics, ts, "mem.avail_mb", mb as f64, &[]);
            }
            // Cache total + available for the 1 Hz `mem.available_pct` summary.
            if let (Some(total), Some(avail)) = (mem.total, mem.available) {
                self.last_mem = Some((total, avail));
            }
            if let Some(swap_free) = mem.swap_free {
                any = true;
                snap.signals
                    .insert("mem.swap_free_bytes".to_string(), MpVal::from(swap_free));
            }
            if let Some(swap_total) = mem.swap_total {
                any = true;
                snap.signals
                    .insert("mem.swap_total_bytes".to_string(), MpVal::from(swap_total));
            }
            if let Some(cache) = mem.cache {
                any = true;
                snap.signals
                    .insert("mem.cache_bytes".to_string(), MpVal::from(cache));
            }
            // PSI: one `some` line per resource, avg10 is the chart-friendly value.
            let mut psi_any = false;
            for resource in ["cpu", "memory", "io"] {
                if let Some(p) = read_pressure(&self.root, resource) {
                    psi_any = true;
                    let avg10_key = format!("mem.psi.{resource}.some.avg10");
                    snap.signals.insert(avg10_key.clone(), MpVal::from(p.avg10));
                    snap.signals.insert(
                        format!("mem.psi.{resource}.some.total_us"),
                        MpVal::from(p.total_us),
                    );
                    push_metric(&mut metrics, ts, &avg10_key, p.avg10 as f64, &[]);
                }
            }
            if !any && !psi_any {
                unavailable += 1;
            }
        }

        // --- per-iface net counters -------------------------------------
        if now >= self.next_net {
            self.next_net = now + NET_CADENCE;
            let ifaces = read_iface_stats(&self.root);
            if ifaces.is_empty() {
                unavailable += 1;
            }
            for i in &ifaces {
                let n = sanitize(&i.name);
                for (suffix, value) in [
                    ("rx_bytes", i.rx_bytes),
                    ("tx_bytes", i.tx_bytes),
                    ("rx_pkts", i.rx_pkts),
                    ("tx_pkts", i.tx_pkts),
                    ("rx_drop", i.rx_drop),
                    ("tx_drop", i.tx_drop),
                    ("rx_err", i.rx_err),
                    ("tx_err", i.tx_err),
                ] {
                    let key = format!("net.{n}.{suffix}");
                    snap.signals.insert(key.clone(), MpVal::from(value));
                    // The byte counters are the high-value time series; emit those
                    // as metrics, the rest live in the snapshot blob.
                    if suffix == "rx_bytes" || suffix == "tx_bytes" {
                        push_metric(&mut metrics, ts, &key, value as f64, &[("iface", &i.name)]);
                    }
                }
            }
        }

        // --- disk I/O + page faults -------------------------------------
        if now >= self.next_disk_sched {
            self.next_disk_sched = now + DISK_SCHED_CADENCE;
            let disks = read_diskstats(&self.root);
            let vm = read_vmstat(&self.root);
            // ctxt/processes come from /proc/stat; read it once here too (cheap).
            let stat = read_proc_stat(&self.root);
            let mut any = !disks.is_empty();
            for d in &disks {
                let n = sanitize(&d.name);
                snap.signals
                    .insert(format!("disk.{n}.rd_sectors"), MpVal::from(d.rd_sectors));
                snap.signals
                    .insert(format!("disk.{n}.wr_sectors"), MpVal::from(d.wr_sectors));
                snap.signals
                    .insert(format!("disk.{n}.io_ms"), MpVal::from(d.io_ms));
            }
            if let Some(ctxt) = stat.ctxt {
                any = true;
                snap.signals
                    .insert("sched.ctxt".to_string(), MpVal::from(ctxt));
            }
            if let Some(procs) = stat.processes {
                any = true;
                snap.signals
                    .insert("sched.processes".to_string(), MpVal::from(procs));
            }
            if let Some(pf) = vm.pgfault {
                any = true;
                snap.signals
                    .insert("sched.pgfault".to_string(), MpVal::from(pf));
            }
            if let Some(pmf) = vm.pgmajfault {
                any = true;
                snap.signals
                    .insert("sched.pgmajfault".to_string(), MpVal::from(pmf));
            }
            // Load averages from `/proc/loadavg`.
            if let Some(la) = read_loadavg(&self.root) {
                any = true;
                snap.signals
                    .insert("sched.loadavg_1".to_string(), MpVal::from(la.one));
                snap.signals
                    .insert("sched.loadavg_5".to_string(), MpVal::from(la.five));
                snap.signals
                    .insert("sched.loadavg_15".to_string(), MpVal::from(la.fifteen));
            }
            if !any {
                unavailable += 1;
            }
        }

        // --- USB enumeration speed --------------------------------------
        if now >= self.next_usb {
            self.next_usb = now + USB_CADENCE;
            let devices = self::usb::read_usb_devices(&self.root);
            if devices.is_empty() {
                unavailable += 1;
            }
            for d in &devices {
                let id = format!("{}_{}", d.vid, d.pid);
                let key = format!("usb.{id}.speed_mbps");
                snap.signals.insert(key.clone(), MpVal::from(d.speed_mbps));
                push_metric(
                    &mut metrics,
                    ts,
                    &key,
                    d.speed_mbps as f64,
                    &[
                        ("vid", &d.vid),
                        ("pid", &d.pid),
                        ("bus", &d.bus.to_string()),
                        ("dev", &d.dev.to_string()),
                    ],
                );
            }
        }

        // --- 1 Hz headline summary metrics ------------------------------
        //
        // A small canonical set emitted once a second for the at-a-glance health
        // series, derived from the values the per-class reads above already
        // produced (no extra sysfs/proc reads beyond the live filesystem
        // statvfs). `thermal.primary_c` is the fourth canonical summary metric and
        // is already emitted by the thermal class at its own faster cadence, so it
        // is intentionally not duplicated here. The block is skipped on a tick
        // that produced no signals at all (an unreadable root), mirroring the
        // empty-snapshot drop in `emit`, so a board with nothing to read emits no
        // summary either.
        if now >= self.next_summary {
            self.next_summary = now + SUMMARY_CADENCE;
            if !snap.signals.is_empty() {
                if let Some(util) = self.last_cpu_util_all {
                    push_metric(&mut metrics, ts, "cpu.utilization_pct", util, &[]);
                }
                if let Some((total, avail)) = self.last_mem {
                    if total > 0 {
                        let pct = avail as f64 / total as f64 * 100.0;
                        push_metric(&mut metrics, ts, "mem.available_pct", pct, &[]);
                    }
                }
                if let Some(used_pct) = self::disk::read_fs_used_pct(&self.root) {
                    push_metric(&mut metrics, ts, "disk.used_pct", used_pct, &[]);
                }
                // Filesystem total + used bytes (statvfs on the live root mount).
                // Gated with the rest of the summary on a non-empty snapshot so a
                // board that read nothing else emits no row — statvfs ignores the
                // collector's injected root, so this must not run for an empty
                // fixture tree that has no other signals.
                if let Some((total, used)) = read_fs_usage(&self.root) {
                    snap.signals
                        .insert("disk.fs_total_bytes".to_string(), MpVal::from(total));
                    snap.signals
                        .insert("disk.fs_used_bytes".to_string(), MpVal::from(used));
                }
            }
        }

        self.unavailable_classes = unavailable;
        TickOutput {
            snapshot: snap,
            metrics,
            throttle_due: now >= self.next_throttle,
            regdomain_due: now >= self.next_regdomain,
        }
    }

    /// Mark the throttle class as fired and set its next deadline. Called by the
    /// async loop after it reads the (subprocess-backed) throttle flags.
    fn mark_throttle_fired(&mut self, now: Instant) {
        self.next_throttle = now + THROTTLE_CADENCE;
    }

    /// Mark the regulatory-domain class as fired and set its next deadline.
    /// Called by the async loop after it reads the (subprocess-backed) `iw reg
    /// get` domain, which mirrors the throttle subprocess pattern.
    fn mark_regdomain_fired(&mut self, now: Instant) {
        self.next_regdomain = now + REGDOMAIN_CADENCE;
    }

    /// Fold per-core and aggregate utilization into the snapshot + metrics, using
    /// the previous `/proc/stat` baseline. A first sample (no baseline yet) emits
    /// nothing for utilization; it appears from the next tick once a delta exists.
    /// Returns the aggregate utilization percentage when one was computed, so the
    /// caller can cache it for the 1 Hz summary.
    fn fold_utilization(
        &self,
        stat: &ProcStat,
        ts: i64,
        snap: &mut HwSnapshot,
        metrics: &mut Vec<TelemetryFrame>,
    ) -> Option<f64> {
        let prev = self.baselines.proc_stat.as_ref()?;
        let mut aggregate_pct = None;
        // Aggregate utilization, the quick-glance `cpu.util.all` metric.
        if let (Some(p), Some(n)) = (prev.aggregate, stat.aggregate) {
            if let Some(pct) = util_pct(p, n) {
                snap.signals
                    .insert("cpu.util.all".to_string(), MpVal::from(pct));
                push_metric(metrics, ts, "cpu.util.all", pct as f64, &[]);
                aggregate_pct = Some(pct as f64);
            }
        }
        // Per-core utilization, matched by core index across the two samples.
        for (core, n) in &stat.cores {
            let Some(p) = prev.cores.iter().find(|(c, _)| c == core).map(|(_, t)| *t) else {
                continue;
            };
            if let Some(pct) = util_pct(p, *n) {
                let key = format!("cpu.util.{core}");
                snap.signals.insert(key.clone(), MpVal::from(pct));
                push_metric(
                    metrics,
                    ts,
                    &key,
                    pct as f64,
                    &[("core", &core.to_string())],
                );
            }
        }
        aggregate_pct
    }
}

/// What one [`Collector::tick`] produced: the snapshot, its metric frames, and
/// whether the (separately-read) subprocess-backed classes are due this tick.
struct TickOutput {
    snapshot: HwSnapshot,
    metrics: Vec<TelemetryFrame>,
    throttle_due: bool,
    regdomain_due: bool,
}

/// Used memory in MiB, derived from total minus available. Zero when either is
/// absent so the metric never reports a misleading negative.
fn used_mb(mem: &self::memory::MemInfo) -> u64 {
    match (mem.total, mem.available) {
        (Some(t), Some(a)) if t >= a => (t - a) / (1024 * 1024),
        _ => 0,
    }
}

/// Append one telemetry metric with optional string tags.
fn push_metric(
    out: &mut Vec<TelemetryFrame>,
    ts_us: i64,
    metric: &str,
    value: f64,
    tags: &[(&str, &str)],
) {
    let mut frame = TelemetryFrame::new(ts_us, metric, value);
    for (k, v) in tags {
        frame.tags.insert((*k).to_string(), MpVal::from(*v));
    }
    out.push(frame);
}

/// Sanitize a name fragment for use inside a dotted signal/metric key: lower-case
/// it and replace any character that is not `[a-z0-9]` with `_`, so a chip /
/// zone / iface name with spaces or punctuation cannot break the dotted-key
/// convention.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Fold the decoded throttle flags into a snapshot + the `throttle.flags` metric.
fn fold_throttle(t: Throttle, ts: i64, snap: &mut HwSnapshot, metrics: &mut Vec<TelemetryFrame>) {
    snap.signals
        .insert("throttle.raw".to_string(), MpVal::from(t.raw));
    snap.signals.insert(
        "throttle.under_voltage".to_string(),
        MpVal::from(t.under_voltage),
    );
    snap.signals.insert(
        "throttle.freq_capped".to_string(),
        MpVal::from(t.freq_capped),
    );
    snap.signals
        .insert("throttle.throttled".to_string(), MpVal::from(t.throttled));
    snap.signals.insert(
        "throttle.soft_temp_limit".to_string(),
        MpVal::from(t.soft_temp_limit),
    );
    push_metric(metrics, ts, "throttle.flags", t.raw as f64, &[]);
}

/// Send a snapshot and its metric frames into the ingest channel.
///
/// A snapshot that carries no signals is not emitted: a board where nothing was
/// readable on a tick (no `/sys`, no `/proc`) produces no row rather than a
/// stream of empty snapshots. When at least one signal was read, the snapshot
/// and every metric are pushed.
///
/// The hardware stream is low-severity: on a full channel the snapshot and the
/// metrics are dropped by the channel (the daemon's drop policy sheds them), so
/// the collector never blocks the runtime waiting for capacity. `try_send` is
/// used precisely so a saturated writer cannot stall sampling.
fn emit(tx: &mpsc::Sender<IngestFrame>, snapshot: HwSnapshot, metrics: Vec<IngestFrame>) {
    if snapshot.signals.is_empty() {
        return;
    }
    let _ = tx.try_send(IngestFrame::Hw(snapshot));
    for frame in metrics {
        let _ = tx.try_send(frame);
    }
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
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Lay down a fixture tree exercising every reader, so one tick yields a rich
    /// snapshot. Returns the temp dir (kept alive by the caller).
    fn rich_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let w = |rel: &str, body: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        };
        // SoC: a Pi so the throttle class is reachable (the subprocess will be
        // absent in CI, which the throttle reader handles gracefully).
        w(
            "proc/device-tree/compatible",
            "raspberrypi,4-model-b\u{0}brcm,bcm2711\u{0}",
        );
        // Thermal.
        w("sys/class/thermal/thermal_zone0/type", "cpu-thermal\n");
        w("sys/class/thermal/thermal_zone0/temp", "48000\n");
        // hwmon temp + power rails on one chip.
        w("sys/class/hwmon/hwmon0/name", "rpi_volt\n");
        w("sys/class/hwmon/hwmon0/temp1_input", "50000\n");
        w("sys/class/hwmon/hwmon1/name", "ina226\n");
        w("sys/class/hwmon/hwmon1/in1_input", "5000\n");
        w("sys/class/hwmon/hwmon1/curr1_input", "1200\n");
        w("sys/class/hwmon/hwmon1/power1_input", "6000000\n");
        // cpufreq.
        w(
            "sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq",
            "1500000\n",
        );
        w(
            "sys/devices/system/cpu/cpu0/cpufreq/scaling_governor",
            "ondemand\n",
        );
        // /proc/stat for utilization + ctxt/processes.
        w(
            "proc/stat",
            "cpu  100 0 50 1000 20 0 5 0 0 0\ncpu0 100 0 50 1000 20 0 5 0 0 0\nctxt 50000\nprocesses 1000\n",
        );
        // meminfo + PSI.
        w(
            "proc/meminfo",
            "MemTotal: 4000000 kB\nMemAvailable: 3000000 kB\nBuffers: 100000 kB\nCached: 300000 kB\nSwapTotal: 1000000 kB\nSwapFree: 800000 kB\n",
        );
        w("proc/loadavg", "0.50 0.40 0.30 1/200 9999\n");
        w(
            "proc/pressure/cpu",
            "some avg10=1.50 avg60=0.50 avg300=0.10 total=42\n",
        );
        // net.
        let nstats = root.join("sys/class/net/eth0/statistics");
        fs::create_dir_all(&nstats).unwrap();
        fs::write(nstats.join("rx_bytes"), "1000\n").unwrap();
        fs::write(nstats.join("tx_bytes"), "2000\n").unwrap();
        // disk + vmstat.
        w(
            "proc/diskstats",
            " 179 0 mmcblk0 1 0 4000 5 1 0 2000 4 0 6 9\n",
        );
        w("proc/vmstat", "pgfault 7777\npgmajfault 12\n");
        // usb.
        w("sys/bus/usb/devices/1-1/idVendor", "0bda\n");
        w("sys/bus/usb/devices/1-1/idProduct", "a81a\n");
        w("sys/bus/usb/devices/1-1/busnum", "1\n");
        w("sys/bus/usb/devices/1-1/devnum", "4\n");
        w("sys/bus/usb/devices/1-1/speed", "480\n");
        dir
    }

    fn signal_keys(snap: &HwSnapshot) -> Vec<String> {
        snap.signals.keys().cloned().collect()
    }

    #[test]
    fn one_tick_against_a_rich_fixture_populates_every_class() {
        let dir = rich_fixture();
        let mut c = Collector::new(dir.path());
        let out = c.tick(Instant::now());
        let keys = signal_keys(&out.snapshot);
        let has = |k: &str| keys.iter().any(|s| s == k);

        assert!(has("soc.compat"), "soc compat carried");
        assert!(has("thermal.primary_c"), "primary zone temp");
        assert!(has("thermal.cpu_thermal_c"), "named zone temp");
        assert!(
            keys.iter().any(|k| k.starts_with("thermal.hwmon.")),
            "hwmon temp"
        );
        assert!(has("cpu.freq.0"), "core freq");
        assert!(has("cpu.gov.0"), "core governor");
        assert!(
            keys.iter().any(|k| k.starts_with("power.")),
            "power rail signal present: {keys:?}"
        );
        assert!(has("mem.total_bytes"), "mem total");
        assert!(has("mem.avail_bytes"), "mem avail");
        assert!(has("mem.cache_bytes"), "mem cache (buffers + cached)");
        assert!(has("mem.swap_total_bytes"), "swap total");
        assert!(has("sched.loadavg_1"), "load average");
        assert!(has("mem.psi.cpu.some.avg10"), "psi cpu avg10");
        assert!(has("net.eth0.rx_bytes"), "net rx bytes");
        assert!(has("disk.mmcblk0.rd_sectors"), "disk read sectors");
        assert!(has("sched.ctxt"), "context switches");
        assert!(has("sched.pgfault"), "page faults");
        assert!(has("usb.0bda_a81a.speed_mbps"), "usb speed");

        // First tick has no utilization yet (no /proc/stat baseline). The metric
        // set still carries the immediate signals (temp, freq, mem, net, usb).
        assert!(!has("cpu.util.all"), "no utilization on the first sample");
        assert!(
            out.metrics.iter().any(|m| m.metric == "thermal.primary_c"),
            "a temperature metric was emitted"
        );
        assert!(out.throttle_due, "throttle is due on the first tick");
    }

    #[test]
    fn utilization_appears_on_the_second_sample() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let stat_path = root.join("proc/stat");
        fs::create_dir_all(stat_path.parent().unwrap()).unwrap();
        // First sample.
        fs::write(
            &stat_path,
            "cpu  100 0 50 1000 0 0 0 0 0 0\ncpu0 100 0 50 1000 0 0 0 0 0 0\n",
        )
        .unwrap();
        let mut c = Collector::new(root);
        let first = c.tick(Instant::now());
        assert!(!first.snapshot.signals.contains_key("cpu.util.all"));

        // Second sample: 100 more busy of 200 more total -> 50% on cpu0 and agg.
        // Advance the freq/util deadline by sampling at a time past the cadence.
        fs::write(
            &stat_path,
            "cpu  200 0 50 1100 0 0 0 0 0 0\ncpu0 200 0 50 1100 0 0 0 0 0 0\n",
        )
        .unwrap();
        let later = Instant::now() + FREQ_UTIL_CADENCE + Duration::from_millis(1);
        let second = c.tick(later);
        let util = second
            .snapshot
            .signals
            .get("cpu.util.all")
            .and_then(|v| v.as_f64())
            .expect("utilization present on the second sample");
        assert!((util - 50.0).abs() < 0.5, "got {util}");
        // The per-core utilization metric was emitted too.
        assert!(second.metrics.iter().any(|m| m.metric == "cpu.util.0"));
    }

    #[test]
    fn summary_metrics_emit_at_one_hz_from_cached_class_values() {
        // A fixture with memory + thermal + two distinct /proc/stat samples so
        // the 1 Hz summary can derive cpu.utilization_pct, mem.available_pct and
        // (on Linux) disk.used_pct. thermal.primary_c is emitted by the thermal
        // class itself, so it appears in the metric stream too.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let w = |rel: &str, body: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        };
        w("sys/class/thermal/thermal_zone0/type", "cpu-thermal\n");
        w("sys/class/thermal/thermal_zone0/temp", "48000\n");
        // 75% available (3/4 of total).
        w(
            "proc/meminfo",
            "MemTotal: 4000000 kB\nMemAvailable: 3000000 kB\n",
        );
        w(
            "proc/stat",
            "cpu  100 0 50 1000 0 0 0 0 0 0\ncpu0 100 0 50 1000 0 0 0 0 0 0\n",
        );

        let mut c = Collector::new(root);
        // First tick: summary fires (mem cached → mem.available_pct), but cpu
        // utilization has no baseline yet so cpu.utilization_pct is absent.
        let first = c.tick(Instant::now());
        let mem_pct = first
            .metrics
            .iter()
            .find(|m| m.metric == "mem.available_pct")
            .map(|m| m.value)
            .expect("mem.available_pct present on the first summary");
        assert!((mem_pct - 75.0).abs() < 0.5, "got {mem_pct}");
        assert!(
            !first
                .metrics
                .iter()
                .any(|m| m.metric == "cpu.utilization_pct"),
            "no cpu summary on the first sample (no baseline yet)"
        );

        // Advance /proc/stat by 100 busy of 200 total → 50% utilization, and tick
        // past both the summary cadence and the freq/util cadence.
        w(
            "proc/stat",
            "cpu  200 0 50 1100 0 0 0 0 0 0\ncpu0 200 0 50 1100 0 0 0 0 0 0\n",
        );
        let later = Instant::now() + SUMMARY_CADENCE + Duration::from_millis(1);
        let second = c.tick(later);
        let util = second
            .metrics
            .iter()
            .find(|m| m.metric == "cpu.utilization_pct")
            .map(|m| m.value)
            .expect("cpu.utilization_pct present once a baseline exists");
        assert!((util - 50.0).abs() < 0.5, "got {util}");
        assert!(
            second
                .metrics
                .iter()
                .any(|m| m.metric == "mem.available_pct"),
            "mem summary re-emits each second"
        );
        // disk.used_pct reads the live filesystem via statvfs (Linux only); on
        // other dev hosts it is gracefully absent.
        #[cfg(target_os = "linux")]
        assert!(
            second.metrics.iter().any(|m| m.metric == "disk.used_pct"),
            "disk.used_pct present on Linux"
        );
    }

    #[test]
    fn empty_root_yields_a_sparse_snapshot_and_counts_unavailable_classes() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Collector::new(dir.path());
        // SoC node absent -> Other -> no soc.compat signal.
        let out = c.tick(Instant::now());
        assert!(out.snapshot.signals.is_empty(), "no readable signals");
        assert!(out.metrics.is_empty(), "no metrics from an empty root");
        // Every file-based class that was due found nothing on this tick.
        assert!(
            c.unavailable_classes() >= 6,
            "most classes are unavailable: {}",
            c.unavailable_classes()
        );
        assert_eq!(c.soc_family(), SocFamily::Other);
    }

    #[test]
    fn cadence_gating_skips_a_class_until_its_period_elapses() {
        let dir = rich_fixture();
        let mut c = Collector::new(dir.path());
        let t0 = Instant::now();
        // First tick fires every class (all deadlines start at construction time).
        let _ = c.tick(t0);
        // A tick one base-tick later: USB (10s cadence) is NOT due again, but
        // thermal (200ms) is also not due yet at +100ms. Nothing fast re-fires.
        let out = c.tick(t0 + BASE_TICK);
        assert!(
            !out.snapshot
                .signals
                .contains_key("usb.0bda_a81a.speed_mbps"),
            "USB must not re-sample within its 10s cadence"
        );
        // A tick well past the thermal cadence re-fires thermal.
        let out2 = c.tick(t0 + THERMAL_CADENCE + Duration::from_millis(1));
        assert!(
            out2.snapshot.signals.contains_key("thermal.primary_c"),
            "thermal re-fires after its cadence"
        );
    }

    #[test]
    fn fold_throttle_writes_flags_and_a_metric() {
        let mut snap = HwSnapshot::new(1);
        let mut metrics = Vec::new();
        let t = super::throttle::decode_throttle(0x1); // under-voltage active
        fold_throttle(t, 1, &mut snap, &mut metrics);
        assert_eq!(
            snap.signals
                .get("throttle.under_voltage")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            snap.signals.get("throttle.raw").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert!(metrics.iter().any(|m| m.metric == "throttle.flags"));
    }

    #[test]
    fn sanitize_makes_dotted_key_safe_fragments() {
        assert_eq!(sanitize("CPU Thermal"), "cpu_thermal");
        assert_eq!(sanitize("wlan0"), "wlan0");
        assert_eq!(sanitize("VBUS-5V"), "vbus_5v");
    }

    #[tokio::test]
    async fn run_collector_emits_a_snapshot_then_stops_on_shutdown() {
        let dir = rich_fixture();
        let root: PathBuf = dir.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel::<IngestFrame>(256);
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(run_collector(root, tx, stop_rx));

        // Within a few base ticks at least one HwSnapshot must land.
        let mut saw_hw = false;
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Some(IngestFrame::Hw(_))) => {
                    saw_hw = true;
                    break;
                }
                Ok(Some(_)) => continue, // a metric frame; keep draining
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(
            saw_hw,
            "the collector emitted at least one hardware snapshot"
        );

        // Shutdown is observed promptly.
        let _ = stop_tx.send(());
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("collector stops within the bound")
            .expect("collector task did not panic");
    }

    /// A guard so the fixture path helper is exercised even on a host where the
    /// real `/sys` exists: the collector must read the fixture, never the host.
    #[test]
    fn collector_reads_the_injected_root_not_the_host() {
        let dir = tempfile::tempdir().unwrap();
        // Only a single zone in the fixture; if the collector read the host it
        // would (on a real board) report many more, so assert the exact name.
        let p: &Path = dir.path();
        fs::create_dir_all(p.join("sys/class/thermal/thermal_zone0")).unwrap();
        fs::write(
            p.join("sys/class/thermal/thermal_zone0/type"),
            "fixture-zone\n",
        )
        .unwrap();
        fs::write(p.join("sys/class/thermal/thermal_zone0/temp"), "33000\n").unwrap();
        let mut c = Collector::new(p);
        let out = c.tick(Instant::now());
        assert_eq!(
            out.snapshot
                .signals
                .get("thermal.fixture_zone_c")
                .and_then(|v| v.as_f64()),
            Some(33.0)
        );
    }
}
