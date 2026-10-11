//! Signal readers, time formatting and executable lookup for the status routes.

use super::*;

/// Local epoch ms for the offload-link staleness gate.
pub(super) fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn read_board(path: &Path) -> Value {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value.is_object() => value,
            _ => json!({}),
        },
        Err(_) => json!({}),
    }
}

/// A numeric signal value, or `None` if absent / non-numeric. A JSON `bool` is
/// not a `Number`, so it is excluded naturally.
pub(super) fn signal_num(signals: &Map<String, Value>, key: &str) -> Option<f64> {
    match signals.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// Memory used percentage from the total + available byte signals: `(total -
/// available) / total * 100`, the same value `psutil.virtual_memory().percent`
/// reports. `None` when either signal is absent; `0.0` when total is non-positive.
pub(super) fn memory_percent(signals: &Map<String, Value>) -> Option<f64> {
    let total = signal_num(signals, "mem.total_bytes")?;
    let avail = signal_num(signals, "mem.avail_bytes")?;
    if total <= 0.0 {
        return Some(0.0);
    }
    let used = (total - avail).max(0.0);
    Some(used / total * 100.0)
}

/// Filesystem used percentage from the total + used byte signals: `used / total *
/// 100`, the same value `psutil.disk_usage("/").percent` reports. `None` when
/// either signal is absent; `0.0` when total is non-positive.
pub(super) fn disk_percent(signals: &Map<String, Value>) -> Option<f64> {
    let total = signal_num(signals, "disk.fs_total_bytes")?;
    let used = signal_num(signals, "disk.fs_used_bytes")?;
    if total <= 0.0 {
        return Some(0.0);
    }
    Some(used / total * 100.0)
}

/// Round to one decimal place, matching the canonical resource-field rounding.
pub(super) fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// The current UTC time as an ISO-8601 string (`YYYY-MM-DDTHH:MM:SS+00:00`),
/// type-matching the Python `datetime.now(timezone.utc).isoformat()` the health
/// block carries. Second precision (the value is not byte-compared; it is a
/// freshness marker), `+00:00` offset.
pub(super) fn iso8601_utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    iso8601_from_unix_secs(secs)
}

/// Format a Unix-epoch second count as `YYYY-MM-DDTHH:MM:SS+00:00` (UTC). Uses
/// the civil-from-days conversion so it is correct across month/year/leap
/// boundaries without a date-time dependency.
pub(super) fn iso8601_from_unix_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Howard Hinnant's civil_from_days: days since the Unix epoch → (y, m, d).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

/// True when `name` is found as an executable on `PATH` (see [`binary_path`]).
pub(super) fn binary_on_path(name: &str) -> bool {
    binary_path(name).is_some()
}

/// The first executable named `name` on `PATH`, or `None`. Mirrors
/// `shutil.which(name)` for the no-explicit-path case (the dependency names are
/// bare command names, never absolute paths): walk each `PATH` entry, join the
/// name, and accept the first regular file that is executable. Off Unix the
/// executable-bit check is skipped (existence is the signal).
pub(crate) fn binary_path(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// True when `path` is a file that is executable by the current user. On Unix the
/// owner/group/other execute bits are checked; elsewhere existence as a file is
/// the signal.
#[cfg(unix)]
pub(super) fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub(super) fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Process uptime: seconds since this daemon started, the fallback the status
/// route uses when the snapshot carries no `service_uptime`. The Python route
/// falls back to the runtime's process uptime in the same way.
pub fn process_uptime_seconds(started: Instant) -> f64 {
    started.elapsed().as_secs_f64()
}
