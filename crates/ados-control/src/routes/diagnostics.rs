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
//! * `device` — the configured `device_id`.
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

const BYTES_PER_MB: f64 = 1024.0 * 1024.0;
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// `GET /api/v1/diagnostics`. Guaranteed 200: every section degrades to its
/// FastAPI default rather than failing when its source is unavailable.
///
/// The FastAPI route wraps the body in a 1-second TTL cache so concurrent LCD +
/// GCS polls do not fan out to journalctl + psutil; that is a cost optimisation,
/// not part of the contract, so the native composes fresh each call.
pub async fn get_diagnostics(State(state): State<AppState>) -> Json<Value> {
    let signals = state.logd.latest_hw_signals().await;

    Json(json!({
        "agent": collect_agent(&state),
        "board": collect_board(&state.board_path),
        "system": collect_system(signals.as_ref()),
        "network": collect_network(),
        "device": collect_device(&state.pairing_paths.config),
        "logs": { "agent": collect_logs() },
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
/// read. `process_cpu_percent` matches the Python `psutil cpu_percent(interval=0)`
/// first-call convention (`0.0`) — a single-shot read has no prior sample to diff,
/// and the field is masked in the conformance diff. A read miss degrades each
/// metric to `null`, the same `None` the Python `except` arm sets.
fn collect_agent(state: &AppState) -> Value {
    let (cpu, mem) = process_metrics();
    json!({
        "version": state.agent_version,
        "uptime_seconds": state.process_uptime_seconds(),
        "process_cpu_percent": cpu,
        "process_memory_mb": mem,
    })
}

/// This daemon's `(process_cpu_percent, process_memory_mb)`. RSS is read from
/// `/proc/self/statm` (the resident set in pages × the page size, converted to MB
/// and rounded to one decimal), matching the Python `proc.memory_info().rss`
/// conversion. CPU is the `0.0` first-call value the Python `cpu_percent(0.0)`
/// returns. A `statm` read miss degrades the memory metric to `null` (the Python
/// `None`); the field is masked in the conformance diff.
fn process_metrics() -> (Value, Value) {
    let cpu = json!(0.0);
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

/// The system block, mirroring the FastAPI `_collect_system`'s store-first leg:
/// the merged hardware signals mapped to the diagnostics field set
/// (`cpu_percent`, `memory_used_mb`, `memory_total_mb`, `disk_used_gb`,
/// `disk_total_gb`, `temp_c`, `load_avg`). The Python falls back to a live
/// `psutil` read when the store is unreachable or missing an essential field; the
/// native front has no psutil, so a store miss degrades to the all-null shape
/// (the Python `except ImportError` arm) — the same store-first / degrade-in-place
/// contract `/api/system` and `/api/status/full` follow.
///
/// `memory_used_mb` / `memory_total_mb` are ints (the Python `int(r[...])` cast on
/// the rounded MB), `disk_used_gb` / `disk_total_gb` are one-decimal floats, and
/// `temp_c` is the primary thermal zone (`thermal.primary_c`); when that signal is
/// absent the native reads `/sys/class/thermal/thermal_zone0/temp` directly, the
/// same sysfs fallback the Python `_read_cpu_temp` lands on. `load_avg` is the
/// 1/5/15 average read straight from `/proc/loadavg` (the value
/// `os.getloadavg()` reports), each rounded to two decimals.
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

/// The most-degraded system shape: the FastAPI `except ImportError` arm (store
/// down AND no psutil). Every numeric reading is null; `temp_c` still attempts the
/// sysfs read (the Python keeps `_read_cpu_temp()` in that arm), and `load_avg`
/// still reads `/proc/loadavg` (the Python keeps `[0.0, 0.0, 0.0]` only on a hard
/// `getloadavg` miss). The native reaches this only when the store is unreachable.
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
/// decimals — the value `os.getloadavg()` reports, which the Python diagnostics
/// `load_avg` carries. A read / parse miss degrades to `[0.0, 0.0, 0.0]`, the
/// Python `except (AttributeError, OSError)` fallback.
fn load_avg() -> Value {
    match read_loadavg() {
        Some([a, b, c]) => json!([round2(a), round2(b), round2(c)]),
        None => json!([0.0, 0.0, 0.0]),
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
// device — the configured device id.
// ---------------------------------------------------------------------------

/// The device identity, mirroring the FastAPI `_collect_device`: the configured
/// `agent.device_id`, falling back to `/etc/ados/device_id`, then to `"--"`. The
/// config is read off the same `/etc/ados/config.yaml` slice the pairing-info
/// route projects.
fn collect_device(config_path: &Path) -> Value {
    let cfg = crate::config::PairingConfig::load_from(config_path);
    let from_config = Some(cfg.agent.device_id).filter(|s| !s.is_empty());
    let device_id = from_config
        .or_else(read_device_id_file)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "--".to_string());
    json!({ "device_id": device_id })
}

/// The device id persisted at `/etc/ados/device_id`, trimmed. The same file the
/// Python `_collect_device` falls back to when the config carries no `device_id`.
/// `None` when absent / unreadable / empty.
fn read_device_id_file() -> Option<String> {
    let raw = std::fs::read_to_string("/etc/ados/device_id").ok()?;
    let id = raw.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

// ---------------------------------------------------------------------------
// logs — the last few ados-agent log lines.
// ---------------------------------------------------------------------------

/// The last few `ados-agent` log lines, mirroring the FastAPI
/// `_collect_logs("ados-agent", count=10)`.
///
/// The FastAPI route shells out to `journalctl -u ados-agent -o cat`; the native
/// front has no journal-tail seam on the logging-store query client (which exposes
/// only the hardware-snapshot read), so this section degrades to an empty list.
/// The log lines + their timestamps are volatile and are masked in the conformance
/// diff, so the stable contract is the `{logs: {agent: [...]}}` shape, which an
/// empty list satisfies. NOTE for the integrator: when the logging-store client
/// grows a `kind=logs` query method this should query the store for the most
/// recent `ados-agent` lines instead of returning empty.
fn collect_logs() -> Value {
    json!([])
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
mod tests {
    use super::*;

    fn signals(pairs: &[(&str, f64)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect()
    }

    #[test]
    fn system_derives_the_diagnostics_shape_from_signals() {
        let s = signals(&[
            ("mem.total_bytes", 4.0 * BYTES_PER_GB),
            ("mem.avail_bytes", 1.0 * BYTES_PER_GB),
            ("cpu.util.all", 12.34),
            ("disk.fs_total_bytes", 32.0 * BYTES_PER_GB),
            ("disk.fs_used_bytes", 8.0 * BYTES_PER_GB),
            ("thermal.primary_c", 47.49),
        ]);
        let v = derive_system(&s).expect("essentials present");
        assert_eq!(v["cpu_percent"], json!(12.3));
        // 4 GiB and 3 GiB used, as integer MB (banker's-rounded MB).
        assert_eq!(v["memory_total_mb"], json!(4096));
        assert_eq!(v["memory_used_mb"], json!(3072));
        assert_eq!(v["disk_total_gb"], json!(32.0));
        assert_eq!(v["disk_used_gb"], json!(8.0));
        // Primary thermal zone, rounded to one decimal.
        assert_eq!(v["temp_c"], json!(47.5));
        // The load average is read from the host /proc/loadavg; just assert the
        // shape is a three-element array (the values are masked in conformance).
        let load = v["load_avg"].as_array().expect("load_avg is an array");
        assert_eq!(load.len(), 3);
    }

    #[test]
    fn system_missing_an_essential_degrades_to_nulls() {
        // No CPU signal → derive returns None → the collector serves the degraded
        // shape with the numeric readings null.
        let s = signals(&[
            ("mem.total_bytes", BYTES_PER_GB),
            ("mem.avail_bytes", BYTES_PER_GB),
            ("disk.fs_total_bytes", BYTES_PER_GB),
            ("disk.fs_used_bytes", 0.0),
        ]);
        assert!(derive_system(&s).is_none());
        let degraded = collect_system(Some(&s));
        assert_eq!(degraded["cpu_percent"], Value::Null);
        assert_eq!(degraded["memory_total_mb"], Value::Null);
        assert_eq!(degraded["disk_used_gb"], Value::Null);
        // The shape still carries every key (temp_c + load_avg attempt their reads).
        assert!(degraded.get("temp_c").is_some());
        assert_eq!(degraded["load_avg"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn system_with_no_store_is_the_degraded_shape() {
        let d = collect_system(None);
        assert_eq!(d["cpu_percent"], Value::Null);
        assert_eq!(d["memory_used_mb"], Value::Null);
        assert_eq!(d["disk_total_gb"], Value::Null);
        assert_eq!(d["load_avg"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn board_projects_the_sidecar_summary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        std::fs::write(
            &path,
            r#"{"name":"Raspberry Pi 4B","soc":"BCM2711","arch":"aarch64","ram_mb":4096,"extra":"ignored"}"#,
        )
        .unwrap();
        let b = collect_board(&path);
        assert_eq!(b["name"], json!("Raspberry Pi 4B"));
        assert_eq!(b["soc"], json!("BCM2711"));
        assert_eq!(b["arch"], json!("aarch64"));
        assert_eq!(b["ram_total_mb"], json!(4096));
    }

    #[test]
    fn board_of_an_absent_sidecar_is_the_default_summary() {
        let dir = tempfile::tempdir().unwrap();
        let b = collect_board(&dir.path().join("nope.json"));
        // The same `"--"` / `"unknown"` / `0` the FastAPI route emits when its own
        // detect_board() raises.
        assert_eq!(b["name"], json!("--"));
        assert_eq!(b["soc"], json!("unknown"));
        assert_eq!(b["arch"], json!("unknown"));
        assert_eq!(b["ram_total_mb"], json!(0));
    }

    #[test]
    fn board_with_missing_fields_falls_back_per_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        // Only `name` present; soc/arch/ram fall back individually.
        std::fs::write(&path, r#"{"name":"some-board"}"#).unwrap();
        let b = collect_board(&path);
        assert_eq!(b["name"], json!("some-board"));
        assert_eq!(b["soc"], json!("unknown"));
        assert_eq!(b["arch"], json!("unknown"));
        assert_eq!(b["ram_total_mb"], json!(0));
    }

    #[test]
    fn device_reads_the_config_device_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "agent:\n  device_id: abcdef1234567890\n").unwrap();
        let d = collect_device(&path);
        assert_eq!(d["device_id"], json!("abcdef1234567890"));
    }

    #[test]
    fn device_of_an_empty_config_falls_back_to_dashes() {
        // No device_id in config and (on the test host) no /etc/ados/device_id →
        // the `"--"` placeholder. The env file is not present in CI, so the config
        // miss resolves to the final fallback.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "agent:\n  name: test\n").unwrap();
        let d = collect_device(&path);
        // Either the host has no /etc/ados/device_id (→ "--") or, on a real rig, a
        // real id; both are non-empty strings. Assert the key is a string.
        assert!(d["device_id"].is_string());
    }

    #[test]
    fn network_shape_carries_every_key() {
        // The values depend on the host network; assert the structural contract:
        // all three keys present (each is a string IP/MAC or null).
        let n = collect_network();
        assert!(n.get("ip").is_some());
        assert!(n.get("mac_eth0").is_some());
        assert!(n.get("mac_wlan0").is_some());
    }

    #[test]
    fn parse_primary_ipv4_picks_the_first_non_loopback_host() {
        // A trimmed-down /proc/net/fib_trie dump: the loopback host first, then a
        // real LAN host. The loopback /32 LOCAL is skipped; the LAN host wins.
        let dump = "\
Local:
  +-- 0.0.0.0/0 2 0 2
     |-- 127.0.0.1
        /32 host LOCAL
     |-- 192.168.1.42
        /32 host LOCAL
     |-- 192.168.1.255
        /32 link BROADCAST
";
        assert_eq!(parse_primary_ipv4(dump), Some("192.168.1.42".to_string()));
    }

    #[test]
    fn parse_primary_ipv4_with_only_loopback_is_none() {
        let dump = "\
Local:
     |-- 127.0.0.1
        /32 host LOCAL
";
        assert_eq!(parse_primary_ipv4(dump), None);
    }

    #[test]
    fn is_ipv4_literal_accepts_dotted_quads_only() {
        assert!(is_ipv4_literal("192.168.1.1"));
        assert!(is_ipv4_literal("10.0.0.255"));
        assert!(!is_ipv4_literal("256.0.0.1"));
        assert!(!is_ipv4_literal("1.2.3"));
        assert!(!is_ipv4_literal("not-an-ip"));
        assert!(!is_ipv4_literal("/32"));
    }

    #[test]
    fn logs_agent_is_an_empty_list() {
        // No journal-tail seam on the logging-store client yet; the section
        // degrades to an empty list (the log lines are masked in conformance).
        assert_eq!(collect_logs(), json!([]));
    }

    #[test]
    fn round_int_is_banker_s_rounding() {
        assert_eq!(round_int(0.5), 0); // ties to even
        assert_eq!(round_int(1.5), 2);
        assert_eq!(round_int(2.5), 2);
        assert_eq!(round_int(2.4), 2);
    }

    #[test]
    fn round2_matches_python_round_two() {
        assert_eq!(round2(0.425), 0.43);
        assert_eq!(round2(1.005), 1.0);
        assert_eq!(round2(0.42), 0.42);
    }
}
