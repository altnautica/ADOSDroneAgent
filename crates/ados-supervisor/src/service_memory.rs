//! Per-service memory sampler, grouped by the systemd cgroup each process runs in.
//!
//! The agent is a fleet of long-running `ados-*.service` units. The obvious way
//! to get per-service memory is systemd's `MemoryCurrent` cgroup property, but
//! that needs the kernel **memory cgroup controller**, which is disabled by
//! default on Raspberry Pi OS (it requires `cgroup_enable=memory` on the boot
//! cmdline plus a reboot). On such a board `MemoryCurrent` reads `[not set]` for
//! every unit regardless of `MemoryAccounting=yes`.
//!
//! So this module derives per-service memory from `/proc` instead, which works
//! on every board with no boot parameter and no reboot: for each running PID it
//! reads the owning systemd unit from `/proc/<pid>/cgroup` and sums the
//! process's **PSS** (proportional set size) from `/proc/<pid>/smaps_rollup`.
//! PSS divides shared pages (one `libpython` mapped across several Python
//! services) fairly across the processes that map them, so the per-service
//! totals add up sensibly and a multi-process unit (the video orchestrator plus
//! its ffmpeg and mediamtx children) is summed correctly.
//!
//! This is the durable counterpart to the process-local PSS scan the FastAPI
//! `/api/services` route runs on demand: the supervisor runs as root with the
//! whole `/proc` tree readable and a steady polling cadence, so it samples the
//! same grouped sum continuously and ships one metric per unit to the logging
//! daemon. The route then reads the latest per-unit value back from the durable
//! store, with the live scan as the fallback when the store has no sample yet.
//!
//! Everything here is best-effort and never raises: an unreadable `/proc` entry,
//! a PID that exits mid-scan, or no read permission all resolve to skipping that
//! process. The sum is a saturating add so a pathological reading can never
//! overflow, and the scan is bounded to whatever PIDs `/proc` lists at the
//! instant it runs.

use std::collections::BTreeMap;
use std::time::Duration;

use ados_protocol::logd::emitter::IngestEmitter;
use ados_protocol::logd::{Fields, Value};
use tokio::sync::watch;

/// The dotted metric key carried per unit. One row per unit per sample, tagged
/// with the owning unit name so the reader can rebuild the per-service map.
pub const METRIC_MEMORY_PSS_BYTES: &str = "service.memory_pss_bytes";

/// The tag key naming the owning systemd unit on each sample.
pub const TAG_UNIT: &str = "unit";

/// How often the sampler re-scans `/proc`. Matches the supervisor's monitor tick
/// cadence so the durable per-service series tracks the fleet at the same rate
/// the live scan would when polled, without adding a hot loop.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Extract the `ados-*.service` unit from a `/proc/<pid>/cgroup` body.
///
/// Pure + testable. Returns `None` when no ados unit appears (the process
/// belongs to some other slice, or to no unit at all). The match is the first
/// `ados-<name>.service` token on any cgroup line, which covers both the unified
/// `0::/system.slice/ados.slice/ados-video.service` form and a v1-style
/// multi-line controller file.
pub fn unit_from_cgroup(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(unit) = first_ados_unit(line) {
            return Some(unit);
        }
    }
    None
}

/// Find the first `ados-<name>.service` token in one line, where `<name>` is a
/// run of `[a-z0-9-]`. Pure helper kept separate so the scan logic is testable
/// without a regex dependency.
fn first_ados_unit(line: &str) -> Option<String> {
    const PREFIX: &str = "ados-";
    const SUFFIX: &str = ".service";
    let bytes = line.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(PREFIX) {
        let start = search_from + rel;
        // Walk forward over the unit-name body (`[a-z0-9-]`) after the prefix.
        let mut end = start + PREFIX.len();
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' {
                end += 1;
            } else {
                break;
            }
        }
        let candidate = &line[start..end];
        // A valid token is `ados-<name>` immediately followed by `.service`.
        if end < bytes.len() && candidate.len() > PREFIX.len() && line[end..].starts_with(SUFFIX) {
            return Some(format!("{candidate}{SUFFIX}"));
        }
        // No match at this prefix occurrence; resume the search just past it so a
        // non-matching `ados-` earlier on the line cannot wedge the scan.
        search_from = start + PREFIX.len();
    }
    None
}

/// Parse the `Pss:` line out of a `/proc/<pid>/smaps_rollup` body (KiB).
///
/// Pure + testable. Returns 0 when the rollup has no `Pss:` line (older kernels) or it
/// does not parse.
pub fn pss_kib_from_rollup(text: &str) -> u64 {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Pss:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|tok| tok.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    0
}

/// Sum PSS (bytes) per ados unit across all running PIDs by reading `/proc`.
///
/// Returns a unit → byte-total map. Units with no readable PSS simply do not
/// appear; the caller decides how to render a missing unit. Best-effort: any
/// unreadable entry is skipped. The KiB→byte conversion is exact (`* 1024`);
/// the reader converts bytes→MiB the same way the live route does so the two
/// paths agree to the same rounding.
#[cfg(target_os = "linux")]
pub fn scan_pss_by_unit() -> BTreeMap<String, u64> {
    scan_pss_by_unit_in(std::path::Path::new("/proc"))
}

/// The non-Linux build has no `/proc`; the sampler is inert (an empty map), so
/// the supervisor still compiles and unit-tests on a dev host.
#[cfg(not(target_os = "linux"))]
pub fn scan_pss_by_unit() -> BTreeMap<String, u64> {
    BTreeMap::new()
}

/// Scan a `/proc`-shaped directory and sum PSS bytes per ados unit. Factored out
/// of [`scan_pss_by_unit`] so a test can point it at a fixture tree.
pub fn scan_pss_by_unit_in(proc_root: &std::path::Path) -> BTreeMap<String, u64> {
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return totals;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        // Only numeric PID directories carry the per-process files.
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let pid_dir = proc_root.join(pid);
        let Ok(cgroup) = std::fs::read_to_string(pid_dir.join("cgroup")) else {
            continue;
        };
        let Some(unit) = unit_from_cgroup(&cgroup) else {
            continue;
        };
        let Ok(rollup) = std::fs::read_to_string(pid_dir.join("smaps_rollup")) else {
            // PID exited mid-scan, or no read permission: skip.
            continue;
        };
        let pss_kib = pss_kib_from_rollup(&rollup);
        if pss_kib == 0 {
            continue;
        }
        let bytes = pss_kib.saturating_mul(1024);
        let total = totals.entry(unit).or_insert(0);
        *total = total.saturating_add(bytes);
    }
    totals
}

/// Emit one `service.memory_pss_bytes` sample per unit through `emitter`.
///
/// Factored out of the run loop so a test can assert the emit shape without a
/// timer. Best-effort: each sample is a non-blocking enqueue on the shared
/// shipper; a full channel or an absent daemon drops it without disturbing the
/// supervisor.
pub fn emit_samples(emitter: &IngestEmitter, totals: &BTreeMap<String, u64>) {
    for (unit, bytes) in totals {
        let mut tags = Fields::new();
        tags.insert(TAG_UNIT.to_string(), Value::from(unit.as_str()));
        emitter.emit_metric(METRIC_MEMORY_PSS_BYTES, *bytes as f64, tags);
    }
}

/// Run the per-service memory sampler until shutdown.
///
/// Scans `/proc` on a steady cadence and ships one `service.memory_pss_bytes`
/// metric per ados unit to the logging daemon. The `/proc` scan is synchronous
/// and brief (a few dozen small reads); it runs on a blocking-aware tick so it
/// never holds the supervisor's serial monitor loop, and a slow disk read can
/// never stall service orchestration. Exits promptly when `shutdown` flips true.
pub async fn run(shutdown: watch::Receiver<bool>) {
    // The shared shipper is best-effort: an absent logging daemon backs off
    // quietly, so constructing the emitter never fails and never blocks.
    let emitter = IngestEmitter::new("ados-supervisor");
    let mut shutdown = shutdown;
    let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                // Run the synchronous /proc scan off the async reactor so a slow
                // read never stalls other supervisor tasks on the same runtime.
                let totals = tokio::task::spawn_blocking(scan_pss_by_unit)
                    .await
                    .unwrap_or_default();
                emit_samples(&emitter, &totals);
            }
            res = shutdown.changed() => {
                // A closed channel (sender dropped) or an explicit `true` both end
                // the sampler so shutdown is prompt.
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
