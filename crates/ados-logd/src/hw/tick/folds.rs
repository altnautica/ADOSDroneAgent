//! The memory, network, disk and USB folds of the hardware tick.

use super::*;

impl Collector {
    /// Memory totals + cache + swap and the pressure-stall `some` lines. Caches
    /// `(total, available)` for the 1 Hz `mem.available_pct` summary. Returns `1`
    /// when neither a memory field nor a PSI line was readable.
    pub(super) fn fold_mem_psi(
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
    pub(super) fn fold_net(
        &self,
        ts: i64,
        snap: &mut HwSnapshot,
        metrics: &mut Vec<TelemetryFrame>,
    ) -> u32 {
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
    pub(super) fn fold_disk_sched(&self, snap: &mut HwSnapshot) -> u32 {
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
    pub(super) fn fold_usb(
        &self,
        ts: i64,
        snap: &mut HwSnapshot,
        metrics: &mut Vec<TelemetryFrame>,
    ) -> u32 {
        let devices = crate::hw::usb::read_usb_devices(&self.root);
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
}
