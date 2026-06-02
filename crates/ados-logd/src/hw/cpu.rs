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
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn reads_per_core_freq_and_governor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq",
            "1416000\n",
        );
        write(
            root,
            "sys/devices/system/cpu/cpu0/cpufreq/scaling_governor",
            "schedutil\n",
        );
        write(
            root,
            "sys/devices/system/cpu/cpu1/cpufreq/scaling_cur_freq",
            "600000\n",
        );
        write(
            root,
            "sys/devices/system/cpu/cpu1/cpufreq/scaling_governor",
            "powersave\n",
        );
        // A non-core directory the cpu/ tree carries.
        write(
            root,
            "sys/devices/system/cpu/cpuidle/current_driver",
            "menu\n",
        );

        let cores = read_cpufreq(root);
        assert_eq!(cores.len(), 2);
        assert_eq!(cores[0].core, 0);
        assert_eq!(cores[0].freq_khz, Some(1_416_000));
        assert_eq!(cores[0].governor.as_deref(), Some("schedutil"));
        assert_eq!(cores[1].core, 1);
        assert_eq!(cores[1].freq_khz, Some(600_000));
        assert_eq!(cores[1].governor.as_deref(), Some("powersave"));
    }

    #[test]
    fn core_without_cpufreq_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // cpu0 exists but has no cpufreq subtree at all.
        write(root, "sys/devices/system/cpu/cpu0/online", "1\n");
        assert!(read_cpufreq(root).is_empty());
    }

    #[test]
    fn core_index_matches_only_cpu_digits() {
        assert_eq!(core_index("cpu0"), Some(0));
        assert_eq!(core_index("cpu11"), Some(11));
        assert_eq!(core_index("cpufreq"), None);
        assert_eq!(core_index("cpuidle"), None);
        assert_eq!(core_index("cpu"), None);
    }

    #[test]
    fn parses_proc_stat_aggregate_and_per_core() {
        // Realistic /proc/stat head. The aggregate line is the sum of the cores.
        let text = "\
cpu  100 0 50 1000 20 0 5 0 0 0
cpu0 60 0 30 600 10 0 3 0 0 0
cpu1 40 0 20 400 10 0 2 0 0 0
intr 12345
ctxt 987654
btime 1700000000
processes 4321
procs_running 2
procs_blocked 0
";
        let stat = parse_proc_stat(text);
        // Aggregate: total = 100+50+1000+20+5 = 1175; idle = 1000+20 = 1020;
        // busy = 1175-1020 = 155.
        let agg = stat.aggregate.unwrap();
        assert_eq!(agg.total, 1175);
        assert_eq!(agg.busy, 155);
        assert_eq!(stat.cores.len(), 2);
        assert_eq!(stat.cores[0].0, 0);
        // cpu0: total = 60+30+600+10+3 = 703; idle = 600+10 = 610; busy = 93.
        assert_eq!(stat.cores[0].1.total, 703);
        assert_eq!(stat.cores[0].1.busy, 93);
        assert_eq!(stat.ctxt, Some(987_654));
        assert_eq!(stat.processes, Some(4321));
    }

    #[test]
    fn read_proc_stat_is_empty_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let stat = read_proc_stat(dir.path());
        assert_eq!(stat, ProcStat::default());
    }

    #[test]
    fn util_pct_is_the_busy_fraction_of_the_delta() {
        let prev = CpuTimes {
            total: 1000,
            busy: 200,
        };
        // 100 more total, 50 of it busy -> 50%.
        let now = CpuTimes {
            total: 1100,
            busy: 250,
        };
        let pct = util_pct(prev, now).unwrap();
        assert!((pct - 50.0).abs() < 0.001, "got {pct}");
    }

    #[test]
    fn util_pct_is_none_for_a_zero_or_backward_delta() {
        let s = CpuTimes {
            total: 1000,
            busy: 200,
        };
        // No forward progress.
        assert_eq!(util_pct(s, s), None);
        // A counter that went backward (counter reset) yields no value.
        let earlier = CpuTimes {
            total: 900,
            busy: 100,
        };
        assert_eq!(util_pct(s, earlier), None);
    }

    #[test]
    fn cpu_times_tolerates_short_lines_on_older_kernels() {
        // user nice system idle only (no iowait/irq/...): total = 1+0+2+10 = 13,
        // idle = 10 (field 3), iowait absent -> busy = 3.
        let stat = parse_proc_stat("cpu 1 0 2 10\n");
        let agg = stat.aggregate.unwrap();
        assert_eq!(agg.total, 13);
        assert_eq!(agg.busy, 3);
    }
}
