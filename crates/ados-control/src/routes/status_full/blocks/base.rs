//! Resources: the 13-field subset of derive_resources.

use ados_protocol::wfb_status::json_truthy;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

pub(crate) const BYTES_PER_MB: f64 = 1024.0 * 1024.0;
pub(crate) const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Derive the consolidated-status `resources` block from merged hardware signals.
///
/// Returns the exact 13-field subset the FastAPI `/api/status/full` selects from
/// `derive_resources`: `cpu_percent`, the memory family, the swap family, the disk
/// family, and `temperature` (the `temperatures` map + `load_avg` list the helper
/// also produces are NOT carried by the consolidated route). Returns an empty
/// object `{}` when the store is unreachable or an essential field is missing —
/// the most-degraded shape the FastAPI route emits (both the store and psutil
/// unavailable). The essential set mirrors the Python guard: memory total +
/// available, aggregate CPU, and filesystem total + used.
pub(crate) fn derive_resources_subset(signals: Option<&Map<String, Value>>) -> Value {
    let Some(signals) = signals else {
        return json!({});
    };

    let total = signal_num(signals, "mem.total_bytes");
    let avail = signal_num(signals, "mem.avail_bytes");
    let cpu = signal_num(signals, "cpu.util.all");
    let disk_total = signal_num(signals, "disk.fs_total_bytes");
    let disk_used = signal_num(signals, "disk.fs_used_bytes");

    // Any missing essential → the route falls back to a complete read; on this
    // daemon there is no psutil fallback, so the most-degraded path is an empty
    // object, matching the Python `except ImportError: pass` (resources stays {}).
    let (Some(total), Some(avail), Some(cpu), Some(disk_total), Some(disk_used)) =
        (total, avail, cpu, disk_total, disk_used)
    else {
        return json!({});
    };

    let used = (total - avail).max(0.0);
    let swap_total = signal_num(signals, "mem.swap_total_bytes").unwrap_or(0.0);
    let swap_free = signal_num(signals, "mem.swap_free_bytes").unwrap_or(0.0);
    let swap_used = (swap_total - swap_free).max(0.0);
    let cache = signal_num(signals, "mem.cache_bytes").unwrap_or(0.0);
    let primary = signal_num(signals, "thermal.primary_c");

    let memory_percent = if total > 0.0 {
        round1(used / total * 100.0)
    } else {
        0.0
    };
    let swap_percent = if swap_total > 0.0 {
        round1(swap_used / swap_total * 100.0)
    } else {
        0.0
    };
    let disk_percent = if disk_total > 0.0 {
        round1(disk_used / disk_total * 100.0)
    } else {
        0.0
    };

    json!({
        "cpu_percent": round1(cpu),
        "memory_percent": memory_percent,
        "memory_used_mb": round_int(used / BYTES_PER_MB),
        "memory_total_mb": round_int(total / BYTES_PER_MB),
        "memory_available_mb": round_int(avail / BYTES_PER_MB),
        "memory_cache_mb": round_int(cache / BYTES_PER_MB),
        "swap_total_mb": round_int(swap_total / BYTES_PER_MB),
        "swap_used_mb": round_int(swap_used / BYTES_PER_MB),
        "swap_percent": swap_percent,
        "disk_percent": disk_percent,
        "disk_used_gb": round1(disk_used / BYTES_PER_GB),
        "disk_total_gb": round1(disk_total / BYTES_PER_GB),
        "temperature": primary.map(Value::from).unwrap_or(Value::Null),
    })
}

/// A numeric signal value, or `None` if absent / non-numeric. A JSON `bool` is not
/// a `Number`, so it is excluded naturally (matching the Python `_num` bool guard).
pub(crate) fn signal_num(signals: &Map<String, Value>, key: &str) -> Option<f64> {
    match signals.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// Round to one decimal place, matching the Python `round(x, 1)`.
pub(crate) fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Round to the nearest integer with round-half-to-even (banker's rounding),
/// byte-matching the Python built-in `round(x)` the MB conversions use. The
/// distinction only bites at an exact `.5` (which a real byte/MB conversion almost
/// never hits), but the contract is byte-parity, so the tie-break must match.
pub(crate) fn round_int(v: f64) -> i64 {
    let floor = v.floor();
    let diff = v - floor;
    let rounded = if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else {
        // Exactly .5: round to the even neighbour.
        let f = floor as i64;
        if f % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    };
    rounded as i64
}

// ---------------------------------------------------------------------------
// Services: the systemd-fallback inventory in the consolidated-status shape.
// ---------------------------------------------------------------------------

/// Build the consolidated-status `services` list from the systemd unit inventory.
///
/// This daemon has no in-process service tracker, so it serves the systemd
/// inventory: one object per `ados-*.service` unit, shaped `{name, state,
/// sub_state, task_done, memory_mb}`. `state` is the systemd ActiveState
/// (`idle` plus `reason: "condition_unmet"` for a condition-skipped unit, see
/// [`mark_condition_unmet`]) and `sub_state` the unit's sub-state (each
/// `"unknown"` when absent); `task_done`
/// is `sub_state != "running"`; no uptime is emitted (this daemon carries no
/// transition log, and a 0 would read as a just-restarted unit); `memory_mb` is
/// the unit's grouped PSS. An absent / failing `systemctl` degrades to an empty
/// list.
pub(crate) async fn build_services_list() -> Value {
    let mut services = systemd_services_fallback().await;
    mark_condition_unmet(&mut services).await;
    crate::routes::services::attach_service_memory(&mut services).await;
    Value::Array(services)
}

/// Relabel every inactive unit whose last start systemd skipped on an unmet
/// unit condition (`ConditionPathExists=/dev/hailo0` on a board without the
/// accelerator) as `state: "idle"`, `reason: "condition_unmet"`: the
/// supervisor holds such a row idle, not stopped or failed, and a bare
/// `inactive` would not say why. One `systemctl show` covers every inactive row.
pub(crate) async fn mark_condition_unmet(services: &mut [Value]) {
    let units: Vec<String> = services
        .iter()
        .filter(|s| s["state"] == "inactive")
        .filter_map(|s| s["name"].as_str().map(|n| format!("{n}.service")))
        .collect();
    if units.is_empty() {
        return;
    }
    let mut args = vec![
        "show",
        "-p",
        "Id,ConditionResult,ConditionTimestampMonotonic",
    ];
    args.extend(units.iter().map(String::as_str));
    let out = crate::probe::capture_systemctl(&args, crate::probe::PROBE_TIMEOUT).await;
    apply_condition_unmet(services, &condition_unmet_units(out.text()));
}

/// The unit names (without `.service`) in `systemctl show -p
/// Id,ConditionResult,ConditionTimestampMonotonic` output (one blank-line
/// separated block per unit) whose condition was evaluated and failed. A unit
/// whose condition was never evaluated reads `ConditionResult=no` with a zero
/// timestamp, and is not one of them.
pub(crate) fn condition_unmet_units(text: &str) -> std::collections::HashSet<String> {
    text.split("\n\n")
        .filter_map(|block| {
            let (mut id, mut result, mut evaluated) = (None, None, false);
            for line in block.lines() {
                match line.split_once('=') {
                    Some(("Id", v)) => id = Some(v.trim()),
                    Some(("ConditionResult", v)) => result = Some(v.trim()),
                    Some(("ConditionTimestampMonotonic", v)) => {
                        evaluated = v.trim().parse::<u64>().is_ok_and(|t| t > 0);
                    }
                    _ => {}
                }
            }
            if result == Some("no") && evaluated {
                id?.strip_suffix(".service").map(str::to_string)
            } else {
                None
            }
        })
        .collect()
}

/// Set `state: "idle"`, `reason: "condition_unmet"` on each inactive row named
/// in `unmet`.
pub(crate) fn apply_condition_unmet(
    services: &mut [Value],
    unmet: &std::collections::HashSet<String>,
) {
    for svc in services.iter_mut() {
        let hit =
            svc["state"] == "inactive" && svc["name"].as_str().is_some_and(|n| unmet.contains(n));
        if hit {
            svc["state"] = json!("idle");
            svc["reason"] = json!("condition_unmet");
        }
    }
}

/// Enumerate `ados-*.service` units via `systemctl list-units`, returning the
/// per-unit objects (without `memory_mb`, which the caller attaches).
///
/// : `systemctl list-units
/// --type=service --all --no-pager --no-legend ados-*.service`, then for each row
/// `parts = line.split(None, 4)` (at most five whitespace-split tokens), the unit
/// is `parts[0]` with leading status glyphs (`●`/`*`) stripped, the sub-state is
/// `parts[3]`, and the object is built from those. A non-`.service` unit, a row
/// with fewer than four columns, an `rc != 0`, or a missing `systemctl` yields an
/// empty list.
///
/// The probe forces `SYSTEMD_COLORS=0` + an empty pager + `LANG=C` (see
/// [`crate::probe::capture_systemctl`], which owns those three variables now):
/// systemd prefixes a failed unit's row with a status glyph and will colour or
/// page its output otherwise, and this parser splits on columns. `rc != 0` and a
/// missing binary both arrive as [`crate::probe::ProbeOutput::Unavailable`],
/// which is the empty list the Python returns for either — as is a probe that
/// times out on a wedged bus.
pub(crate) async fn systemd_services_fallback() -> Vec<Value> {
    let out = crate::probe::capture_systemctl(
        &[
            "list-units",
            "--type=service",
            "--all",
            "--no-pager",
            "--no-legend",
            "--plain",
            "ados-*.service",
        ],
        crate::probe::PROBE_TIMEOUT,
    )
    .await;

    out.text().lines().filter_map(parse_fallback_line).collect()
}

/// Parse one `systemctl list-units` row into the fallback service object, or
/// `None` when the row is blank, has too few columns, or is not a `.service` unit.
///
/// The row itself is parsed by the one unit-row parser the services route uses
/// ([`crate::routes::services::parse_unit_line`]), which strips a leading status
/// glyph. A failed unit is exactly the row the dashboard most needs, and older
/// systemd builds prefix it with `●`/`*` even under `--plain`. This projects the
/// parsed row onto the status/full shape: `state` is the systemd ActiveState
/// (`active`, `inactive`, `failed`, `activating`, …, or `unknown`), `sub_state`
/// is the unit's sub-state (`running`, `dead`, `exited`, `auto-restart`, …), and
/// `task_done` is `sub_state != "running"`. No uptime is carried: the list does
/// not report one, so the consumer shows it as unknown.
pub(crate) fn parse_fallback_line(line: &str) -> Option<Value> {
    let unit = crate::routes::services::parse_unit_line(line)?;
    let name = unit["name"].as_str()?.to_string();
    let or_unknown = |key: &str| {
        let v = unit[key].as_str().unwrap_or("").trim();
        if v.is_empty() { "unknown" } else { v }.to_string()
    };
    let state = or_unknown("state");
    let sub = or_unknown("sub_state");

    Some(json!({
        "name": name,
        "state": state,
        "task_done": sub != "running",
        "sub_state": sub,
    }))
}

// ---------------------------------------------------------------------------
// logd query seam + shared helpers.
// ---------------------------------------------------------------------------

/// `bool(map.get(key, False))` — Python truthiness over the value, defaulting to
/// `false` when the key is absent.
pub(crate) fn json_truthy_default_false(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key).map(json_truthy).unwrap_or(false)
}

/// The current wall-clock time in seconds (float) since the Unix epoch, matching
/// the Python `time.time()` used for the sidecar freshness gate.
pub(crate) fn now_unix_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
