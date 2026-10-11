//! `GET /api/v1/diagnostics` — the composite triage snapshot.
//!
//! Byte-faithful to the FastAPI `diagnostics.get_diagnostics`: one read-mostly
//! object the LCD Diagnostics drilldown + the Mission Control remote-display pane
//! poll, composing six sections so the operator can triage from the panel without
//! a phone or laptop:
//!
//! * `agent` — version + uptime + this daemon's process metrics.
//! * `board` — the HAL board summary (`name` / `soc` / `arch` / `ram_total_mb`),
//!   projected from the board sidecar the detector persists.
//! * `system` — CPU / RAM / disk / temperature / load-average, sourced from the
//!   durable logging store's merged hardware snapshot (the one Rust collector),
//!   mapped to the diagnostics field names.
//! * `network` — the primary IPv4 + the ethernet/wlan MAC reads.
//! * `device` — the node's resolved `device_id`.
//! * `logs.agent` — the last few `ados-agent` log lines.
//!
//! Every section is fault-tolerant: an absent store / sidecar / config / `/sys`
//! file degrades that section to the same default the FastAPI route emits when its
//! own source is unavailable (the board summary's `"--"` / `"unknown"` / `0`, the
//! system block's nulls, a `null` IP/MAC, the device `"--"`), never a 500. The
//! route is guaranteed-200.
//!
//! The FastAPI route's `system` block is the `derive_resources` projection (the
//! `temperature` primary zone, the `memory_*` MB ints, the `disk_*` GB floats, the
//! 1/5/15 load average), so the native maps the same merged signals to those names
//! and reads the load average straight from `/proc/loadavg` (the same value
//! `os.getloadavg()` reports) — there is no psutil to fall back to, so a store
//! miss degrades the block to nulls rather than a live probe.
//!
//! The live readings (uptime, the process metrics, the CPU/RAM/disk/temp numbers,
//! the load average, the IP/MAC reads, and the log lines + their timestamps) drift
//! between two reads, so the conformance diff masks them; the stable contract is
//! the nested shape — every section + its keys present.

use std::path::Path;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::state::AppState;

mod video;
pub use video::*;

const BYTES_PER_MB: f64 = 1024.0 * 1024.0;
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// `GET /api/v1/diagnostics`. Guaranteed 200: every section degrades to its
/// FastAPI default rather than failing when its source is unavailable.
///
/// The FastAPI route wraps the body in a 1-second TTL cache so concurrent LCD +
/// GCS polls do not fan out to journalctl + psutil; that is a cost optimisation,
/// not part of the contract, so the native composes fresh each call.
pub async fn get_diagnostics(State(state): State<AppState>) -> Json<Value> {
    // The log store ships off, so on a stock node the store has no signals; the
    // host read is the same fallback `/api/status/full` takes. The degraded
    // all-null shape is left for a host that answers neither.
    let signals = match state.logd.latest_hw_signals().await {
        Some(s) => Some(s),
        None => {
            let local = crate::hw_local::collect_signals();
            (!local.is_empty()).then_some(local)
        }
    };

    Json(json!({
        "agent": collect_agent(&state),
        "board": collect_board(&state.board_path),
        "system": collect_system(signals.as_ref()),
        "network": collect_network(),
        "device": collect_device(&state.pairing_paths.config),
        "logs": { "agent": collect_logs(&state.logd).await },
    }))
}

// ---------------------------------------------------------------------------
// agent — version + uptime + process metrics.
// ---------------------------------------------------------------------------

/// The agent identity + this daemon's process metrics, mirroring the FastAPI
/// `_collect_agent`: the resolved version, the runtime uptime (the daemon's own
/// process uptime, the same fallback the Python route lands on when the runtime
/// has no tracked value), and the per-process CPU + RSS.
///
/// The process metrics come from `/proc/self`: RSS from `/proc/self/statm`,
/// reported in MB rounded to one decimal like the Python `memory_info().rss / MiB`
/// read. `process_cpu_percent` is `null`: a single-shot read has no prior sample
/// to diff, and a fixed `0.0` would read as an idle process. A read miss degrades
/// the memory metric to `null`.
fn collect_agent(state: &AppState) -> Value {
    let (cpu, mem) = process_metrics();
    json!({
        "version": state.agent_version(),
        "uptime_seconds": state.process_uptime_seconds(),
        "process_cpu_percent": cpu,
        "process_memory_mb": mem,
    })
}

/// This daemon's `(process_cpu_percent, process_memory_mb)`. RSS is read from
/// `/proc/self/statm` (the resident set in pages × the page size, converted to MB
/// and rounded to one decimal), matching the Python `proc.memory_info().rss`
/// conversion. CPU is `null`: a utilisation needs two samples over an interval
/// and this one-shot read takes none, so a fixed `0.0` would be a fabricated
/// reading. A `statm` read miss degrades the memory metric to `null`.
fn process_metrics() -> (Value, Value) {
    let cpu = Value::Null;
    let mem = read_self_rss_mb().map(Value::from).unwrap_or(Value::Null);
    (cpu, mem)
}

/// Resident set size of this process in MB, rounded to one decimal, from
/// `/proc/self/statm` (`size resident shared ...`, in pages). `None` when the file
/// is absent / unreadable / malformed (a non-Linux host or a sandbox). The page
/// size is `sysconf(_SC_PAGESIZE)`, 4096 on the supported boards.
#[cfg(target_os = "linux")]
fn read_self_rss_mb() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: f64 = text.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = page_size_bytes();
    Some(round1(resident_pages * page_size / BYTES_PER_MB))
}

#[cfg(not(target_os = "linux"))]
fn read_self_rss_mb() -> Option<f64> {
    None
}

/// The OS page size in bytes, via `sysconf(_SC_PAGESIZE)`, used to convert the
/// `/proc/self/statm` page count to bytes. Falls back to the 4 KiB default the
/// supported boards use when the query is unavailable.
#[cfg(target_os = "linux")]
fn page_size_bytes() -> f64 {
    // SAFETY: `sysconf` is a pure read of a system parameter with no side effects.
    let v = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) };
    if v > 0 {
        v as f64
    } else {
        4096.0
    }
}

// ---------------------------------------------------------------------------
// board — the HAL summary.
// ---------------------------------------------------------------------------

/// The board summary, mirroring the FastAPI `_collect_board`: `name` (or `"--"`),
/// `soc` (or `"unknown"`), `arch` (or `"unknown"`), and `ram_total_mb` (`ram_mb`
/// as an int, or `0`). Projected from the full board dict the detector persists to
/// the board sidecar — the same `name` / `soc` / `arch` / `ram_mb` fields the
/// FastAPI `detect_board()` returns. An absent / unreadable / non-object sidecar
/// degrades every field to its default, the same shape the FastAPI route emits
/// when its own `detect_board()` raises.
fn collect_board(board_path: &Path) -> Value {
    let board = crate::routes::status::read_board(board_path);
    let obj = board.as_object();

    let name = obj
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("--")
        .to_string();
    let soc = obj
        .and_then(|m| m.get("soc"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let arch = obj
        .and_then(|m| m.get("arch"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let ram_total_mb = obj
        .and_then(|m| m.get("ram_mb"))
        .and_then(Value::as_i64)
        .unwrap_or(0);

    json!({
        "name": name,
        "soc": soc,
        "arch": arch,
        "ram_total_mb": ram_total_mb,
    })
}

// ---------------------------------------------------------------------------
// system — CPU / RAM / disk / temp / load average.
// ---------------------------------------------------------------------------

/// The system block, mirroring the FastAPI `_collect_system`: the hardware
/// signals (the log store's when it runs, else the host's own read through
/// [`crate::hw_local`]) mapped to the diagnostics field set (`cpu_percent`,
/// `memory_used_mb`, `memory_total_mb`, `disk_used_gb`, `disk_total_gb`,
/// `temp_c`, `load_avg`). When neither source yields the essentials the block
/// degrades to the all-null shape, the same contract `/api/system` and
/// `/api/status/full` follow.
///
/// `memory_used_mb` / `memory_total_mb` are ints (the Python `int(r[...])` cast on
/// the rounded MB), `disk_used_gb` / `disk_total_gb` are one-decimal floats, and
/// `temp_c` is the primary thermal zone (`thermal.primary_c`); when that signal is
/// absent the native reads `/sys/class/thermal/thermal_zone0/temp` directly, the
/// same sysfs fallback the Python `_read_cpu_temp` lands on. `load_avg` is the
/// 1/5/15 average read straight from `/proc/loadavg` (the value
/// `os.getloadavg()` reports), each rounded to two decimals, or null when the
/// host has no load average to read.
fn collect_system(signals: Option<&Map<String, Value>>) -> Value {
    match signals.and_then(derive_system) {
        Some(body) => body,
        None => degraded_system(),
    }
}

/// Map the merged hardware signals to the diagnostics `system` shape, or `None`
/// when an essential field is missing (memory total + available, aggregate CPU,
/// filesystem total + used — the same essential set `derive_resources` requires
/// before it returns a body).
fn derive_system(signals: &Map<String, Value>) -> Option<Value> {
    let total = signal_num(signals, "mem.total_bytes")?;
    let avail = signal_num(signals, "mem.avail_bytes")?;
    let cpu = signal_num(signals, "cpu.util.all")?;
    let disk_total = signal_num(signals, "disk.fs_total_bytes")?;
    let disk_used = signal_num(signals, "disk.fs_used_bytes")?;

    let used = (total - avail).max(0.0);

    // The primary thermal zone, falling back to the thermal-zone0 sysfs read the
    // Python `_read_cpu_temp` lands on when the store has not sampled a temp.
    let temp_c = signal_num(signals, "thermal.primary_c")
        .map(round1)
        .map(Value::from)
        .unwrap_or_else(|| {
            read_cpu_temp_sysfs()
                .map(Value::from)
                .unwrap_or(Value::Null)
        });

    Some(json!({
        "cpu_percent": round1(cpu),
        "memory_used_mb": round_int(used / BYTES_PER_MB),
        "memory_total_mb": round_int(total / BYTES_PER_MB),
        "disk_used_gb": round1(disk_used / BYTES_PER_GB),
        "disk_total_gb": round1(disk_total / BYTES_PER_GB),
        "temp_c": temp_c,
        "load_avg": load_avg(),
    }))
}

/// The most-degraded system shape: neither the store nor the host read yielded
/// the essentials. Every numeric reading is null; `temp_c` still attempts the
/// sysfs read and `load_avg` still reads `/proc/loadavg`.
fn degraded_system() -> Value {
    json!({
        "cpu_percent": Value::Null,
        "memory_used_mb": Value::Null,
        "memory_total_mb": Value::Null,
        "disk_used_gb": Value::Null,
        "disk_total_gb": Value::Null,
        "temp_c": read_cpu_temp_sysfs().map(Value::from).unwrap_or(Value::Null),
        "load_avg": load_avg(),
    })
}

/// The 1/5/15-minute load average from `/proc/loadavg`, each rounded to two
/// decimals — the value `os.getloadavg()` reports. Null when it cannot be read:
/// three zeros would claim a measured idle host.
fn load_avg() -> Value {
    load_avg_value(read_loadavg())
}

/// The wire form of a parsed load average.
fn load_avg_value(parsed: Option<[f64; 3]>) -> Value {
    match parsed {
        Some([a, b, c]) => json!([round2(a), round2(b), round2(c)]),
        None => Value::Null,
    }
}

/// Parse the first three fields of `/proc/loadavg` (`<1m> <5m> <15m> ...`) as
/// floats. `None` when the file is absent / unreadable / malformed.
fn read_loadavg() -> Option<[f64; 3]> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut it = text.split_whitespace();
    let a: f64 = it.next()?.parse().ok()?;
    let b: f64 = it.next()?.parse().ok()?;
    let c: f64 = it.next()?.parse().ok()?;
    Some([a, b, c])
}

/// The SoC temperature from `/sys/class/thermal/thermal_zone0/temp` (milli-degrees
/// Celsius), divided by 1000 and rounded to one decimal — the final fallback the
/// Python `_read_cpu_temp` lands on. `None` when the file is absent / unreadable /
/// malformed.
fn read_cpu_temp_sysfs() -> Option<f64> {
    let raw = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    let milli: i64 = raw.trim().parse().ok()?;
    Some(round1(milli as f64 / 1000.0))
}

// ---------------------------------------------------------------------------
// network — primary IPv4 + ethernet/wlan MAC.
// ---------------------------------------------------------------------------

/// The network block, mirroring the FastAPI `_collect_network`: the primary
/// non-loopback IPv4 and the `eth0` / `wlan0` MAC addresses. Each leg degrades to
/// `null` when its source is unavailable, the same `None` the Python reads return
/// on a miss.
fn collect_network() -> Value {
    json!({
        "ip": read_primary_ipv4().map(Value::from).unwrap_or(Value::Null),
        "mac_eth0": read_mac("eth0").map(Value::from).unwrap_or(Value::Null),
        "mac_wlan0": read_mac("wlan0").map(Value::from).unwrap_or(Value::Null),
    })
}

/// The MAC address of `iface` from `/sys/class/net/<iface>/address`, trimmed. The
/// same `/sys` read the Python `_read_mac` does. `None` when the file is absent
/// (the interface is missing) / unreadable / empty.
fn read_mac(iface: &str) -> Option<String> {
    let path = format!("/sys/class/net/{iface}/address");
    let mac = std::fs::read_to_string(path).ok()?;
    let mac = mac.trim();
    if mac.is_empty() {
        None
    } else {
        Some(mac.to_string())
    }
}

/// The first non-loopback IPv4 host address, the same value the Python
/// `_read_primary_ipv4` extracts from `ip -4 -o addr show`. The native reads the
/// kernel routing-table dump at `/proc/net/fib_trie` instead of shelling out: it
/// scans for `/32` host entries tagged `LOCAL` (a locally-assigned address) and
/// returns the first that is not loopback (`127.0.0.0/8`). `None` when the file is
/// absent / unreadable or carries no usable address.
fn read_primary_ipv4() -> Option<String> {
    let text = std::fs::read_to_string("/proc/net/fib_trie").ok()?;
    parse_primary_ipv4(&text)
}

/// Extract the first non-loopback `/32` LOCAL host address from a `/proc/net/fib_trie`
/// dump. The dump indents a tree; a host address appears as a `|-- <ip>` line
/// immediately followed (within the next couple of lines) by a `/32 host LOCAL`
/// marker. This walks the lines, remembering the most recent `|-- <ip>` candidate,
/// and returns it on the first `/32 host LOCAL` that is not in `127.0.0.0/8`. Pure
/// (text in), so it is unit-testable without a host network.
fn parse_primary_ipv4(text: &str) -> Option<String> {
    let mut candidate: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '|', '+', '-']).trim();
        if is_ipv4_literal(trimmed) {
            // A bare `<ip>` line (the tree leaf key) is the candidate for the
            // marker line that follows it.
            candidate = Some(trimmed.to_string());
            continue;
        }
        if trimmed.contains("/32 host LOCAL") {
            if let Some(ip) = candidate.take() {
                if !ip.starts_with("127.") {
                    return Some(ip);
                }
            }
        }
    }
    None
}

/// True when `s` is a dotted-quad IPv4 literal (four `0..=255` octets). Used to
/// pick the leaf-key lines out of the `fib_trie` tree dump.
fn is_ipv4_literal(s: &str) -> bool {
    let mut octets = 0;
    for part in s.split('.') {
        match part.parse::<u16>() {
            Ok(n) if n <= 255 && !part.is_empty() => octets += 1,
            _ => return false,
        }
    }
    octets == 4
}

// ---------------------------------------------------------------------------
// device — this node's device id.
// ---------------------------------------------------------------------------

/// The device identity: the node's resolved device id (the same one the
/// pairing-info route reports), or `"--"` on an unprovisioned node.
fn collect_device(config_path: &Path) -> Value {
    let cfg = crate::config::PairingConfig::load_from(config_path);
    let device_id = Some(cfg.agent.device_id)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "--".to_string());
    json!({ "device_id": device_id })
}

// ---------------------------------------------------------------------------
// logs — the last few ados-agent log lines.
// ---------------------------------------------------------------------------

/// How many recent log lines the triage snapshot carries.
const DIAG_LOG_LINES: i64 = 10;

/// The most recent agent log lines from the logging store, oldest first, each
/// `"<source> <LEVEL>: <message>"`. `null` when the store cannot be read, so an
/// unreadable store is not shown as a quiet log.
async fn collect_logs(logd: &crate::ipc::LogdQueryClient) -> Value {
    match logd.rows("logs", DIAG_LOG_LINES, None).await {
        Some(rows) => Value::Array(log_lines(&rows)),
        None => Value::Null,
    }
}

/// Render store log rows (newest first, as the query returns them) as display
/// lines in chronological order.
fn log_lines(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .rev()
        .filter_map(|r| {
            let msg = r.get("msg")?.as_str()?;
            let source = r.get("source").and_then(Value::as_str).unwrap_or("?");
            let level = r.get("level").and_then(Value::as_str).unwrap_or("?");
            Some(json!(format!(
                "{source} {}: {msg}",
                level.to_ascii_uppercase()
            )))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Per-file helpers (copied verbatim from the sibling resource routes so the
// arithmetic byte-matches Python's round()).
// ---------------------------------------------------------------------------

/// A numeric signal value, or `None` if absent / non-numeric (a JSON `bool` is not
/// a `Number`, so it is excluded, matching the Python `_num` bool guard).
fn signal_num(signals: &Map<String, Value>, key: &str) -> Option<f64> {
    match signals.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// Round to one decimal place, matching the Python `round(x, 1)`.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Round to two decimal places, matching the Python `round(x, 2)` the load
/// average carries.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Round to the nearest integer with round-half-to-even (banker's rounding),
/// byte-matching the Python built-in `round(x)` the MB conversions use.
fn round_int(v: f64) -> i64 {
    let floor = v.floor();
    let diff = v - floor;
    let rounded = if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else {
        let f = floor as i64;
        if f % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    };
    rounded as i64
}

#[cfg(test)]
mod tests;

// The bytes-every-hop pipeline integration test lives in its own file (this one is
// already large) but is a CHILD module of `diagnostics`, so it reaches the crate-
// internal sampler seams (`VideoSample::from_run_dir`, `build_video_diagnostics`)
// without widening the public surface.
#[cfg(test)]
mod video_pipeline_e2e;
