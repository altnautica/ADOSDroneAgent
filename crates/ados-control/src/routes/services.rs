//! Service-inventory route: the live `ados-*.service` unit list.
//!
//! `GET /api/services` reports every agent service the dashboard renders, with
//! its systemd state and a per-service memory figure.
//!
//! ## Why this surface serves the systemd view
//!
//! The control surface is a separate daemon with no in-process service tracker,
//! so the route serves systemd's view of every `ados-*` unit: the live unit list
//! from `systemctl list-units`, as `{name, active, state, sub_state, pid,
//! load_state}` objects.
//!
//! ## Wire contract
//!
//! - **`services`** — one object per `ados-*.service` unit:
//!   `{name, active, state, sub_state, pid, load_state, memory_mb}`. `name` is
//!   the unit basename with `.service` stripped, `active` is `state == "active"`,
//!   `state`/`sub_state`/`load_state` are the systemd columns, `pid` is always
//!   `null` (the fallback path does not resolve MainPIDs), and `memory_mb` is the
//!   unit's grouped PSS in MiB.
//! - **`systemd_available`** — `false` only when `systemctl` itself could not be
//!   reached (binary missing, spawn error, unreachable bus, or a probe that timed
//!   out); `true` when systemd answered even if no `ados-*` unit exists (an empty
//!   `services` list). The dashboard renders a different empty state for each
//!   case.
//! - **`process`** — `{pid, cpu_percent, memory_mb}` for the serving process. The
//!   CPU figure is `0.0` (a stateless request has no prior sample to diff
//!   against), and `memory_mb` is this process's RSS in MiB rounded to one
//!   decimal.
//!
//! Every external read degrades in place: an absent `systemctl` yields an empty
//! list with `systemd_available:false`, an unreadable `/proc` yields `0.0`
//! memory, and a non-Linux dev host yields an empty list + zero memory — never a
//! 500.

use std::collections::BTreeMap;

use axum::Json;
use serde_json::{json, Value};

/// The unit glob the route asks systemd about. Covers every drone +
/// ground-station + agent unit on a stock install; a new `ados-*` unit is picked
/// up automatically with no change here.
const SYSTEMD_FALLBACK_GLOB: &str = "ados-*.service";

/// `GET /api/services` → `{services, systemd_available, process}`.
///
/// Reads the live `ados-*.service` unit list from systemd, attaches each unit's
/// grouped PSS, and reports the serving process's own metrics. Guaranteed-200:
/// an absent `systemctl` degrades to an empty list with `systemd_available:false`
/// and a unit whose memory the `/proc` scan could not measure reports `null`.
pub async fn list_services() -> Json<Value> {
    let (mut services, systemd_available) = systemd_inventory().await;

    // Resolve each entry's owning unit, write the per-unit PSS back onto every
    // entry; a unit the scan measured nothing for is null.
    attach_service_memory(&mut services).await;

    let process = process_metrics();

    Json(json!({
        "services": services,
        "systemd_available": systemd_available,
        "process": process,
    }))
}

/// Read the live `ados-*.service` unit list from systemd.
///
/// Returns `(entries, available)`. `entries` is the list of service objects (each
/// without `memory_mb` yet — the caller attaches that). `available` is `false`
/// when `systemctl` could not be reached, distinct from "systemd answered but no
/// `ados-*` unit exists" (an empty list with `available:true`).
///
/// The probe forces `SYSTEMD_COLORS=0` + an empty pager + `LANG=C` (see
/// [`crate::probe::capture_systemctl`], which owns those three variables now) so
/// the output is plain columns: the default failed-unit rows are prefixed with a
/// status glyph (`● foo`, `× bar`), and a naive whitespace split would drop those
/// rows — yet failed units are exactly the rows the dashboard most needs. The
/// leading glyph is stripped before parsing so a failed unit still surfaces.
///
/// `available:false` now also covers a non-zero exit and a timeout, which the
/// blocking version reported as `available:true` with empty stdout.
/// [`crate::probe::ProbeOutput`] cannot express the distinction, and folding them
/// in is the reading that keeps the field honest: `list-units` with a glob that
/// matches nothing still exits zero, so a non-zero exit means the bus could not
/// be queried — which is what `available:false` already means to the dashboard.
async fn systemd_inventory() -> (Vec<Value>, bool) {
    let out = crate::probe::capture_systemctl(
        &[
            "list-units",
            "--type=service",
            "--all",
            "--no-legend",
            "--no-pager",
            "--plain",
            SYSTEMD_FALLBACK_GLOB,
        ],
        crate::probe::PROBE_TIMEOUT,
    )
    .await;

    // Unreachable systemd: distinct from "no units" — the dashboard renders a
    // different empty state. A spawn failure, a missing binary, and a probe
    // timeout all land here.
    if !out.is_ok() {
        return (Vec::new(), false);
    }

    let entries = out.text().lines().filter_map(parse_unit_line).collect();
    (entries, true)
}

/// Parse one `systemctl list-units` row into a service object, or `None` when the
/// row is blank, has too few columns, or is not a `.service` unit.
///
/// Strips any leading status glyph systemd prepends to non-running units (a
/// non-alphanumeric non-ASCII char like `●`/`×`, or one of `*●×`), then splits on
/// whitespace runs into the columns `unit load active sub description`. The first
/// four are taken; the unit basename has `.service` stripped to form `name`.
/// `active` is `active_state == "active"`, `pid` is always `null` on this
/// fallback path. Lines with fewer than four columns are skipped.
pub(crate) fn parse_unit_line(line: &str) -> Option<Value> {
    let mut stripped = line.trim();
    if stripped.is_empty() {
        return None;
    }

    // Drop a leading status glyph. The first guard matches a non-alphanumeric
    // non-ASCII leading char (`●` U+25CF, `×` U+00D7); the second matches the
    // ASCII bullet/glyph set `* ● ×`. Either way the leading token is removed and
    // the remainder re-trimmed.
    let first = stripped.chars().next()?;
    let is_glyph =
        (!first.is_alphanumeric() && !first.is_ascii()) || matches!(first, '*' | '●' | '×');
    if is_glyph {
        match stripped.split_once(char::is_whitespace) {
            Some((_, rest)) => stripped = rest.trim_start(),
            // No whitespace after the glyph: nothing left → skip.
            None => return None,
        }
    }

    // Split on runs of whitespace into columns, matching `str.split()` semantics
    // (repeated separators collapse, leading/trailing trimmed).
    let cols: Vec<&str> = stripped.split_whitespace().collect();
    if cols.len() < 4 {
        return None;
    }

    let unit = cols[0];
    let load_state = cols[1];
    let active_state = cols[2];
    let sub_state = cols[3];

    let suffix = ".service";
    if !unit.ends_with(suffix) {
        return None;
    }
    let name = &unit[..unit.len() - suffix.len()];

    Some(json!({
        "name": name,
        "active": active_state == "active",
        "state": active_state,
        "sub_state": sub_state,
        "pid": Value::Null,
        "load_state": load_state,
    }))
}

/// Attach a `memory_mb` field to every service entry, in place.
///
/// Resolves each entry's owning systemd unit, scans `/proc` once to sum each
/// unit's grouped PSS, and writes the MiB figure back onto every entry that maps
/// to that unit. Entries with no resolvable unit, or a unit with no running
/// process, get `0.0`. A single scan serves every entry (units sharing a process are
/// summed once).
pub(crate) async fn attach_service_memory(services: &mut [Value]) {
    // One /proc PSS scan groups every running ados-* process by its cgroup unit.
    // It reads two files per PID, so it runs on the blocking pool rather than on
    // the worker every other request shares.
    let pss_by_unit = crate::probe::offload(scan_pss_by_unit)
        .await
        .unwrap_or_default();
    apply_service_memory(services, &pss_by_unit);
}

/// Write each entry's `memory_mb` from a scan: the unit's summed PSS, or `null`
/// when the scan measured nothing for it (no running process, an unreadable
/// rollup, no `/proc`). A `0.0` would read as a measured empty unit.
pub(crate) fn apply_service_memory(services: &mut [Value], pss_by_unit: &BTreeMap<String, f64>) {
    for svc in services.iter_mut() {
        let mb = svc
            .get("name")
            .and_then(Value::as_str)
            .and_then(unit_for_service)
            .and_then(|u| pss_by_unit.get(&u).copied());
        if let Some(obj) = svc.as_object_mut() {
            obj.insert(
                "memory_mb".to_string(),
                mb.map(Value::from).unwrap_or(Value::Null),
            );
        }
    }
}

/// Resolve a services-list entry name to its systemd unit, or `None`.
///
/// A unit-basename name (`ados-video`) maps to `<name>.service`; a short in-process
/// label (`video-pipeline`) maps through the fixed table below. Anything else returns
/// `None`. The systemd-fallback entries this route emits all carry `ados-*` basenames,
/// so they take the first branch; the short-label table is carried for the older
/// in-process labels a stored entry can still name.
fn unit_for_service(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if name.starts_with("ados-") {
        return Some(if name.ends_with(".service") {
            name.to_string()
        } else {
            format!("{name}.service")
        });
    }
    short_name_to_unit(name).map(str::to_string)
}

/// Map an in-process service short name onto the unit-table row that owns its
/// cgroup on a stock multi-process install. Names absent here have no
/// dedicated unit and resolve to `None`.
fn short_name_to_unit(name: &str) -> Option<&'static str> {
    let stem = match name {
        "fc-connection" => "ados-mavlink",
        "video-pipeline" => "ados-video",
        "wfb-link" => "ados-wfb",
        "rest-api" => "ados-api",
        "cloud-command-poll" | "agent-heartbeat" | "pairing-beacon" | "pairing-heartbeat" => {
            "ados-cloud"
        }
        _ => return None,
    };
    ados_protocol::units::find(stem).map(|u| u.name)
}

/// Sum PSS (MiB, one decimal) per `ados-*.service` unit across all running PIDs.
///
/// For each numeric `/proc/<pid>` entry it reads the owning unit from
/// `/proc/<pid>/cgroup` and the process's PSS from `/proc/<pid>/smaps_rollup`, summing
/// PSS per unit. PSS divides shared pages (one libpython mapped by several services)
/// fairly across the mappers, so the per-unit totals add up sensibly and a
/// multi-process unit is summed across its children. Best-effort and never raises: an
/// unreadable entry, a PID that exits mid-scan, or no read permission skips that
/// process and contributes nothing. On a non-Linux host there is no `/proc`, so the
/// scan yields an empty map and every unit lands at `0.0`.
fn scan_pss_by_unit() -> BTreeMap<String, f64> {
    let mut totals_kib: BTreeMap<String, u64> = BTreeMap::new();

    let dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return BTreeMap::new(),
    };

    for entry in dir.flatten() {
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str() else {
            continue;
        };
        // Only numeric PID directories.
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }

        let cgroup = match std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let Some(unit) = unit_from_cgroup(&cgroup) else {
            continue;
        };

        // PID may have exited mid-scan, or reading another process's rollup may
        // need root; either way skip and contribute nothing.
        let rollup = match std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let pss = pss_kib_from_rollup(&rollup);
        if pss > 0 {
            *totals_kib.entry(unit).or_insert(0) += pss;
        }
    }

    totals_kib
        .into_iter()
        .map(|(unit, kib)| (unit, round1(kib as f64 / 1024.0)))
        .collect()
}

/// Extract the `ados-*.service` unit from a `/proc/<pid>/cgroup` body, or `None`
/// when no ados unit appears (the process belongs to another slice or no unit).
///
/// Matches the pattern `(ados-[a-z0-9-]+\.service)`: a literal `ados-`
/// followed by one-or-more lowercase-alphanumeric-or-dash chars, then `.service`.
/// Scans the whole body and returns the first match. Pure + testable.
fn unit_from_cgroup(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let needle = b"ados-";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            // Greedily consume [a-z0-9-]+ after `ados-`.
            let mut j = i + needle.len();
            let body_start = j;
            while j < bytes.len() {
                let c = bytes[j];
                if c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' {
                    j += 1;
                } else {
                    break;
                }
            }
            // Need at least one body char, then a literal `.service`.
            if j > body_start && bytes[j..].starts_with(b".service") {
                let end = j + ".service".len();
                return Some(String::from_utf8_lossy(&bytes[i..end]).into_owned());
            }
        }
        i += 1;
    }
    None
}

/// Parse the `Pss:` line out of a `/proc/<pid>/smaps_rollup` body (KiB).
///
/// Returns `0` when the rollup has no `Pss:` line (older kernels) or it does not parse
/// to a number.
fn pss_kib_from_rollup(text: &str) -> u64 {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Pss:") {
            let mut tokens = rest.split_whitespace();
            return match tokens.next() {
                Some(tok) if !tok.is_empty() && tok.bytes().all(|b| b.is_ascii_digit()) => {
                    tok.parse::<u64>().unwrap_or(0)
                }
                _ => 0,
            };
        }
    }
    0
}

/// The serving process's metrics: `{pid, cpu_percent, memory_mb}`.
///
/// `pid` is this process's PID. `cpu_percent` is `0.0`: a stateless request has
/// no prior sample to diff against. `memory_mb` is this process's resident set size in MiB,
/// rounded to one decimal — the same field psutil's `memory_info().rss` reports,
/// read here from `/proc/self`. On a non-Linux dev host the RSS read is `0.0`.
fn process_metrics() -> Value {
    json!({
        "pid": std::process::id(),
        "cpu_percent": 0.0,
        "memory_mb": round1(self_rss_mb()),
    })
}

/// This process's resident set size in MiB, read from `/proc/self/statm`.
///
/// `statm`'s second field is the resident page count; multiplied by the page
/// size it is the RSS in bytes, the same value psutil's `memory_info().rss`
/// reports. Degrades to `0.0` when the file is absent / unparseable (a non-Linux
/// host, or a hardened `/proc`), never panicking.
#[cfg(target_os = "linux")]
fn self_rss_mb() -> f64 {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return 0.0;
    };
    // Fields: size resident shared text lib data dt — the second is resident
    // pages.
    let Some(resident_pages) = statm.split_whitespace().nth(1) else {
        return 0.0;
    };
    let Ok(pages) = resident_pages.parse::<u64>() else {
        return 0.0;
    };
    // Page size in bytes. 4096 is the universal default on the agent's boards;
    // read the live value so an exotic page size is still correct.
    let page_size = page_size_bytes();
    (pages as f64 * page_size as f64) / (1024.0 * 1024.0)
}

#[cfg(not(target_os = "linux"))]
fn self_rss_mb() -> f64 {
    0.0
}

/// The system page size in bytes via `sysconf(_SC_PAGESIZE)`, falling back to the
/// universal 4096 when the query fails. Linux-only (the only RSS caller).
#[cfg(target_os = "linux")]
fn page_size_bytes() -> u64 {
    // Safety: `sysconf` is a pure read of a system constant; a non-positive
    // return (the query is unsupported) falls back to the 4 KiB default.
    let rc = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) };
    if rc > 0 {
        rc as u64
    } else {
        4096
    }
}

/// Round to one decimal place for the memory figures.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests;
