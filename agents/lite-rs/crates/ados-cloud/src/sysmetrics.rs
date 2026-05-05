//! System metrics collection for the heartbeat enrichment payload.
//!
//! The cloud relay heartbeat carries dynamic per-tick numbers (cpu, mem,
//! temperature) so the GCS fleet card can render live load and operators
//! can spot a thermally-throttled drone before a flight goes wrong. The
//! Python full agent collects these via `psutil`; on lite we use the
//! `sysinfo` crate which reads `/proc` on Linux without spawning shells.
//!
//! Collection is best-effort. A missing `/sys/class/thermal/thermal_zone0`
//! does not fail the heartbeat — temperature reads as `None` and the GCS
//! shows a dash. Same for the cpu and memory probes when sysinfo cannot
//! refresh on a given platform.

use std::sync::Mutex;

use serde::Serialize;
use sysinfo::System;

/// Snapshot of the live system metrics. All fields optional so a partial
/// read still produces a usable heartbeat.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SysMetrics {
    /// Process-wide CPU utilisation as a percentage (0-100). Computed
    /// from sysinfo's two-tick sampler — the first call after process
    /// start returns 0.0 because there is no prior sample to diff
    /// against; subsequent calls return real values.
    pub cpu_pct: Option<f32>,

    /// Total physical memory in megabytes (1024 * 1024 bytes per MB).
    pub mem_total_mb: Option<u64>,

    /// Used physical memory in megabytes. `total - available` matching
    /// the `free` command's "used" column rather than "free" so cache
    /// memory is correctly counted as available.
    pub mem_used_mb: Option<u64>,

    /// SoC temperature in degrees Celsius. Read from
    /// `/sys/class/thermal/thermal_zone0/temp` (millidegrees) and
    /// divided by 1000. Boards without a thermal zone return None.
    pub soc_temp_c: Option<f32>,
}

/// Singleton sysinfo `System` so the CPU sampler accumulates state
/// across heartbeat ticks. sysinfo's CPU number requires two refreshes
/// at least the minimum interval apart to produce a non-zero reading,
/// which is exactly how a heartbeat-tick sampler should behave.
static SYS: Mutex<Option<System>> = Mutex::new(None);

/// Collect a fresh metrics snapshot. Cheap to call (~200 µs on a
/// Cortex-A7) — single `/proc` walk inside sysinfo, single open + parse
/// for the thermal zone.
pub fn collect() -> SysMetrics {
    let mut metrics = SysMetrics::default();
    if let Ok(mut guard) = SYS.lock() {
        let sys = guard.get_or_insert_with(System::new);
        sys.refresh_memory();
        sys.refresh_cpu_usage();
        metrics.mem_total_mb = Some(sys.total_memory() / (1024 * 1024));
        let used = sys.total_memory().saturating_sub(sys.available_memory());
        metrics.mem_used_mb = Some(used / (1024 * 1024));
        metrics.cpu_pct = Some(sys.global_cpu_usage());
    }
    metrics.soc_temp_c = read_soc_temp();
    metrics
}

fn read_soc_temp() -> Option<f32> {
    // The kernel exposes thermal zones at /sys/class/thermal/thermal_zone*.
    // Zone 0 is conventionally the CPU/SoC zone on RPi, Rockchip, and
    // most ARM SBCs. The value is millidegrees Celsius.
    let raw = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    let trimmed = raw.trim();
    let millideg: i32 = trimmed.parse().ok()?;
    Some(millideg as f32 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_returns_some_memory_values() {
        // Memory should always be readable on any Linux/macOS test host.
        let m = collect();
        assert!(m.mem_total_mb.is_some(), "mem_total_mb must be readable");
        assert!(m.mem_used_mb.is_some(), "mem_used_mb must be readable");
        let total = m.mem_total_mb.unwrap();
        let used = m.mem_used_mb.unwrap();
        assert!(total > 0, "total memory must be > 0");
        assert!(
            used <= total,
            "used ({used} MB) must not exceed total ({total} MB)"
        );
    }

    #[test]
    fn collect_handles_missing_thermal_zone_gracefully() {
        // On a machine without thermal_zone0 (most macOS hosts), the
        // helper must return None rather than panicking.
        let m = collect();
        // Either Some on Linux SBCs, or None on macOS — both are fine.
        if let Some(t) = m.soc_temp_c {
            assert!(
                (0.0..=120.0).contains(&t),
                "soc_temp_c {t} outside plausible range"
            );
        }
    }

    #[test]
    fn cpu_pct_within_range() {
        // Two collects produce a non-zero CPU reading on the second tick.
        let _ = collect();
        std::thread::sleep(std::time::Duration::from_millis(250));
        let m = collect();
        if let Some(pct) = m.cpu_pct {
            assert!(
                (0.0..=100.0 * 64.0).contains(&pct),
                "cpu_pct {pct} outside expected range"
            );
        }
    }
}
