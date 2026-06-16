//! `GET /api/system` — CPU / memory / swap / disk / temperature snapshot.
//!
//! Byte-faithful to the FastAPI `system.get_system_resources`: the primary
//! source is the durable logging store's merged hardware snapshot (the one
//! Rust collector), mapped to the canonical resource fields. The native front
//! has no `psutil`, so when the store is unreachable or missing an essential
//! field it returns the most-degraded null shape (the Python `except
//! ImportError` branch) rather than a live read — the same store-first /
//! degrade-in-place contract the consolidated `/api/status/full` resources
//! block follows. The live numeric readings are masked in the conformance diff;
//! the stable scalars (`cpu_count`, the `*_total_*` capacities) are the
//! contract.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::state::AppState;

const BYTES_PER_MB: f64 = 1024.0 * 1024.0;
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// `GET /api/system`. Guaranteed 200: degrades to the null shape rather than a
/// 500 when the store has no usable snapshot.
pub async fn get_system_resources(State(state): State<AppState>) -> Json<Value> {
    let signals = state.logd.latest_hw_signals().await;
    match signals.as_ref().and_then(derive_system) {
        Some(body) => Json(body),
        None => Json(degraded()),
    }
}

/// Map merged hardware signals to the `/api/system` 14-field shape, or `None`
/// when an essential field is missing (memory total + available, aggregate CPU,
/// filesystem total + used — the same essential set as `derive_resources`).
fn derive_system(signals: &Map<String, Value>) -> Option<Value> {
    let total = signal_num(signals, "mem.total_bytes")?;
    let avail = signal_num(signals, "mem.avail_bytes")?;
    let cpu = signal_num(signals, "cpu.util.all")?;
    let disk_total = signal_num(signals, "disk.fs_total_bytes")?;
    let disk_used = signal_num(signals, "disk.fs_used_bytes")?;

    let used = (total - avail).max(0.0);
    let swap_total = signal_num(signals, "mem.swap_total_bytes").unwrap_or(0.0);
    let swap_free = signal_num(signals, "mem.swap_free_bytes").unwrap_or(0.0);
    let swap_used = (swap_total - swap_free).max(0.0);
    let cache = signal_num(signals, "mem.cache_bytes").unwrap_or(0.0);

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

    Some(json!({
        "cpu_percent": round1(cpu),
        "cpu_count": cpu_count(),
        "memory_total_mb": round_int(total / BYTES_PER_MB),
        "memory_used_mb": round_int(used / BYTES_PER_MB),
        "memory_available_mb": round_int(avail / BYTES_PER_MB),
        "memory_cache_mb": round_int(cache / BYTES_PER_MB),
        "memory_percent": memory_percent,
        "swap_total_mb": round_int(swap_total / BYTES_PER_MB),
        "swap_used_mb": round_int(swap_used / BYTES_PER_MB),
        "swap_percent": swap_percent,
        "disk_total_gb": round1(disk_total / BYTES_PER_GB),
        "disk_used_gb": round1(disk_used / BYTES_PER_GB),
        "disk_percent": disk_percent,
        "temperatures": temperatures(signals),
    }))
}

/// The per-sensor temperature map, built from the `thermal.<sensor>_c` signals
/// exactly like the Python `derive_resources`: every `thermal.*_c` key EXCEPT
/// `thermal.primary_c` (a duplicate of the first zone, surfaced separately),
/// keyed by the sensor sub-name. A `bool` is excluded (a JSON bool is not a
/// number, matching the Python `isinstance` guard).
fn temperatures(signals: &Map<String, Value>) -> Value {
    let mut temps = Map::new();
    for (key, value) in signals {
        if !(key.starts_with("thermal.") && key.ends_with("_c")) {
            continue;
        }
        if key == "thermal.primary_c" {
            continue;
        }
        if let Value::Number(n) = value {
            if let Some(f) = n.as_f64() {
                let name = &key["thermal.".len()..key.len() - "_c".len()];
                temps.insert(name.to_string(), json!(f));
            }
        }
    }
    Value::Object(temps)
}

/// The most-degraded shape: the FastAPI `except ImportError` branch (store down
/// AND no psutil). All fields null + `available: false` so the dashboard renders
/// "—" rather than misleading zeros. The native reaches this only when the store
/// is unreachable (it has no psutil to fall back to).
fn degraded() -> Value {
    json!({
        "cpu_percent": Value::Null,
        "cpu_count": Value::Null,
        "memory_total_mb": Value::Null,
        "memory_used_mb": Value::Null,
        "memory_available_mb": Value::Null,
        "memory_cache_mb": Value::Null,
        "memory_percent": Value::Null,
        "swap_total_mb": Value::Null,
        "swap_used_mb": Value::Null,
        "swap_percent": Value::Null,
        "disk_total_gb": Value::Null,
        "disk_used_gb": Value::Null,
        "disk_percent": Value::Null,
        "temperatures": {},
        "available": false,
    })
}

/// The logical CPU count, matching the Python `os.cpu_count()`: the total online
/// logical CPUs (NOT affinity-limited), read by counting `processor` records in
/// `/proc/cpuinfo`. Returns `Value::Null` when the count is indeterminate, the
/// same null `os.cpu_count()` can return.
fn cpu_count() -> Value {
    match std::fs::read_to_string("/proc/cpuinfo") {
        Ok(text) => {
            let n = text.lines().filter(|l| l.starts_with("processor")).count();
            if n > 0 {
                json!(n)
            } else {
                Value::Null
            }
        }
        Err(_) => Value::Null,
    }
}

/// A numeric signal value, or `None` if absent / non-numeric (a JSON `bool` is
/// not a `Number`, so it is excluded, matching the Python `_num` bool guard).
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
    fn derives_the_full_shape_from_signals() {
        let s = signals(&[
            ("mem.total_bytes", 4.0 * BYTES_PER_GB),
            ("mem.avail_bytes", 1.0 * BYTES_PER_GB),
            ("cpu.util.all", 12.34),
            ("disk.fs_total_bytes", 32.0 * BYTES_PER_GB),
            ("disk.fs_used_bytes", 8.0 * BYTES_PER_GB),
            ("mem.swap_total_bytes", 1.0 * BYTES_PER_GB),
            ("mem.swap_free_bytes", 0.5 * BYTES_PER_GB),
            ("mem.cache_bytes", 0.5 * BYTES_PER_GB),
            ("thermal.cpu_thermal_c", 47.5),
            ("thermal.primary_c", 47.5),
        ]);
        let v = derive_system(&s).expect("essentials present");
        assert_eq!(v["cpu_percent"], json!(12.3));
        assert_eq!(v["memory_total_mb"], json!(4096));
        assert_eq!(v["memory_used_mb"], json!(3072));
        assert_eq!(v["memory_available_mb"], json!(1024));
        assert_eq!(v["disk_total_gb"], json!(32.0));
        assert_eq!(v["disk_used_gb"], json!(8.0));
        assert_eq!(v["disk_percent"], json!(25.0));
        assert_eq!(v["swap_percent"], json!(50.0));
        // primary is surfaced separately, so it is NOT in the per-sensor map.
        assert_eq!(v["temperatures"], json!({"cpu_thermal": 47.5}));
        // cpu_count is read from the host; just assert the key exists.
        assert!(v.get("cpu_count").is_some());
    }

    #[test]
    fn missing_an_essential_returns_none() {
        // No CPU signal → None (the route then serves the degraded shape).
        let s = signals(&[
            ("mem.total_bytes", BYTES_PER_GB),
            ("mem.avail_bytes", BYTES_PER_GB),
            ("disk.fs_total_bytes", BYTES_PER_GB),
            ("disk.fs_used_bytes", 0.0),
        ]);
        assert!(derive_system(&s).is_none());
    }

    #[test]
    fn degraded_shape_is_all_null_plus_available_false() {
        let d = degraded();
        assert_eq!(d["cpu_percent"], Value::Null);
        assert_eq!(d["available"], json!(false));
        assert_eq!(d["temperatures"], json!({}));
    }

    #[test]
    fn round_int_is_banker_s_rounding() {
        assert_eq!(round_int(0.5), 0); // ties to even
        assert_eq!(round_int(1.5), 2);
        assert_eq!(round_int(2.5), 2);
        assert_eq!(round_int(2.4), 2);
    }
}
