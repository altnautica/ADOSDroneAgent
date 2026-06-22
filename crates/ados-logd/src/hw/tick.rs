//! One synchronous sampling pass, grouped by signal class.
//!
//! [`Collector::tick`] is the orchestrator: it stamps one timestamp, carries the
//! constant SoC compat, then dispatches to a per-class fold method whose cadence
//! is due. Each `fold_*` method holds exactly the reads and snapshot/metric
//! writes for its class and returns whether the class produced nothing, so the
//! unavailable-class count is accumulated the same way the inline blocks did.
//! The file IO done here is synchronous, so the caller runs the whole pass on a
//! blocking thread.

use std::time::Instant;

use rmpv::Value as MpVal;

use ados_protocol::logd::{HwSnapshot, TelemetryFrame};

use super::cpu::{read_cpufreq, read_proc_stat, util_pct, ProcStat};
use super::disk::{read_diskstats, read_fs_usage};
use super::helpers::{push_metric, sanitize, used_mb};
use super::memory::{read_meminfo, read_pressure};
use super::net::read_iface_stats;
use super::power::{read_power_rails, RailKind};
use super::sched::{read_loadavg, read_vmstat};
use super::thermal::{read_hwmon_temps, read_thermal_zones};
use super::{
    Collector, TickOutput, DISK_SCHED_CADENCE, FREQ_UTIL_CADENCE, MEM_PSI_CADENCE, NET_CADENCE,
    POWER_CADENCE, SUMMARY_CADENCE, THERMAL_CADENCE, USB_CADENCE,
};
use crate::writer::now_us;

impl Collector {
    /// Run one collector tick at `now`: read every class whose cadence is due,
    /// fold the readings into one [`HwSnapshot`], and accumulate the per-signal
    /// [`TelemetryFrame`] metrics. The file IO done here is synchronous, so a
    /// caller runs the whole pass on a blocking thread.
    ///
    /// Returns the snapshot plus the metric frames it produced. The Pi throttle
    /// flags are NOT read here (they need an async subprocess); the caller folds
    /// them in. The snapshot timestamp is taken once so the snapshot row and its
    /// metrics share an instant.
    pub(super) fn tick(&mut self, now: Instant) -> TickOutput {
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

        if now >= self.next_thermal {
            self.next_thermal = now + THERMAL_CADENCE;
            unavailable += self.fold_thermal(ts, &mut snap, &mut metrics);
        }
        if now >= self.next_freq_util {
            self.next_freq_util = now + FREQ_UTIL_CADENCE;
            unavailable += self.fold_freq_util(ts, &mut snap, &mut metrics);
        }
        if now >= self.next_power {
            self.next_power = now + POWER_CADENCE;
            unavailable += self.fold_power(ts, &mut snap, &mut metrics);
        }
        if now >= self.next_mem_psi {
            self.next_mem_psi = now + MEM_PSI_CADENCE;
            unavailable += self.fold_mem_psi(ts, &mut snap, &mut metrics);
        }
        if now >= self.next_net {
            self.next_net = now + NET_CADENCE;
            unavailable += self.fold_net(ts, &mut snap, &mut metrics);
        }
        if now >= self.next_disk_sched {
            self.next_disk_sched = now + DISK_SCHED_CADENCE;
            unavailable += self.fold_disk_sched(&mut snap);
        }
        if now >= self.next_usb {
            self.next_usb = now + USB_CADENCE;
            unavailable += self.fold_usb(ts, &mut snap, &mut metrics);
        }
        if now >= self.next_summary {
            self.next_summary = now + SUMMARY_CADENCE;
            self.fold_summary(ts, &mut snap, &mut metrics);
        }

        self.unavailable_classes = unavailable;
        TickOutput {
            snapshot: snap,
            metrics,
            throttle_due: now >= self.next_throttle,
            regdomain_due: now >= self.next_regdomain,
            pss_due: now >= self.next_pss,
        }
    }

    /// Thermal zones + hwmon temperatures. The first zone is the quick-glance
    /// primary. Returns `1` when no thermal source was readable.
    fn fold_thermal(
        &self,
        ts: i64,
        snap: &mut HwSnapshot,
        metrics: &mut Vec<TelemetryFrame>,
    ) -> u32 {
        let zones = read_thermal_zones(&self.root);
        let hwmon = read_hwmon_temps(&self.root);
        let unavailable = u32::from(zones.is_empty() && hwmon.is_empty());
        // The first zone is the quick-glance primary temperature.
        if let Some(primary) = zones.first() {
            snap.signals
                .insert("thermal.primary_c".to_string(), MpVal::from(primary.c));
            push_metric(metrics, ts, "thermal.primary_c", primary.c as f64, &[]);
        }
        for z in &zones {
            let key = format!("thermal.{}_c", sanitize(&z.name));
            snap.signals.insert(key.clone(), MpVal::from(z.c));
            push_metric(metrics, ts, &key, z.c as f64, &[("zone", &z.name)]);
        }
        for t in &hwmon {
            let key = format!(
                "thermal.hwmon.{}_{}_c",
                sanitize(&t.chip),
                sanitize(&t.label)
            );
            snap.signals.insert(key.clone(), MpVal::from(t.c));
            push_metric(
                metrics,
                ts,
                &key,
                t.c as f64,
                &[("chip", &t.chip), ("label", &t.label)],
            );
        }
        unavailable
    }

    /// Per-core frequency + governor + utilization. Utilization is a rate against
    /// the previous `/proc/stat`; the aggregate feeds the 1 Hz summary cache.
    /// Returns `1` when neither a core nor a `/proc/stat` aggregate was readable.
    fn fold_freq_util(
        &mut self,
        ts: i64,
        snap: &mut HwSnapshot,
        metrics: &mut Vec<TelemetryFrame>,
    ) -> u32 {
        let cores = read_cpufreq(&self.root);
        let stat = read_proc_stat(&self.root);
        let unavailable = u32::from(cores.is_empty() && stat.aggregate.is_none());
        for c in &cores {
            if let Some(khz) = c.freq_khz {
                let key = format!("cpu.freq.{}", c.core);
                snap.signals.insert(key.clone(), MpVal::from(khz));
                push_metric(
                    metrics,
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
        if let Some(util) = self.fold_utilization(&stat, ts, snap, metrics) {
            self.last_cpu_util_all = Some(util);
        }
        self.baselines.proc_stat = Some(stat);
        unavailable
    }

    /// hwmon voltage / current / power rails. Returns `1` when no rail was readable.
    fn fold_power(&self, ts: i64, snap: &mut HwSnapshot, metrics: &mut Vec<TelemetryFrame>) -> u32 {
        let rails = read_power_rails(&self.root);
        let unavailable = u32::from(rails.is_empty());
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
                metrics,
                ts,
                &key,
                r.value as f64,
                &[("chip", &r.chip), ("label", &r.label), ("unit", unit)],
            );
        }
        unavailable
    }

    /// Memory totals + cache + swap and the pressure-stall `some` lines. Caches
    /// `(total, available)` for the 1 Hz `mem.available_pct` summary. Returns `1`
    /// when neither a memory field nor a PSI line was readable.
    fn fold_mem_psi(
        &mut self,
        ts: i64,
        snap: &mut HwSnapshot,
        metrics: &mut Vec<TelemetryFrame>,
    ) -> u32 {
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
            push_metric(metrics, ts, "mem.used_mb", used_mb(&mem) as f64, &[]);
            push_metric(metrics, ts, "mem.avail_mb", mb as f64, &[]);
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
                push_metric(metrics, ts, &avg10_key, p.avg10 as f64, &[]);
            }
        }
        u32::from(!any && !psi_any)
    }

    /// Per-interface network counters. The byte counters are emitted as metrics;
    /// the rest live in the snapshot blob. Returns `1` when no iface was readable.
    fn fold_net(&self, ts: i64, snap: &mut HwSnapshot, metrics: &mut Vec<TelemetryFrame>) -> u32 {
        let ifaces = read_iface_stats(&self.root);
        let unavailable = u32::from(ifaces.is_empty());
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
                    push_metric(metrics, ts, &key, value as f64, &[("iface", &i.name)]);
                }
            }
        }
        unavailable
    }

    /// Disk I/O sectors, scheduler counters (ctxt/processes), page faults, and
    /// load averages. Returns `1` when nothing in the class was readable.
    fn fold_disk_sched(&self, snap: &mut HwSnapshot) -> u32 {
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
        u32::from(!any)
    }

    /// USB enumeration speed + topology. Returns `1` when no device was readable.
    fn fold_usb(&self, ts: i64, snap: &mut HwSnapshot, metrics: &mut Vec<TelemetryFrame>) -> u32 {
        let devices = super::usb::read_usb_devices(&self.root);
        let unavailable = u32::from(devices.is_empty());
        for d in &devices {
            let id = format!("{}_{}", d.vid, d.pid);
            let key = format!("usb.{id}.speed_mbps");
            snap.signals.insert(key.clone(), MpVal::from(d.speed_mbps));
            push_metric(
                metrics,
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
        unavailable
    }

    /// The 1 Hz headline summary metrics, derived from the values the per-class
    /// reads already produced (only the live `statvfs` is an extra read). Skipped
    /// on a tick that produced no signals at all, mirroring the empty-snapshot
    /// drop in `emit`.
    fn fold_summary(&self, ts: i64, snap: &mut HwSnapshot, metrics: &mut Vec<TelemetryFrame>) {
        // A small canonical set emitted once a second for the at-a-glance health
        // series, derived from the values the per-class reads above already
        // produced (no extra sysfs/proc reads beyond the live filesystem
        // statvfs). `thermal.primary_c` is the fourth canonical summary metric and
        // is already emitted by the thermal class at its own faster cadence, so it
        // is intentionally not duplicated here. The block is skipped on a tick
        // that produced no signals at all (an unreadable root), mirroring the
        // empty-snapshot drop in `emit`, so a board with nothing to read emits no
        // summary either.
        if !snap.signals.is_empty() {
            if let Some(util) = self.last_cpu_util_all {
                push_metric(metrics, ts, "cpu.utilization_pct", util, &[]);
            }
            if let Some((total, avail)) = self.last_mem {
                if total > 0 {
                    let pct = avail as f64 / total as f64 * 100.0;
                    push_metric(metrics, ts, "mem.available_pct", pct, &[]);
                }
            }
            if let Some(used_pct) = super::disk::read_fs_used_pct(&self.root) {
                push_metric(metrics, ts, "disk.used_pct", used_pct, &[]);
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

    /// Mark the throttle class as fired and set its next deadline. Called by the
    /// async loop after it reads the (subprocess-backed) throttle flags.
    pub(super) fn mark_throttle_fired(&mut self, now: Instant) {
        self.next_throttle = now + super::THROTTLE_CADENCE;
    }

    /// Mark the regulatory-domain class as fired and set its next deadline.
    /// Called by the async loop after it reads the (subprocess-backed) `iw reg
    /// get` domain, which mirrors the throttle subprocess pattern.
    pub(super) fn mark_regdomain_fired(&mut self, now: Instant) {
        self.next_regdomain = now + super::REGDOMAIN_CADENCE;
    }

    /// Mark the per-service-memory class as fired and set its next deadline.
    /// Called by the async loop after it resolves unit PIDs (the subprocess-backed
    /// `systemctl` query) and samples `smaps_rollup`.
    pub(super) fn mark_pss_fired(&mut self, now: Instant) {
        self.next_pss = now + super::PSS_CADENCE;
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
