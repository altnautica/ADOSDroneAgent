//! Per-core frequency / governor and CPU utilization readers.
//!
//! Two sources:
//!
//! - `/sys/devices/system/cpu/cpu*/cpufreq/{scaling_cur_freq,scaling_governor}`
//!   — the current per-core frequency (kHz) and the active governor name. Read
//!   directly each tick; no diffing needed.
//! - `/proc/stat` — the cumulative per-core jiffy counters. Utilization is a
//!   *rate*, so it is derived from the delta between two successive samples: the
//!   collector keeps the previous [`ProcStat`] and computes the busy fraction
//!   over the interval. A single sample on its own yields no utilization (there
//!   is nothing to diff against yet); the first tick records frequency/governor
//!   and seeds the stat baseline, and utilization appears from the second tick.

use std::path::Path;

use super::reader::{list_dir, read_trimmed, read_u32, under};

/// One core's frequency and governor.
#[derive(Debug, Clone, PartialEq)]
pub struct CoreFreq {
    /// Logical core index (the `N` in `cpuN`).
    pub core: u32,
    /// Current scaling frequency in kHz, when readable.
    pub freq_khz: Option<u32>,
    /// Active scaling governor (e.g. `performance`, `schedutil`), when readable.
    pub governor: Option<String>,
}

/// Read per-core `scaling_cur_freq` (kHz) and `scaling_governor` for every
/// `/sys/devices/system/cpu/cpu*/cpufreq` that exists. A core whose `cpufreq`
/// directory is absent (offline / no cpufreq driver) is skipped.
pub fn read_cpufreq(root: &Path) -> Vec<CoreFreq> {
    let base = under(root, "/sys/devices/system/cpu");
    let mut out = Vec::new();
    for entry in list_dir(&base) {
        let Some(core) = core_index(&entry) else {
            continue;
        };
        let cpufreq = base.join(&entry).join("cpufreq");
        let freq_khz = read_u32(&cpufreq.join("scaling_cur_freq"));
        let governor = read_trimmed(&cpufreq.join("scaling_governor"));
        // Skip a `cpuN` that carries no cpufreq data at all (e.g. `cpuidle`-only
        // or an offline core); a core with at least one readable field is kept.
        if freq_khz.is_none() && governor.is_none() {
            continue;
        }
        out.push(CoreFreq {
            core,
            freq_khz,
            governor,
        });
    }
    out.sort_by_key(|c| c.core);
    out
}

/// Parse `cpuN` into its index `N`. Rejects `cpufreq`, `cpuidle`, and any other
/// non-`cpu<digits>` entry under the cpu directory.
fn core_index(entry: &str) -> Option<u32> {
    let rest = entry.strip_prefix("cpu")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u32>().ok()
}

/// The cumulative jiffy counters for one CPU line in `/proc/stat`.
///
/// `total` is the sum of every field; `busy` is `total` minus the idle fields
/// (`idle` + `iowait`). Utilization over an interval is
/// `(busy_now - busy_prev) / (total_now - total_prev)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    /// Sum of all jiffy fields on the line.
    pub total: u64,
    /// Non-idle jiffies (all fields except `idle` and `iowait`).
    pub busy: u64,
}

/// A parsed `/proc/stat` snapshot: the aggregate `cpu` line plus each per-core
/// `cpuN` line, indexed by `N`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcStat {
    /// The aggregate `cpu` line.
    pub aggregate: Option<CpuTimes>,
    /// Per-core lines, keyed by the logical core index.
    pub cores: Vec<(u32, CpuTimes)>,
    /// The `ctxt` total (cumulative context switches), when present.
    pub ctxt: Option<u64>,
    /// The `processes` total (cumulative forks), when present.
    pub processes: Option<u64>,
}

/// Read and parse `/proc/stat`. Returns an empty [`ProcStat`] when the file is
/// absent (a non-Linux host); the per-line parser tolerates missing fields.
pub fn read_proc_stat(root: &Path) -> ProcStat {
    let path = under(root, "/proc/stat");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_proc_stat(&text),
        Err(_) => ProcStat::default(),
    }
}

/// Parse the text of `/proc/stat`. Split out so it is testable against a fixture
/// string. Lines that are not `cpu`/`cpuN`/`ctxt`/`processes` are ignored.
pub fn parse_proc_stat(text: &str) -> ProcStat {
    let mut stat = ProcStat::default();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        match key {
            "cpu" => stat.aggregate = parse_cpu_times(parts),
            "ctxt" => stat.ctxt = parts.next().and_then(|v| v.parse::<u64>().ok()),
            "processes" => stat.processes = parts.next().and_then(|v| v.parse::<u64>().ok()),
            _ if key.starts_with("cpu") => {
                if let Some(core) = key.strip_prefix("cpu").and_then(|n| n.parse::<u32>().ok()) {
                    if let Some(times) = parse_cpu_times(parts) {
                        stat.cores.push((core, times));
                    }
                }
            }
            _ => {}
        }
    }
    stat
}

/// Fold the jiffy fields after the `cpu`/`cpuN` key into a [`CpuTimes`].
///
/// The kernel order is user, nice, system, idle, iowait, irq, softirq, steal,
/// guest, guest_nice. Idle = `idle + iowait` (fields 3 and 4, zero-based); every
/// other field counts as busy. Missing trailing fields (older kernels) are
/// treated as zero. A line with no numeric fields yields `None`.
fn parse_cpu_times<'a>(fields: impl Iterator<Item = &'a str>) -> Option<CpuTimes> {
    let mut total: u64 = 0;
    let mut idle: u64 = 0;
    let mut any = false;
    for (i, f) in fields.enumerate() {
        let Ok(v) = f.parse::<u64>() else {
            break;
        };
        any = true;
        total = total.saturating_add(v);
        if i == 3 || i == 4 {
            idle = idle.saturating_add(v);
        }
    }
    if !any {
        return None;
    }
    Some(CpuTimes {
        total,
        busy: total.saturating_sub(idle),
    })
}

/// Compute the busy fraction (0.0..=100.0) between two cumulative samples.
///
/// Returns `None` when there is no forward progress in `total` (a stale or
/// equal pair), so a degenerate interval does not produce a misleading 0%.
pub fn util_pct(prev: CpuTimes, now: CpuTimes) -> Option<f32> {
    let dt = now.total.checked_sub(prev.total)?;
    if dt == 0 {
        return None;
    }
    let db = now.busy.saturating_sub(prev.busy);
    Some((db as f64 / dt as f64 * 100.0) as f32)
}

#[cfg(test)]
mod tests;
