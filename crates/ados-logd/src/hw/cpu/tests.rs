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
