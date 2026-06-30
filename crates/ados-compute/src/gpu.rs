//! Host-GPU reporting for the workstation profile.
//!
//! On a Mac the compute node runs on Apple-Silicon (or an Intel + discrete)
//! GPU, so the compute status carries a `gpu` block: the GPU identity (name, core
//! count, Metal support, unified-memory size) and a live utilisation sample. The
//! identity is static (resolved once, cached); the utilisation is sampled with a
//! short cache so a rapid poll never re-spawns `powermetrics`.
//!
//! Best-effort + honest (Rule 44): every field is `Option`, and any failure (a
//! missing tool, no passwordless sudo for `powermetrics`, a parse miss) leaves
//! that field `null` rather than fabricating a value. On a non-macOS host the
//! whole block is all-`null`.
//!
//! Sources (macOS): `system_profiler SPDisplaysDataType -json` for name / cores /
//! Metal, `sysinfo` for the unified-memory size, and
//! `sudo -n powermetrics --samplers gpu_power -i 200 -n 1` for the live
//! utilisation (the "GPU HW active residency" line).

#[cfg(target_os = "macos")]
use std::sync::Mutex;
use std::sync::OnceLock;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

use ados_protocol::compute::ComputeGpu;

/// Sample the host GPU: the cached static identity plus a freshly-sampled (short-
/// cached) utilisation. On a non-macOS host this is all-`null`.
pub fn sample() -> ComputeGpu {
    let mut gpu = identity();
    gpu.utilization_pct = utilization();
    gpu
}

/// The static GPU identity (name, cores, unified memory, Metal), resolved once
/// and cached for the process lifetime. Utilisation is intentionally NOT cached
/// here (it is live).
fn identity() -> ComputeGpu {
    static IDENTITY: OnceLock<ComputeGpu> = OnceLock::new();
    IDENTITY.get_or_init(probe_identity).clone()
}

#[cfg(target_os = "macos")]
fn probe_identity() -> ComputeGpu {
    let mut gpu = ComputeGpu {
        unified_memory_mb: total_memory_mb(),
        ..ComputeGpu::default()
    };
    if let Some(display) = first_display_json() {
        gpu.name = display
            .get("sppci_model")
            .or_else(|| display.get("_name"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        gpu.cores = display
            .get("sppci_cores")
            .and_then(|v| v.as_str())
            .and_then(|s| s.trim().parse::<u32>().ok());
        gpu.metal = display
            .get("spdisplays_metalfamily")
            .or_else(|| display.get("spdisplays_mtlgpufamilysupport"))
            .and_then(|v| v.as_str())
            .map(prettify_metal);
    }
    gpu
}

#[cfg(not(target_os = "macos"))]
fn probe_identity() -> ComputeGpu {
    // No portable GPU identity source off macOS; the block stays all-null until a
    // platform-specific probe is added.
    ComputeGpu::default()
}

/// Total physical memory in MB — the unified memory the Apple-Silicon GPU shares
/// with the system. `None` when indeterminate.
#[cfg(target_os = "macos")]
fn total_memory_mb() -> Option<u64> {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_memory();
    let bytes = sys.total_memory();
    if bytes == 0 {
        None
    } else {
        Some(((bytes as f64) / (1024.0 * 1024.0)).round() as u64)
    }
}

/// Parse the first `SPDisplaysDataType` entry from `system_profiler … -json` into
/// a JSON object, or `None` on any spawn / parse / shape failure.
#[cfg(target_os = "macos")]
fn first_display_json() -> Option<serde_json::Map<String, serde_json::Value>> {
    let out = std::process::Command::new("system_profiler")
        .arg("SPDisplaysDataType")
        .arg("-json")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("SPDisplaysDataType")?
        .as_array()?
        .first()?
        .as_object()
        .cloned()
}

/// Turn a `system_profiler` Metal token into a friendly label: `spdisplays_metal3`
/// → `Metal 3`, `spdisplays_metal` → `Metal`. An unrecognised shape passes through
/// trimmed.
#[cfg(target_os = "macos")]
fn prettify_metal(raw: &str) -> String {
    let s = raw.trim().trim_start_matches("spdisplays_");
    if let Some(rest) = s.strip_prefix("metal") {
        let n = rest.trim();
        if n.is_empty() {
            "Metal".to_string()
        } else {
            format!("Metal {n}")
        }
    } else {
        raw.trim().to_string()
    }
}

/// A short-cached utilisation sample: the instant it was taken and the value
/// (which is itself optional — a sample can fail).
#[cfg(target_os = "macos")]
type UtilSample = Option<(Instant, Option<f32>)>;

/// A short-cached live GPU utilisation percentage. The cache (a ~1 s TTL) bounds
/// how often `powermetrics` is spawned under a rapid poll. `None` when not
/// sampleable (off macOS, or any sample failure).
#[cfg(target_os = "macos")]
fn utilization() -> Option<f32> {
    const TTL: Duration = Duration::from_millis(1000);
    static CACHE: OnceLock<Mutex<UtilSample>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));

    if let Ok(guard) = cache.lock() {
        if let Some((at, val)) = guard.as_ref() {
            if at.elapsed() < TTL {
                return *val;
            }
        }
    }
    let sampled = sample_gpu_residency();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), sampled));
    }
    sampled
}

#[cfg(not(target_os = "macos"))]
fn utilization() -> Option<f32> {
    None
}

/// Run `powermetrics` once and parse the GPU active-residency percentage.
/// `powermetrics` requires root; on the dev Mac passwordless sudo is configured,
/// so this uses `sudo -n` (non-interactive) and returns `None` if sudo is denied,
/// the tool is missing, or the residency line is absent — never a fabricated
/// value, never a blocking prompt.
#[cfg(target_os = "macos")]
fn sample_gpu_residency() -> Option<f32> {
    let out = std::process::Command::new("sudo")
        .args([
            "-n",
            "powermetrics",
            "--samplers",
            "gpu_power",
            "-i",
            "200",
            "-n",
            "1",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_gpu_residency(&text)
}

/// Parse the "GPU HW active residency" percentage out of a `powermetrics
/// --samplers gpu_power` block. Returns the residency percentage (the first
/// number before `%` on that line), or `None` if the line is absent /
/// unparseable. Pure (platform-agnostic) so it is unit-tested on any host.
fn parse_gpu_residency(text: &str) -> Option<f32> {
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("gpu") && lower.contains("active residency") {
            let after = line.split(':').nth(1)?;
            return parse_first_percent(after);
        }
    }
    None
}

/// Parse the first percentage in a string like `  12.50% (396 MHz: 12% …)`: the
/// number up to the first `%`. `None` when it does not parse.
fn parse_first_percent(s: &str) -> Option<f32> {
    let pct = s.split('%').next()?.trim();
    pct.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_residency_line() {
        let block = "\
**** GPU usage ****
GPU HW active frequency: 396 MHz
GPU HW active residency:  12.50% (396 MHz: 12% 528 MHz:  0%)
GPU idle residency:  87.50%
GPU Power: 45 mW
";
        assert_eq!(parse_gpu_residency(block), Some(12.5));
    }

    #[test]
    fn residency_line_with_no_decimal() {
        let block = "GPU HW active residency: 100% (...)";
        assert_eq!(parse_gpu_residency(block), Some(100.0));
    }

    #[test]
    fn missing_residency_line_is_none() {
        let block = "GPU Power: 45 mW\nGPU idle residency: 87.50%";
        assert_eq!(parse_gpu_residency(block), None);
    }

    #[test]
    fn first_percent_parses_and_rejects_garbage() {
        assert_eq!(parse_first_percent(" 12.50% (extra)"), Some(12.5));
        assert_eq!(parse_first_percent("  0%"), Some(0.0));
        assert_eq!(parse_first_percent("n/a"), None);
    }

    #[test]
    fn sample_never_panics_and_is_self_consistent() {
        // Off macOS this is all-null; on macOS it is best-effort (util may be
        // null when powermetrics needs sudo it lacks). Either way it must not panic
        // and a present utilisation is a sane percentage.
        let gpu = sample();
        if let Some(u) = gpu.utilization_pct {
            assert!((0.0..=100.0).contains(&u), "utilisation in range: {u}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prettifies_metal_tokens() {
        assert_eq!(prettify_metal("spdisplays_metal3"), "Metal 3");
        assert_eq!(prettify_metal("spdisplays_metal"), "Metal");
        assert_eq!(prettify_metal("Metal 3"), "Metal 3");
    }
}
