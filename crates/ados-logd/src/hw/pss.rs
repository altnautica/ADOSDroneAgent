//! Per-service proportional set size (PSS) sampler.
//!
//! Resident set size double-counts shared pages (every process that maps libc
//! claims the whole library), so a per-service memory figure built from RSS sums
//! to more than the box has. PSS — the proportional set size from
//! `/proc/<pid>/smaps_rollup` — divides each shared page's cost across the
//! processes that map it, so the per-service figures sum to the real footprint.
//! Capturing PSS per ados service over time turns "which service grew before the
//! box hit the memory wall" from a live reproduction into a query.
//!
//! The reader has two halves with different testability:
//!
//! - the parse ([`parse_pss_kib`]) and the sampler ([`sample_service_pss_in`])
//!   are pure file IO over an injectable root, so they unit-test over a temp
//!   tree the same way the `/proc/meminfo` and `/sys/class/net` readers do;
//! - the unit→pid resolver ([`resolve_ados_unit_pids`]) shells out to
//!   `systemctl`, so it is Linux-gated, bounded, best-effort (an empty vec on any
//!   failure), and exercised by integration rather than a unit test — the same
//!   posture the `iw reg get` reader takes for its subprocess.
//!
//! The production resolvers are `#[cfg(target_os = "linux")]`: off Linux there is
//! no `/proc/<pid>/smaps_rollup` and no systemd, so both return empty and the
//! collector simply records no per-service memory for the tick.

use std::path::Path;
use std::time::Duration;

/// One service's proportional memory: the unit name and its PSS in KiB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceMemory {
    /// The systemd unit name without the `.service` suffix (e.g. `ados-video`).
    pub name: String,
    /// Proportional set size in kibibytes, from `/proc/<pid>/smaps_rollup`.
    pub pss_kib: u64,
}

/// How long `systemctl` is allowed to run before it is killed. The query is
/// cheap; the bound guards against a hung systemd / dbus call so a wedged
/// `systemctl` can never stall the sampling cadence.
pub const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(2);

/// The core ados service units sampled for per-service memory. Best-effort: a
/// unit that is not installed or not running on a given profile simply yields no
/// MainPID and drops out of the sample. Kept as a small static list local to the
/// collector so the logging daemon carries no dependency on the installer's unit
/// catalog.
pub const ADOS_UNITS: &[&str] = &[
    "ados-supervisor",
    "ados-mavlink",
    "ados-control",
    "ados-video",
    "ados-radio",
    "ados-cloud",
    "ados-logd",
    "ados-net",
    "ados-plugin-host",
    "ados-vision",
    "ados-display",
    "ados-groundlink",
];

/// Parse the `Pss:` value (in KiB) out of a `/proc/<pid>/smaps_rollup` body.
///
/// The rollup file has one aggregate `Pss:` line, e.g. `Pss:   1234 kB`; the
/// value is already in kibibytes, so it is returned as-is. Returns `None` when no
/// `Pss:` line is present or its value does not parse. Pure, so the parse is
/// unit-testable without `/proc`.
pub fn parse_pss_kib(smaps_rollup: &str) -> Option<u64> {
    for line in smaps_rollup.lines() {
        let Some(rest) = line.strip_prefix("Pss:") else {
            continue;
        };
        // The value is the first whitespace-separated token after the colon, in
        // kibibytes (the trailing `kB` unit is dropped).
        if let Some(kib) = rest.split_whitespace().next().and_then(|v| v.parse().ok()) {
            return Some(kib);
        }
    }
    None
}

/// Sample PSS for each `(unit_name, pid)` against the proc tree rooted at `root`,
/// reading `<root>/<pid>/smaps_rollup`. A unit whose rollup file is absent or
/// carries no parseable `Pss:` line is dropped, so only the present services are
/// collected. The root is injectable so the sampler tests over a temp tree.
pub fn sample_service_pss_in(root: &Path, units: &[(String, u32)]) -> Vec<ServiceMemory> {
    let mut out = Vec::with_capacity(units.len());
    for (name, pid) in units {
        let path = root.join(pid.to_string()).join("smaps_rollup");
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(pss_kib) = parse_pss_kib(&body) {
            out.push(ServiceMemory {
                name: name.clone(),
                pss_kib,
            });
        }
    }
    out
}

/// Sample PSS for each `(unit_name, pid)` from the live `/proc`. The production
/// entry point; delegates to [`sample_service_pss_in`] with the real proc root.
pub fn sample_service_pss(units: &[(String, u32)]) -> Vec<ServiceMemory> {
    sample_service_pss_in(Path::new("/proc"), units)
}

/// Resolve the running ados service units to their MainPID via systemd.
///
/// Shells out to `systemctl show --property=MainPID --value <unit>` once per core
/// unit and keeps the units that report a non-zero MainPID (a stopped / not
/// installed unit reports `0`). Thin and best-effort: any spawn / exit / timeout
/// failure yields an empty vec, never a panic, so a board without systemd or with
/// a hung `systemctl` simply contributes no per-service memory for the tick.
///
/// Linux-gated: off Linux there is no systemd, so this returns empty.
#[cfg(target_os = "linux")]
pub async fn resolve_ados_unit_pids() -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for unit in ADOS_UNITS {
        if let Some(pid) = main_pid_of(unit).await {
            out.push(((*unit).to_string(), pid));
        }
    }
    out
}

/// Off-Linux there is no systemd: no units to resolve.
#[cfg(not(target_os = "linux"))]
pub async fn resolve_ados_unit_pids() -> Vec<(String, u32)> {
    Vec::new()
}

/// Query one unit's MainPID via `systemctl show`. Returns `Some(pid)` only for a
/// running unit (non-zero MainPID); `None` on a stopped/absent unit or any
/// spawn / exit / timeout failure.
#[cfg(target_os = "linux")]
async fn main_pid_of(unit: &str) -> Option<u32> {
    use std::process::Stdio;
    use tokio::process::Command;
    use tokio::time::timeout;

    let child = Command::new("systemctl")
        .args(["show", "--property=MainPID", "--value", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;

    let output = match timeout(SYSTEMCTL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => output,
        // Non-zero exit, spawn-side IO error, or the timeout elapsed (the child
        // is killed on drop): no PID for this unit.
        _ => return None,
    };
    let pid: u32 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
    // A stopped or not-installed unit reports MainPID 0; skip it.
    (pid != 0).then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn parses_pss_from_a_rollup_body() {
        let body = "\
Rss:                4096 kB
Pss:                1234 kB
Pss_Anon:            900 kB
Shared_Clean:        100 kB
";
        assert_eq!(parse_pss_kib(body), Some(1234));
    }

    #[test]
    fn pss_is_none_when_the_line_is_absent() {
        let body = "Rss:   4096 kB\nShared_Clean:   100 kB\n";
        assert_eq!(parse_pss_kib(body), None);
        // An empty body has no Pss line either.
        assert_eq!(parse_pss_kib(""), None);
    }

    #[test]
    fn pss_is_none_for_an_unparseable_value() {
        // A malformed value (no number) yields None rather than a wrong reading.
        assert_eq!(parse_pss_kib("Pss:   notanumber kB\n"), None);
    }

    #[test]
    fn samples_present_services_and_drops_absent_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "101/smaps_rollup", "Pss:   500 kB\n");
        write(root, "202/smaps_rollup", "Rss: 4096 kB\nPss:   2048 kB\n");
        // 303 has a rollup with no Pss line; it must be dropped.
        write(root, "303/smaps_rollup", "Rss: 100 kB\n");
        let units = vec![
            ("ados-supervisor".to_string(), 101u32),
            ("ados-video".to_string(), 202u32),
            ("ados-radio".to_string(), 303u32),
            // 404 has no rollup file at all; it must be dropped.
            ("ados-cloud".to_string(), 404u32),
        ];
        let sampled = sample_service_pss_in(root, &units);
        assert_eq!(
            sampled,
            vec![
                ServiceMemory {
                    name: "ados-supervisor".to_string(),
                    pss_kib: 500,
                },
                ServiceMemory {
                    name: "ados-video".to_string(),
                    pss_kib: 2048,
                },
            ]
        );
    }

    #[test]
    fn empty_units_yields_no_samples() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sample_service_pss_in(dir.path(), &[]).is_empty());
    }

    #[tokio::test]
    async fn resolve_is_graceful_when_systemctl_is_absent() {
        // On CI / a dev host without systemd (or off Linux), the resolver returns
        // a vec without panicking or aborting. This is a smoke test of the
        // best-effort path; it does not assert any specific units.
        let _pids = resolve_ados_unit_pids().await;
    }
}
