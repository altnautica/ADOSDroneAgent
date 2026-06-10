//! Collector tick + run-loop tests.
//!
//! These lay down fixture sysfs/proc trees and drive the collector against an
//! injected root, so every signal class is exercised without touching the host,
//! and the async run loop is verified to emit a snapshot and stop on shutdown.

use super::helpers::{fold_throttle, sanitize};
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
