//! Native heartbeat enrichment producer.
//!
//! The cloud heartbeat's deterministic native base (device identity, version,
//! board) carries no live status: no CPU/memory/disk, no FC-link state, no
//! service fleet. The frozen [`crate::heartbeat::HeartbeatPayload`] leaves those
//! fields `Option` + skip-if-none so a base-only heartbeat is honestly silent
//! about them — but the GCS then shows nothing.
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

mod fc_services;
use fc_services::*;
pub use fc_services::{parse_systemctl_units, service_status};

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
/// doc. The radio block is not built here: it is derived in the heartbeat loop
/// itself (`loops::sidecars::read_radio_sidecar`) from the WFB stats sidecar,
/// through the same `ados_protocol::wfb_status` derivation the LAN routes use.
/// This comment used to name "a separate producer" that did not exist, which is
/// why the block shipped absent on every cloud tick.
pub fn build_native_enrichment(prev_cpu: &mut Option<CpuSample>) -> Value {
    build_native_enrichment_with(prev_cpu, true)
}

/// [`build_native_enrichment`] with the service fleet made optional.
///
/// Every other source here is a file read; the service fleet is a `systemctl`
/// fork with a multi-second timeout, which is by far the most expensive part of
/// a tick. A caller sampling faster than the heartbeat does (the auxiliary-lane
/// status producer runs at 1 Hz by default, five times the heartbeat rate) would
/// otherwise fork `systemctl` every second on a small board for a fleet that
/// changes minute to minute.
///
/// Passing `false` omits the `services` key entirely rather than emitting an
/// empty list, so a caller that skipped the sample is indistinguishable from one
/// whose `systemctl` failed: both mean "not measured this tick", and neither may
/// be read as "no services".
pub fn build_native_enrichment_with(prev_cpu: &mut Option<CpuSample>, services: bool) -> Value {
    let mut obj = Map::new();

    // ── Resources from /proc ──────────────────────────────────────────────
    fold_cpu(&mut obj, prev_cpu);
    fold_memory(&mut obj);
    fold_disk(&mut obj);
    fold_temperature(&mut obj);

    // ── FC link from the state IPC socket ─────────────────────────────────
    fold_fc(&mut obj);

    // ── Service fleet from systemctl ──────────────────────────────────────
    if services {
        fold_services(&mut obj);
    }

    Value::Object(obj)
}

/// Fold the CPU percent in from a fresh `/proc/stat` read, updating `prev` to the
/// current sample. Omits `cpuPercent` on the first tick (no prior sample) or when
/// `/proc/stat` is unreadable.
fn fold_cpu(obj: &mut Map<String, Value>, prev: &mut Option<CpuSample>) {
    // The logical CPU count this process can schedule on, a measured host fact.
    if let Ok(n) = std::thread::available_parallelism() {
        obj.insert("cpuCores".to_string(), json!(n.get()));
    }
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
    // The host RAM the kernel reports, the same measured figure the fleet view
    // labels as the board's RAM.
    obj.insert("boardRamMb".to_string(), json!(info.total_mb));
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

/// Round a float to two decimals so the wire carries `12.34`, not the full f64
/// expansion (the contract carries values rounded to two places).
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
mod tests;
