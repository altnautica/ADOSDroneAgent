//! Native heartbeat enrichment producer.
//!
//! The cloud heartbeat's deterministic native base (device identity, version,
//! board) carries no live status: no CPU/memory/disk, no FC-link state, no
//! service fleet. The frozen [`crate::heartbeat::HeartbeatPayload`] leaves those
//! fields `Option` + skip-if-none so a base-only heartbeat is honestly silent
//! about them (operating rule 37) — but the GCS then shows nothing.
//!
//! This module builds that live status **in Rust, on every heartbeat tick**, from
//! the real sources the agent already exposes, and the loop folds it over the
//! base via [`crate::heartbeat::build_payload`]. No subprocess is shelled for the
//! resource sample (psutil is not reimplemented — `/proc` is read directly), the
//! FC link is read from the same state IPC socket the router publishes, and the
//! service fleet comes from one `systemctl list-units` per tick.
//!
//! Every value is best-effort: a key whose source read fails is **omitted** from
//! the enrichment object, never asserted as a fabricated `0` / `false` /
//! `"stopped"`. The fold then leaves the corresponding heartbeat field absent
//! (honest "unknown") rather than lying. The enrichment object is camelCase to
//! match the heartbeat wire keys + the Convex `v.optional` validator surface
//! (`cpuPercent`, `memoryUsedMb`, `services`, `fcConnected`, ...).

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use ados_protocol::state::read_state_value_blocking;
use serde_json::{json, Map, Value};

/// The state IPC socket the MAVLink router publishes the vehicle snapshot to
/// (~10 Hz). The wire form is v1 newline-JSON or v2 length-prefixed msgpack; the
/// shared reader auto-detects per frame. The FC-link extras (`fc_connected`/
/// `fc_port`/`fc_baud`) ride on that snapshot. Overridable via `ADOS_STATE_SOCK`
/// for tests.
pub const STATE_SOCK: &str = "/run/ados/state.sock";

/// Max wall time to wait for one state-socket frame. The publisher streams ~10 Hz
/// so a frame lands well inside this; on timeout the FC fields are simply omitted
/// and the heartbeat never blocks on a stalled or absent router.
const STATE_READ_TIMEOUT: Duration = Duration::from_millis(200);

/// Max wall time for the `systemctl list-units` shell-out. Matches the Python
/// fallback's 3 s timeout; on a slow/absent systemctl the `services` key is
/// omitted rather than holding the tick.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(3);

/// One `/proc/stat` `cpu ` snapshot: the busy/idle split needed to compute the
/// instantaneous CPU percent as a delta against the previous tick's sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuSample {
    /// Idle + iowait jiffies.
    pub idle: u64,
    /// Sum of every field on the aggregate `cpu ` line (idle included).
    pub total: u64,
}

/// Build the native enrichment object folded over the heartbeat base each tick.
///
/// `prev_cpu` carries the previous tick's `/proc/stat` sample across calls so the
/// CPU percent is a true inter-tick delta; it is updated in place. On the very
/// first tick (`prev_cpu` is `None`) there is no delta to compute, so `cpuPercent`
/// is omitted and the sample is seeded for the next tick.
///
/// Every key is best-effort and omitted on a failed source read — see the module
/// doc. The radio block is intentionally NOT built here: the heartbeat keeps its
/// honest `RadioBlock::absent()` (radio enrichment is a separate producer).
pub fn build_native_enrichment(prev_cpu: &mut Option<CpuSample>) -> Value {
    let mut obj = Map::new();

    // ── Resources from /proc ──────────────────────────────────────────────
    fold_cpu(&mut obj, prev_cpu);
    fold_memory(&mut obj);
    fold_disk(&mut obj);
    fold_temperature(&mut obj);

    // ── FC link from the state IPC socket ─────────────────────────────────
    fold_fc(&mut obj);

    // ── Service fleet from systemctl ──────────────────────────────────────
    fold_services(&mut obj);

    Value::Object(obj)
}

/// Fold the CPU percent in from a fresh `/proc/stat` read, updating `prev` to the
/// current sample. Omits `cpuPercent` on the first tick (no prior sample) or when
/// `/proc/stat` is unreadable.
fn fold_cpu(obj: &mut Map<String, Value>, prev: &mut Option<CpuSample>) {
    let cur = match read_proc_stat_cpu() {
        Some(s) => s,
        None => return,
    };
    if let Some(prev_sample) = prev {
        if let Some(pct) = cpu_percent(prev_sample, &cur) {
            obj.insert("cpuPercent".to_string(), json!(round2(pct)));
        }
    }
    *prev = Some(cur);
}

/// Read the aggregate `cpu ` line from `/proc/stat` into a [`CpuSample`].
fn read_proc_stat_cpu() -> Option<CpuSample> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    text.lines().find_map(parse_proc_stat_cpu)
}

/// Parse one `/proc/stat` line into a [`CpuSample`] when it is the aggregate
/// `cpu ` line (the leading line, all cores summed). Returns `None` for any other
/// line (per-core `cpu0`, `intr`, ...) or a malformed one. Pure for unit testing.
///
/// The fields after `cpu` are: user nice system idle iowait irq softirq steal
/// guest guest_nice. `total` sums all present numeric fields; `idle` = idle +
/// iowait (fields 3 and 4), matching the standard busy/idle split.
pub fn parse_proc_stat_cpu(line: &str) -> Option<CpuSample> {
    let rest = line.strip_prefix("cpu ")?;
    let fields: Vec<u64> = rest
        .split_whitespace()
        .map(|f| f.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()?;
    if fields.len() < 4 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    // idle (index 3) + iowait (index 4, when present).
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some(CpuSample { idle, total })
}

/// The CPU busy percent between two samples: `100 * (1 - idle_delta/total_delta)`.
/// Returns `None` when the total did not advance (no elapsed time → undefined) or
/// the counters ran backwards (a stat reset), so the caller omits the field
/// rather than reporting a nonsense value. Pure for unit testing.
pub fn cpu_percent(prev: &CpuSample, cur: &CpuSample) -> Option<f64> {
    let total_delta = cur.total.checked_sub(prev.total)?;
    if total_delta == 0 {
        return None;
    }
    let idle_delta = cur.idle.checked_sub(prev.idle)?;
    let busy = total_delta.saturating_sub(idle_delta);
    Some(100.0 * (busy as f64) / (total_delta as f64))
}

/// Fold memory + swap stats in from `/proc/meminfo`. Omits the whole block when
/// `/proc/meminfo` is unreadable or lacks `MemTotal`.
fn fold_memory(obj: &mut Map<String, Value>) {
    let text = match std::fs::read_to_string("/proc/meminfo") {
        Ok(t) => t,
        Err(_) => return,
    };
    let info = match parse_meminfo(&text) {
        Some(i) => i,
        None => return,
    };

    obj.insert("memoryTotalMb".to_string(), json!(info.total_mb));
    obj.insert("memoryAvailableMb".to_string(), json!(info.available_mb));
    obj.insert("memoryUsedMb".to_string(), json!(info.used_mb));
    obj.insert("memoryCacheMb".to_string(), json!(info.cache_mb));
    obj.insert("memoryPercent".to_string(), json!(round2(info.percent)));

    obj.insert("swapTotalMb".to_string(), json!(info.swap_total_mb));
    obj.insert("swapUsedMb".to_string(), json!(info.swap_used_mb));
    obj.insert("swapPercent".to_string(), json!(round2(info.swap_percent)));
}

/// The derived memory + swap figures (MiB + percent) from `/proc/meminfo`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemInfo {
    pub total_mb: i64,
    pub available_mb: i64,
    pub used_mb: i64,
    pub cache_mb: i64,
    pub percent: f64,
    pub swap_total_mb: i64,
    pub swap_used_mb: i64,
    pub swap_percent: f64,
}

/// Parse `/proc/meminfo` into the derived MiB + percent figures. Returns `None`
/// when `MemTotal` is absent or zero (a meaningless sample). `used = total -
/// available`, `cache = Cached + Buffers`, `swap_used = SwapTotal - SwapFree`.
/// Values in `/proc/meminfo` are kB; converted to MiB. Pure for unit testing.
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let kb = |key: &str| -> Option<u64> {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(key)?;
            // The key is followed by ':' then the value in kB then "kB".
            let rest = rest.trim_start();
            let rest = rest.strip_prefix(':').unwrap_or(rest);
            // `split_whitespace` already skips leading whitespace.
            rest.split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
        })
    };

    let total_kb = kb("MemTotal")?;
    if total_kb == 0 {
        return None;
    }
    let available_kb = kb("MemAvailable").unwrap_or(0);
    let cached_kb = kb("Cached").unwrap_or(0);
    let buffers_kb = kb("Buffers").unwrap_or(0);
    let swap_total_kb = kb("SwapTotal").unwrap_or(0);
    let swap_free_kb = kb("SwapFree").unwrap_or(0);

    let used_kb = total_kb.saturating_sub(available_kb);
    let cache_kb = cached_kb + buffers_kb;
    let percent = 100.0 * (used_kb as f64) / (total_kb as f64);

    let swap_used_kb = swap_total_kb.saturating_sub(swap_free_kb);
    let swap_percent = if swap_total_kb > 0 {
        100.0 * (swap_used_kb as f64) / (swap_total_kb as f64)
    } else {
        0.0
    };

    Some(MemInfo {
        total_mb: kb_to_mb(total_kb),
        available_mb: kb_to_mb(available_kb),
        used_mb: kb_to_mb(used_kb),
        cache_mb: kb_to_mb(cache_kb),
        percent,
        swap_total_mb: kb_to_mb(swap_total_kb),
        swap_used_mb: kb_to_mb(swap_used_kb),
        swap_percent,
    })
}

/// Fold the root-filesystem usage in from `df -kP /`. Omits the block when `df`
/// is absent / errors / unparseable — a portable statvfs without a new direct
/// crate dependency (`df` is in coreutils on every target board). `df -kP`'s
/// POSIX output is one stable-format data line: `Filesystem 1024-blocks Used
/// Available Capacity Mounted-on`.
fn fold_disk(obj: &mut Map<String, Value>) {
    let out = match run_with_timeout("df", &["-kP", "/"], SYSTEMCTL_TIMEOUT) {
        Some(o) => o,
        None => return,
    };
    let disk = match parse_df_root(&out) {
        Some(d) => d,
        None => return,
    };
    obj.insert("diskTotalGb".to_string(), json!(round2(disk.total_gb)));
    obj.insert("diskUsedGb".to_string(), json!(round2(disk.used_gb)));
    obj.insert("diskPercent".to_string(), json!(round2(disk.percent)));
}

/// The derived root-filesystem figures (GiB + percent) from `df -kP`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskInfo {
    pub total_gb: f64,
    pub used_gb: f64,
    pub percent: f64,
}

/// Parse the `df -kP /` output (a header line then one data line in 1024-byte
/// blocks) into GiB + percent. `percent = 100 * used/total`. Returns `None` when
/// no data line parses or the total is zero. Pure for unit testing.
pub fn parse_df_root(out: &str) -> Option<DiskInfo> {
    // Skip the header; take the first data line whose blocks/used parse.
    out.lines().skip(1).find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // Filesystem 1024-blocks Used Available Capacity Mounted-on
        if cols.len() < 6 {
            return None;
        }
        let total_kb = cols[1].parse::<u64>().ok()?;
        let used_kb = cols[2].parse::<u64>().ok()?;
        if total_kb == 0 {
            return None;
        }
        let total_gb = kib_to_gib(total_kb);
        let used_gb = kib_to_gib(used_kb);
        let percent = 100.0 * (used_kb as f64) / (total_kb as f64);
        Some(DiskInfo {
            total_gb,
            used_gb,
            percent,
        })
    })
}

/// Fold the SoC temperature in from `thermal_zone0` (millidegrees → °C). Omits
/// the field when the sysfs node is absent or unparseable.
fn fold_temperature(obj: &mut Map<String, Value>) {
    let text = match std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        Ok(t) => t,
        Err(_) => return,
    };
    if let Some(c) = parse_thermal_millidegrees(&text) {
        obj.insert("temperature".to_string(), json!(round2(c)));
    }
}

/// Parse a thermal-zone millidegree reading into °C. Pure for unit testing.
pub fn parse_thermal_millidegrees(text: &str) -> Option<f64> {
    let milli = text.trim().parse::<i64>().ok()?;
    Some(milli as f64 / 1000.0)
}

/// Fold the FC-link fields in from one bounded read of the state IPC socket. The
/// router publishes the vehicle snapshot with `fc_connected` / `fc_port` /
/// `fc_baud` extras; this connects, reads one snapshot frame, and lifts those
/// fields. On a timeout / absent socket / parse error the fields are omitted.
fn fold_fc(obj: &mut Map<String, Value>) {
    let snap = match read_state_snapshot() {
        Some(v) => v,
        None => return,
    };
    let map = match snap.as_object() {
        Some(m) => m,
        None => return,
    };

    if let Some(connected) = map.get("fc_connected").and_then(Value::as_bool) {
        obj.insert("fcConnected".to_string(), json!(connected));
    }
    if let Some(port) = map.get("fc_port").and_then(Value::as_str) {
        if !port.is_empty() {
            obj.insert("fcPort".to_string(), json!(port));
        }
    }
    if let Some(baud) = map.get("fc_baud").and_then(Value::as_i64) {
        if baud > 0 {
            obj.insert("fcBaud".to_string(), json!(baud));
        }
    }
    // The gated-truth detail behind fcConnected: transport-open vs alive, the
    // heartbeat age, the configured transport class, and the not-alive hint. The
    // GCS already renders these on the LAN path; carry them over cloud relay too
    // so a cloud-paired drone can show "port open · no MAVLink" + the hint rather
    // than only a connected boolean.
    if let Some(open) = map.get("transport_open").and_then(Value::as_bool) {
        obj.insert("transportOpen".to_string(), json!(open));
    }
    if let Some(alive) = map.get("mavlink_alive").and_then(Value::as_bool) {
        obj.insert("mavlinkAlive".to_string(), json!(alive));
    }
    if let Some(age) = map.get("heartbeat_age_s").and_then(Value::as_f64) {
        obj.insert("heartbeatAgeS".to_string(), json!(age));
    }
    if let Some(source) = map.get("fc_source").and_then(Value::as_str) {
        if !source.is_empty() {
            obj.insert("fcSource".to_string(), json!(source));
        }
    }
    if let Some(hint) = map.get("fc_link_hint").and_then(Value::as_str) {
        if !hint.is_empty() {
            obj.insert("fcLinkHint".to_string(), json!(hint));
        }
    }
}

/// Read one state snapshot from the state IPC socket. The state socket replays
/// its last buffer on connect, so a single frame arrives immediately; the read
/// is wall-bounded by [`STATE_READ_TIMEOUT`] so a stalled or absent router never
/// holds the heartbeat tick. The shared reader auto-detects the state wire form
/// per frame (v1 newline-JSON or v2 length-prefixed msgpack), so this reads a v2
/// snapshot as well — the installer ships v2, and a v1-only read silently yields
/// nothing against it.
fn read_state_snapshot() -> Option<Value> {
    let path = std::env::var("ADOS_STATE_SOCK").unwrap_or_else(|_| STATE_SOCK.to_string());
    let mut stream = UnixStream::connect(Path::new(&path)).ok()?;
    stream.set_read_timeout(Some(STATE_READ_TIMEOUT)).ok()?;

    // One frame off the replayed buffer via the shared blocking reader. Any
    // timeout / clean EOF / framing error collapses to `None` so a stalled or
    // absent router never holds the tick (the same tolerance the byte-loop had).
    read_state_value_blocking(&mut stream).ok().flatten()
}

/// Fold the `ados-*` service fleet in from one `systemctl list-units`. Omits the
/// `services` key when systemctl is absent / errors. Each entry is the Convex
/// service-object shape (`name`/`status`/`uptimeSeconds`/`memoryMb`).
fn fold_services(obj: &mut Map<String, Value>) {
    let out = match run_with_timeout(
        "systemctl",
        &[
            "list-units",
            "--type=service",
            "--all",
            "--no-pager",
            "--no-legend",
            "ados-*.service",
        ],
        SYSTEMCTL_TIMEOUT,
    ) {
        Some(o) => o,
        None => return,
    };
    let services = parse_systemctl_units(&out);
    obj.insert("services".to_string(), Value::Array(services));
}

/// Parse `systemctl list-units --no-legend` output into the heartbeat service
/// objects. Columns are `UNIT LOAD ACTIVE SUB DESCRIPTION`; the name is the unit
/// minus `.service`, the status is `running` when SUB is `running` else the SUB
/// verbatim (matching the Python `_systemd_services_fallback`). `uptimeSeconds`
/// and `memoryMb` are emitted as zero — the cloud relay does not have the
/// per-unit accounting the API process does, and the Convex validator accepts the
/// keys as optional with these defaults. Pure for unit testing.
pub fn parse_systemctl_units(out: &str) -> Vec<Value> {
    let mut services = Vec::new();
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        // A failed/dead unit gets a `●`/`*` lead glyph as its own column, which
        // shifts UNIT/LOAD/ACTIVE/SUB by one. Detect it by a column-0 token that
        // is not itself the `.service` unit, then index past it.
        let offset = if cols[0].ends_with(".service") { 0 } else { 1 };
        let unit = match cols.get(offset) {
            Some(u) => u.trim_start_matches(['●', '*']).trim(),
            None => continue,
        };
        if !unit.ends_with(".service") {
            continue;
        }
        // Columns from the unit: UNIT(+0) LOAD(+1) ACTIVE(+2) SUB(+3).
        let sub = match cols.get(offset + 3) {
            Some(s) => s.trim(),
            None => continue,
        };
        let name = &unit[..unit.len() - ".service".len()];
        let status = if sub == "running" { "running" } else { sub };
        services.push(json!({
            "name": name,
            "status": status,
            "uptimeSeconds": 0,
            "memoryMb": 0.0,
        }));
    }
    services
}

/// Round a float to two decimals so the wire carries `12.34`, not the full f64
/// expansion (matches the Python loop's `round(x, 2)` discipline).
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Convert kB (the `/proc/meminfo` unit) to MiB (rounded), matching the Python
/// heartbeat's `// (1024 * 1024)`-style MiB figures derived from bytes. meminfo
/// is in kibibytes, so MiB = kB / 1024.
fn kb_to_mb(kb: u64) -> i64 {
    (kb / 1024) as i64
}

/// Convert 1024-byte blocks (the `df -kP` unit) to GiB.
fn kib_to_gib(kib: u64) -> f64 {
    (kib as f64) / (1024.0 * 1024.0)
}

/// Run a command with a wall-clock timeout, returning its stdout as a string on a
/// clean exit (status 0). The child is reaped via a bounded wait loop; a child
/// that overruns the budget is killed and the call returns `None`. Best-effort:
/// any spawn / wait error yields `None` so the caller omits the dependent keys.
fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proc_stat_cpu_reads_the_aggregate_line() {
        // Aggregate line: user nice system idle iowait irq softirq steal ...
        let s = parse_proc_stat_cpu("cpu  100 0 50 800 50 0 0 0 0 0").unwrap();
        // total = 100+0+50+800+50 = 1000; idle = 800 + 50 (iowait) = 850.
        assert_eq!(s.total, 1000);
        assert_eq!(s.idle, 850);
        // A per-core line is not the aggregate and is rejected.
        assert!(parse_proc_stat_cpu("cpu0 1 2 3 4 5").is_none());
        // A non-cpu line is rejected.
        assert!(parse_proc_stat_cpu("intr 12345 0 0").is_none());
        // Exactly four fields (user nice system idle, no iowait) is the minimum
        // valid aggregate line — idle is index 3.
        let four = parse_proc_stat_cpu("cpu 1 2 3 4").unwrap();
        assert_eq!(four.total, 10);
        assert_eq!(four.idle, 4); // idle only; no iowait field present.
                                  // Fewer than four fields cannot supply the idle slot → rejected.
        assert!(parse_proc_stat_cpu("cpu 1 2 3").is_none());
        assert!(parse_proc_stat_cpu("cpu 1 2").is_none());
    }

    #[test]
    fn cpu_percent_computes_the_inter_tick_delta() {
        // Between the two samples: total advanced 1000, idle advanced 800 →
        // busy 200 → 20%.
        let prev = CpuSample {
            idle: 800,
            total: 1000,
        };
        let cur = CpuSample {
            idle: 1600,
            total: 2000,
        };
        let pct = cpu_percent(&prev, &cur).unwrap();
        assert!((pct - 20.0).abs() < 1e-9, "expected 20%, got {pct}");

        // A fully-busy interval: idle did not advance → 100%.
        let busy = CpuSample {
            idle: 800,
            total: 2000,
        };
        assert!((cpu_percent(&prev, &busy).unwrap() - 100.0).abs() < 1e-9);

        // No elapsed time (total unchanged) → None (undefined, omit the field).
        assert!(cpu_percent(&prev, &prev).is_none());

        // Counters ran backwards (a stat reset) → None.
        let backwards = CpuSample {
            idle: 0,
            total: 500,
        };
        assert!(cpu_percent(&prev, &backwards).is_none());
    }

    #[test]
    fn parse_meminfo_derives_used_and_percent() {
        // 4 GiB total, 1 GiB available → 3 GiB used → 75%.
        let text = "\
MemTotal:        4194304 kB
MemFree:          524288 kB
MemAvailable:    1048576 kB
Buffers:          102400 kB
Cached:           409600 kB
SwapTotal:       2097152 kB
SwapFree:        1048576 kB
";
        let m = parse_meminfo(text).unwrap();
        assert_eq!(m.total_mb, 4096);
        assert_eq!(m.available_mb, 1024);
        assert_eq!(m.used_mb, 3072);
        // cache = Cached + Buffers = 409600 + 102400 = 512000 kB = 500 MiB.
        assert_eq!(m.cache_mb, 500);
        assert!((m.percent - 75.0).abs() < 1e-9);
        // Swap: 2 GiB total, 1 GiB free → 1 GiB used → 50%.
        assert_eq!(m.swap_total_mb, 2048);
        assert_eq!(m.swap_used_mb, 1024);
        assert!((m.swap_percent - 50.0).abs() < 1e-9);
    }

    #[test]
    fn parse_meminfo_handles_no_swap_and_rejects_zero_total() {
        let text = "MemTotal: 1048576 kB\nMemAvailable: 524288 kB\n";
        let m = parse_meminfo(text).unwrap();
        assert_eq!(m.total_mb, 1024);
        assert_eq!(m.used_mb, 512);
        // No swap lines → zero swap, 0% (never a divide-by-zero).
        assert_eq!(m.swap_total_mb, 0);
        assert_eq!(m.swap_used_mb, 0);
        assert_eq!(m.swap_percent, 0.0);
        // A zero or absent MemTotal is a meaningless sample → None.
        assert!(parse_meminfo("MemTotal: 0 kB\n").is_none());
        assert!(parse_meminfo("MemFree: 100 kB\n").is_none());
    }

    #[test]
    fn parse_df_root_reads_blocks_and_percent() {
        let out = "\
Filesystem     1024-blocks    Used Available Capacity Mounted on
/dev/root         62914560 6291456  56623104       10% /
";
        let d = parse_df_root(out).unwrap();
        // 62914560 KiB = 60 GiB total; 6291456 KiB = 6 GiB used; 10%.
        assert!((d.total_gb - 60.0).abs() < 1e-6, "total {}", d.total_gb);
        assert!((d.used_gb - 6.0).abs() < 1e-6, "used {}", d.used_gb);
        assert!((d.percent - 10.0).abs() < 1e-9, "percent {}", d.percent);
        // No data line → None.
        assert!(
            parse_df_root("Filesystem 1024-blocks Used Available Capacity Mounted on\n").is_none()
        );
    }

    #[test]
    fn parse_thermal_millidegrees_converts() {
        assert!((parse_thermal_millidegrees("47500\n").unwrap() - 47.5).abs() < 1e-9);
        assert!((parse_thermal_millidegrees("  52000 ").unwrap() - 52.0).abs() < 1e-9);
        assert!(parse_thermal_millidegrees("not-a-number").is_none());
    }

    #[test]
    fn parse_systemctl_units_lifts_name_and_status() {
        // The --no-legend output: UNIT LOAD ACTIVE SUB DESCRIPTION.
        let out = "\
ados-supervisor.service loaded active running ADOS process supervisor
ados-video.service      loaded active running ADOS video pipeline
ados-cloud.service      loaded inactive dead   ADOS cloud relay
";
        let svcs = parse_systemctl_units(out);
        assert_eq!(svcs.len(), 3);
        assert_eq!(svcs[0]["name"], "ados-supervisor");
        assert_eq!(svcs[0]["status"], "running");
        assert_eq!(svcs[0]["uptimeSeconds"], 0);
        assert_eq!(svcs[0]["memoryMb"], 0.0);
        // A non-running SUB carries through verbatim, not "running".
        assert_eq!(svcs[2]["name"], "ados-cloud");
        assert_eq!(svcs[2]["status"], "dead");
    }

    #[test]
    fn parse_systemctl_units_tolerates_lead_glyph_and_blank_lines() {
        // A failed unit gets a `●` lead glyph; a blank line is skipped.
        let out = "\
● ados-net.service loaded failed failed ADOS uplink

ados-mavlink-router.service loaded active running ADOS MAVLink router
";
        let svcs = parse_systemctl_units(out);
        assert_eq!(svcs.len(), 2);
        assert_eq!(svcs[0]["name"], "ados-net");
        assert_eq!(svcs[0]["status"], "failed");
        assert_eq!(svcs[1]["name"], "ados-mavlink-router");
        assert_eq!(svcs[1]["status"], "running");
    }

    #[test]
    fn build_native_enrichment_omits_cpu_on_first_tick_and_is_an_object() {
        // The first call has no prior sample, so cpuPercent is omitted; the
        // sample is seeded for the next tick. The result is always an object.
        let mut prev: Option<CpuSample> = None;
        let v = build_native_enrichment(&mut prev);
        assert!(v.is_object());
        let obj = v.as_object().unwrap();
        assert!(
            !obj.contains_key("cpuPercent"),
            "first tick has no delta to report"
        );
        // The CPU sample is seeded (on a host with /proc/stat) so the next tick
        // can compute a delta. On a non-Linux dev host /proc/stat is absent and
        // prev stays None, which is fine — the field is simply never folded.
        if Path::new("/proc/stat").exists() {
            assert!(prev.is_some(), "the sample is seeded for the next tick");
        }
        // No key is ever a JSON null — every value is a real reading or omitted.
        for (k, val) in obj {
            assert!(!val.is_null(), "{k} must be a real value or omitted");
        }
    }

    #[test]
    fn read_state_snapshot_lifts_fc_fields_from_a_socket() {
        // Stand up a one-shot Unix socket that replays a state frame, point the
        // enrichment at it, and assert the FC fields fold in. Exercise BOTH wire
        // forms — v1 newline-JSON and v2 length-prefixed msgpack — because the
        // shared reader auto-detects per frame and the installer ships v2 (a
        // v1-only read would silently lift nothing against a v2 producer).
        let snap = json!({
            "armed": false,
            "fc_connected": true,
            "fc_port": "/dev/ttyACM0",
            "fc_baud": 115200,
            "transport_open": true,
            "mavlink_alive": true,
            "heartbeat_age_s": 0.5,
            "fc_source": "serial",
            "fc_link_hint": "none"
        });

        // Serve `wire` bytes on a fresh socket and return the folded enrichment.
        let fold_over_wire = |tag: &str, wire: Vec<u8>| -> Map<String, Value> {
            let dir = std::env::temp_dir();
            let sock = dir.join(format!(
                "ados-enrich-state-{}-{}.sock",
                tag,
                std::process::id()
            ));
            let _ = std::fs::remove_file(&sock);
            let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
            let sock_for_thread = sock.clone();
            let handle = std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    use std::io::Write;
                    let _ = stream.write_all(&wire);
                    let _ = stream.flush();
                }
                let _ = std::fs::remove_file(&sock_for_thread);
            });

            std::env::set_var("ADOS_STATE_SOCK", &sock);
            let mut obj = Map::new();
            fold_fc(&mut obj);
            std::env::remove_var("ADOS_STATE_SOCK");
            let _ = handle.join();
            obj
        };

        let assert_fc = |obj: &Map<String, Value>| {
            assert_eq!(obj.get("fcConnected"), Some(&json!(true)));
            assert_eq!(obj.get("fcPort"), Some(&json!("/dev/ttyACM0")));
            assert_eq!(obj.get("fcBaud"), Some(&json!(115200)));
            // The gated-truth detail lifts alongside the connection triple.
            assert_eq!(obj.get("transportOpen"), Some(&json!(true)));
            assert_eq!(obj.get("mavlinkAlive"), Some(&json!(true)));
            assert_eq!(obj.get("heartbeatAgeS"), Some(&json!(0.5)));
            assert_eq!(obj.get("fcSource"), Some(&json!("serial")));
            assert_eq!(obj.get("fcLinkHint"), Some(&json!("none")));
        };

        // v1 newline-JSON.
        let v1 = ados_protocol::state::encode_v1(&snap).unwrap();
        assert_fc(&fold_over_wire("v1", v1));

        // v2 length-prefixed msgpack (the shipped wire form).
        let v2 = ados_protocol::state::encode_v2(&snap).unwrap();
        assert_fc(&fold_over_wire("v2", v2));
    }

    #[test]
    fn fold_fc_omits_everything_when_the_socket_is_absent() {
        std::env::set_var("ADOS_STATE_SOCK", "/nonexistent/ados/state.sock");
        let mut obj = Map::new();
        fold_fc(&mut obj);
        std::env::remove_var("ADOS_STATE_SOCK");
        assert!(obj.is_empty(), "no FC fields without a state socket");
    }
}
