//! The `system` sub-block (CPU/RAM/temp/uptime/version).

use super::*;

/// The system snapshot the `/status` route carries: `{cpu_pct, ram_used_mb,
/// ram_total_mb, temp_c, uptime_seconds, agent_version}`.
///
/// CPU / RAM / temperature come from the most-recent hardware snapshots in the
/// logging store (the continuous collector samples them) with a direct host read
/// (`crate::hw_local`) behind them, `uptime_seconds` from `/proc/uptime`, and
/// `agent_version` from the resolved app version.
///
/// A leg neither source supplies is `null`. It used to be `0.0` / `0` — and the
/// durable store ships OFF, so on a stock ground station every one of these read
/// zero: the OLED and the GCS Hardware tab rendered an idle CPU and a 0 MB RAM
/// total as measurements. Same reasoning as `status::derive_health`; the keys
/// stay present so a client cannot mistake an unreported leg for an absent one.
pub(super) async fn system_snapshot(state: &AppState) -> Value {
    let signals = match state.logd.latest_hw_signals().await {
        Some(s) => Some(s),
        None => {
            let local = crate::hw_local::collect_signals();
            (!local.is_empty()).then_some(local)
        }
    };
    system_block(
        signals.as_ref(),
        proc_uptime_seconds(),
        &state.agent_version(),
    )
}

/// Compose the `system` block from an optional signal map. Pure, so the
/// unreported-leg behaviour is asserted directly instead of through a
/// re-implementation of it.
pub(super) fn system_block(
    signals: Option<&Map<String, Value>>,
    uptime_s: i64,
    version: &str,
) -> Value {
    let cpu_pct = signals
        .and_then(|s| signal_num(s, "cpu.util.all"))
        .map(Value::from)
        .unwrap_or(Value::Null);
    let (ram_used_mb, ram_total_mb) = match signals.and_then(ram_mb) {
        Some((used, total)) => (Value::from(used), Value::from(total)),
        None => (Value::Null, Value::Null),
    };
    let temp_c = signals
        .and_then(|s| signal_num(s, "thermal.primary_c"))
        .map(Value::from)
        .unwrap_or(Value::Null);
    let disk_pct = signals
        .and_then(root_disk_pct)
        .map(Value::from)
        .unwrap_or(Value::Null);

    json!({
        "cpu_pct": cpu_pct,
        "ram_used_mb": ram_used_mb,
        "ram_total_mb": ram_total_mb,
        "temp_c": temp_c,
        "disk_pct": disk_pct,
        "uptime_seconds": uptime_s,
        "agent_version": version,
    })
}

/// Root-filesystem usage in percent (one decimal) from the used + total byte
/// signals. `None` when either signal is absent or the total is not positive.
pub(super) fn root_disk_pct(signals: &Map<String, Value>) -> Option<f64> {
    let total = signal_num(signals, "disk.fs_total_bytes")?;
    let used = signal_num(signals, "disk.fs_used_bytes")?;
    (total > 0.0).then(|| (used.max(0.0) / total * 1000.0).round() / 10.0)
}

/// Used + total RAM in MiB from the total + available byte signals, mirroring the
/// Python `_system_snapshot` arithmetic (`(total - available) / 1MiB` used,
/// `total / 1MiB` total). `None` when either byte signal is absent.
pub(super) fn ram_mb(signals: &Map<String, Value>) -> Option<(i64, i64)> {
    let total = signal_num(signals, "mem.total_bytes")?;
    let avail = signal_num(signals, "mem.avail_bytes")?;
    let used_bytes = (total - avail).max(0.0);
    let mib = 1024.0 * 1024.0;
    Some(((used_bytes / mib) as i64, (total / mib) as i64))
}

/// System uptime in whole seconds from `/proc/uptime`, the kernel's own counter.
/// Mirrors the Python `int(time.time() - psutil.boot_time())` — `/proc/uptime`'s
/// first field is exactly that delta. Degrades to `0` when the file is absent
/// (a non-Linux dev host) or unparseable, matching the psutil-failure default.
pub(super) fn proc_uptime_seconds() -> i64 {
    match std::fs::read_to_string("/proc/uptime") {
        Ok(text) => text
            .split_whitespace()
            .next()
            .and_then(|f| f.parse::<f64>().ok())
            .map(|s| s as i64)
            .unwrap_or(0),
        Err(_) => 0,
    }
}
