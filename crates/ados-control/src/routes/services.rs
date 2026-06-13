//! Service-inventory route: the live `ados-*.service` unit list.
//!
//! `GET /api/services` reports every agent service the dashboard renders, with
//! its systemd state and a per-service memory figure.
//!
//! ## Why this surface serves the systemd view
//!
//! The FastAPI route first consults an in-process service tracker (the asyncio
//! task list + a transition log of the single supervisor process), and only
//! falls back to systemd's view of every `ados-*` unit when that tracker has no
//! actionable entries. The native control surface is a separate daemon with no
//! such in-process tracker, so the actionable-tracker branch is empty here and
//! the route serves the systemd-fallback shape: the live unit list from
//! `systemctl list-units`. That is the exact same `{name, active, state,
//! sub_state, pid, load_state}` shape the FastAPI route emits on its fallback
//! path, so the GCS sees an identical body either way.
//!
//! ## Parity contract
//!
//! - **`services`** — one object per `ados-*.service` unit:
//!   `{name, active, state, sub_state, pid, load_state, memory_mb}`. `name` is
//!   the unit basename with `.service` stripped, `active` is `state == "active"`,
//!   `state`/`sub_state`/`load_state` are the systemd columns, `pid` is always
//!   `null` (the fallback path does not resolve MainPIDs), and `memory_mb` is the
//!   unit's grouped PSS in MiB.
//! - **`systemd_available`** — `false` only when `systemctl` itself could not be
//!   reached (binary missing, spawn error); `true` when systemd answered even if
//!   no `ados-*` unit exists (an empty `services` list). The dashboard renders a
//!   different empty state for each case.
//! - **`process`** — `{pid, cpu_percent, memory_mb}` for the serving process. The
//!   CPU figure is `0.0` (the FastAPI route's per-request psutil sampler reports
//!   `0.0` on its first observation, which is the only observation a stateless
//!   request makes), and `memory_mb` is this process's RSS in MiB rounded to one
//!   decimal.
//!
//! Every external read degrades in place: an absent `systemctl` yields an empty
//! list with `systemd_available:false`, an unreadable `/proc` yields `0.0`
//! memory, and a non-Linux dev host yields an empty list + zero memory — never a
//! 500.

use std::collections::BTreeMap;
use std::process::Command;

use axum::Json;
use serde_json::{json, Value};

/// The unit glob the route asks systemd about. Covers every drone +
/// ground-station + agent unit on a stock install; a new `ados-*` unit is picked
/// up automatically with no change here. Mirrors the Python fallback pattern.
const SYSTEMD_FALLBACK_GLOB: &str = "ados-*.service";

/// `GET /api/services` → `{services, systemd_available, process}`.
///
/// Reads the live `ados-*.service` unit list from systemd, attaches each unit's
/// grouped PSS, and reports the serving process's own metrics. Guaranteed-200:
/// an absent `systemctl` degrades to an empty list with `systemd_available:false`
/// and an unreadable `/proc` degrades each unit's memory to `0.0`.
pub async fn list_services() -> Json<Value> {
    let (mut services, systemd_available) = systemd_inventory();

    // Resolve each entry's owning unit once, dedupe via the scan, write the
    // per-unit PSS back onto every entry. Entries whose unit has no running
    // process (or whose /proc is unreadable) land at 0.0 — the same value the
    // FastAPI live-scan fallback reports for an absent unit.
    attach_service_memory(&mut services);

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
/// only when `systemctl` itself could not be reached (binary missing, spawn
/// error), distinct from "systemd answered but no `ados-*` unit exists" (an empty
/// list with `available:true`).
///
/// Forces `SYSTEMD_COLORS=0` + an empty pager + `LANG=C` so the output is plain
/// columns: the default failed-unit rows are prefixed with a status glyph
/// (`● foo`, `× bar`), and a naive whitespace split would drop those rows — yet
/// failed units are exactly the rows the dashboard most needs. The leading glyph
/// is stripped before parsing so a failed unit still surfaces.
fn systemd_inventory() -> (Vec<Value>, bool) {
    let output = Command::new("systemctl")
        .args([
            "list-units",
            "--type=service",
            "--all",
            "--no-legend",
            "--no-pager",
            "--plain",
            SYSTEMD_FALLBACK_GLOB,
        ])
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_PAGER", "")
        .env("LANG", "C")
        .output();

    let stdout = match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        // Binary missing or spawn error: distinct from "no units" — the
        // dashboard renders a different empty state. Mirrors the Python
        // `except (SubprocessError, FileNotFoundError)` branch.
        Err(_) => return (Vec::new(), false),
    };

    let entries = stdout.lines().filter_map(parse_unit_line).collect();
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
/// fallback path. Mirrors the Python parser, including the four-column minimum.
fn parse_unit_line(line: &str) -> Option<Value> {
    let mut stripped = line.trim();
    if stripped.is_empty() {
        return None;
    }

    // Drop a leading status glyph. The first guard matches a non-alphanumeric
    // non-ASCII leading char (`●` U+25CF, `×` U+00D7); the second matches the
    // ASCII bullet/glyph set `* ● ×`. Either way the leading token is removed and
    // the remainder re-trimmed, matching the Python `split(None, 1)[-1]`.
    let first = stripped.chars().next()?;
    let is_glyph =
        (!first.is_alphanumeric() && !first.is_ascii()) || matches!(first, '*' | '●' | '×');
    if is_glyph {
        match stripped.split_once(char::is_whitespace) {
            Some((_, rest)) => stripped = rest.trim_start(),
            // No whitespace after the glyph: the Python code yields "" → skip.
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
/// process, get `0.0` — the same value the FastAPI live `/proc` scan reports for
/// an absent unit. A single scan serves every entry (units sharing a process are
/// summed once).
fn attach_service_memory(services: &mut [Value]) {
    // The unit each entry maps to (None for an unrecognised name).
    let unit_by_entry: Vec<Option<String>> = services
        .iter()
        .map(|s| {
            s.as_object()
                .and_then(|m| m.get("name"))
                .and_then(Value::as_str)
                .and_then(unit_for_service)
        })
        .collect();

    // One /proc PSS scan groups every running ados-* process by its cgroup unit.
    let pss_by_unit = scan_pss_by_unit();

    for (svc, unit) in services.iter_mut().zip(unit_by_entry.iter()) {
        let mb = unit
            .as_ref()
            .and_then(|u| pss_by_unit.get(u))
            .copied()
            .unwrap_or(0.0);
        if let Some(obj) = svc.as_object_mut() {
            obj.insert("memory_mb".to_string(), json!(mb));
        }
    }
}

/// Resolve a services-list entry name to its systemd unit, or `None`.
///
/// A unit-basename name (`ados-video`) maps to `<name>.service`; a short
/// in-process label (`video-pipeline`) maps through the fixed table below.
/// Anything else returns `None`. The systemd-fallback entries this route emits
/// all carry `ados-*` basenames, so they take the first branch; the short-label
/// table is carried for full parity with the FastAPI helper. Mirrors the Python
/// `unit_for_service`.
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

/// Map an in-process service short name onto the systemd unit that owns its
/// cgroup on a stock multi-process install. Names absent here have no dedicated
/// unit and resolve to `None`. Mirrors the Python `_SHORT_NAME_TO_UNIT` table.
fn short_name_to_unit(name: &str) -> Option<&'static str> {
    match name {
        "fc-connection" => Some("ados-mavlink.service"),
        "video-pipeline" => Some("ados-video.service"),
        "wfb-link" => Some("ados-wfb.service"),
        "rest-api" => Some("ados-api.service"),
        "health-monitor" => Some("ados-health.service"),
        "cloud-command-poll" => Some("ados-cloud.service"),
        "agent-heartbeat" => Some("ados-cloud.service"),
        "pairing-beacon" => Some("ados-cloud.service"),
        "pairing-heartbeat" => Some("ados-cloud.service"),
        "ota-updater" => Some("ados-ota.service"),
        _ => None,
    }
}

/// Sum PSS (MiB, one decimal) per `ados-*.service` unit across all running PIDs.
///
/// For each numeric `/proc/<pid>` entry it reads the owning unit from
/// `/proc/<pid>/cgroup` and the process's PSS from `/proc/<pid>/smaps_rollup`,
/// summing PSS per unit. PSS divides shared pages (one libpython mapped by
/// several services) fairly across the mappers, so the per-unit totals add up
/// sensibly and a multi-process unit is summed across its children. Best-effort
/// and never raises: an unreadable entry, a PID that exits mid-scan, or no read
/// permission skips that process and contributes nothing. On a non-Linux host
/// there is no `/proc`, so the scan yields an empty map and every unit lands at
/// `0.0`. Mirrors the Python `_scan_pss_by_unit`.
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
/// Matches the Python regex `(ados-[a-z0-9-]+\.service)`: a literal `ados-`
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
/// Returns `0` when the rollup has no `Pss:` line (older kernels) or it does not
/// parse to a number. Mirrors the Python `pss_kib_from_rollup`: the first
/// `Pss:`-prefixed line, second whitespace token, parsed as a non-negative
/// integer.
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
/// `pid` is this process's PID. `cpu_percent` is `0.0`: the FastAPI route reads
/// CPU through psutil's `cpu_percent(interval=0)`, which returns `0.0` on its
/// first observation against a fresh cache, and a stateless request makes exactly
/// that one observation. `memory_mb` is this process's resident set size in MiB,
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

/// Round to one decimal place, matching the Python `round(x, 1)` the memory paths
/// apply.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_running_service_row() {
        // A plain `--plain` row: unit load active sub description.
        let row = "ados-mavlink.service loaded active running ADOS MAVLink router";
        let svc = parse_unit_line(row).unwrap();
        assert_eq!(svc["name"], json!("ados-mavlink"));
        assert_eq!(svc["active"], json!(true));
        assert_eq!(svc["state"], json!("active"));
        assert_eq!(svc["sub_state"], json!("running"));
        assert_eq!(svc["load_state"], json!("loaded"));
        assert_eq!(svc["pid"], Value::Null);
    }

    #[test]
    fn parses_an_inactive_service_row() {
        let row = "ados-ota.service loaded inactive dead ADOS OTA updater";
        let svc = parse_unit_line(row).unwrap();
        assert_eq!(svc["name"], json!("ados-ota"));
        assert_eq!(svc["active"], json!(false));
        assert_eq!(svc["state"], json!("inactive"));
        assert_eq!(svc["sub_state"], json!("dead"));
    }

    #[test]
    fn strips_a_leading_failed_unit_glyph() {
        // systemd prepends a status glyph to failed units; the parser drops it.
        for glyph in ['×', '●', '*'] {
            let row = format!("{glyph} ados-video.service loaded failed failed ADOS Video");
            let svc = parse_unit_line(&row).unwrap();
            assert_eq!(svc["name"], json!("ados-video"), "glyph {glyph}");
            assert_eq!(svc["state"], json!("failed"));
            assert_eq!(svc["sub_state"], json!("failed"));
            assert_eq!(svc["load_state"], json!("loaded"));
        }
    }

    #[test]
    fn skips_blank_and_short_and_non_service_rows() {
        assert!(parse_unit_line("").is_none());
        assert!(parse_unit_line("   ").is_none());
        // Fewer than four columns.
        assert!(parse_unit_line("ados-x.service loaded").is_none());
        // A non-.service unit (e.g. a target glob hit) is filtered out.
        assert!(parse_unit_line("ados-thing.timer loaded active waiting A timer").is_none());
        // A glyph with nothing after it yields an empty remainder → skipped.
        assert!(parse_unit_line("×").is_none());
    }

    #[test]
    fn unit_for_service_maps_basenames_and_short_labels() {
        assert_eq!(
            unit_for_service("ados-video"),
            Some("ados-video.service".to_string())
        );
        assert_eq!(
            unit_for_service("ados-mavlink.service"),
            Some("ados-mavlink.service".to_string())
        );
        assert_eq!(
            unit_for_service("video-pipeline"),
            Some("ados-video.service".to_string())
        );
        assert_eq!(
            unit_for_service("pairing-beacon"),
            Some("ados-cloud.service".to_string())
        );
        assert_eq!(unit_for_service("not-a-thing"), None);
        assert_eq!(unit_for_service(""), None);
    }

    #[test]
    fn unit_from_cgroup_extracts_the_ados_unit() {
        let body = "0::/system.slice/ados.slice/ados-video.service\n";
        assert_eq!(
            unit_from_cgroup(body),
            Some("ados-video.service".to_string())
        );
        // A non-ados slice yields None.
        assert_eq!(unit_from_cgroup("0::/system.slice/sshd.service"), None);
        // `ados-` with no body or no `.service` suffix does not match.
        assert_eq!(unit_from_cgroup("ados-.service"), None);
        assert_eq!(unit_from_cgroup("ados-video"), None);
        assert_eq!(unit_from_cgroup(""), None);
    }

    #[test]
    fn pss_kib_from_rollup_reads_the_first_pss_line() {
        let body = "Rss:  12345 kB\nPss:  6789 kB\nShared_Clean:  100 kB\n";
        assert_eq!(pss_kib_from_rollup(body), 6789);
        // No Pss line → 0.
        assert_eq!(pss_kib_from_rollup("Rss:  100 kB\n"), 0);
        // A malformed Pss line → 0.
        assert_eq!(pss_kib_from_rollup("Pss:  notanumber kB\n"), 0);
        assert_eq!(pss_kib_from_rollup(""), 0);
    }

    #[test]
    fn attach_service_memory_writes_zero_when_no_proc_match() {
        // On a dev host the /proc scan finds no running ados unit, so every entry
        // gets 0.0 — the same value the Python live-scan fallback reports for an
        // absent unit. The field is always present.
        let mut services = vec![
            json!({"name": "ados-video", "active": true, "state": "active"}),
            json!({"name": "ados-ota", "active": false, "state": "inactive"}),
        ];
        attach_service_memory(&mut services);
        for svc in &services {
            assert!(svc.as_object().unwrap().contains_key("memory_mb"));
            assert!(svc["memory_mb"].is_number());
        }
    }

    #[test]
    fn round1_matches_python_rounding() {
        assert_eq!(round1(12.34), 12.3);
        assert_eq!(round1(12.35), 12.4);
        assert_eq!(round1(0.0), 0.0);
    }

    #[test]
    fn process_metrics_has_the_three_keys_with_correct_types() {
        let p = process_metrics();
        assert!(p["pid"].is_number());
        assert_eq!(p["cpu_percent"], json!(0.0));
        assert!(p["memory_mb"].is_number());
        // pid is this process; a sane PID is positive.
        assert!(p["pid"].as_u64().unwrap() > 0);
    }

    /// Golden-fixture parity: with no systemd unit running (a dev host, the only
    /// place these unit tests run), the route serves the systemd-fallback shape.
    /// The exact Python JSON for that case is:
    ///
    /// ```json
    /// {
    ///   "services": [ ... one object per ados-*.service unit ... ],
    ///   "systemd_available": true,   // false when systemctl is absent
    ///   "process": {"pid": <int>, "cpu_percent": 0.0, "memory_mb": <float>}
    /// }
    /// ```
    ///
    /// Each `services[i]` is exactly:
    /// `{name, active, state, sub_state, pid: null, load_state, memory_mb}`.
    ///
    /// The volatile parts (the live unit list, the PID, the RSS) are not
    /// byte-comparable across hosts, so this asserts the envelope keys + types +
    /// the per-entry object shape against the golden, which is the parity
    /// contract. A representative entry (built from the same `parse_unit_line` +
    /// `attach_service_memory` code the route runs) is asserted field-by-field.
    #[tokio::test]
    async fn route_envelope_matches_the_golden_shape() {
        let body = list_services().await.0;
        let obj = body.as_object().expect("top-level object");

        // The three top-level keys, exactly.
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["process", "services", "systemd_available"]);

        // services is an array.
        assert!(obj["services"].is_array(), "services must be an array");

        // systemd_available is a bool.
        assert!(obj["systemd_available"].is_boolean());

        // process is exactly the three-key block with the right types.
        let process = obj["process"].as_object().expect("process object");
        let mut pkeys: Vec<&str> = process.keys().map(String::as_str).collect();
        pkeys.sort_unstable();
        assert_eq!(pkeys, ["cpu_percent", "memory_mb", "pid"]);
        assert_eq!(process["cpu_percent"], json!(0.0));
        assert!(process["pid"].is_number());
        assert!(process["memory_mb"].is_number());

        // Build a representative service entry through the very same code path
        // the route uses, and assert it against the golden entry shape (the live
        // list may be empty on a dev host with no ados units running, so the
        // per-entry shape is pinned here deterministically).
        let mut entry =
            vec![
                parse_unit_line("ados-mavlink.service loaded active running ADOS MAVLink router")
                    .expect("a parseable row"),
            ];
        attach_service_memory(&mut entry);
        let golden = entry[0].as_object().unwrap();
        let mut ekeys: Vec<&str> = golden.keys().map(String::as_str).collect();
        ekeys.sort_unstable();
        assert_eq!(
            ekeys,
            [
                "active",
                "load_state",
                "memory_mb",
                "name",
                "pid",
                "state",
                "sub_state"
            ]
        );
        assert_eq!(golden["name"], json!("ados-mavlink"));
        assert_eq!(golden["active"], json!(true));
        assert_eq!(golden["state"], json!("active"));
        assert_eq!(golden["sub_state"], json!("running"));
        assert_eq!(golden["load_state"], json!("loaded"));
        assert_eq!(golden["pid"], Value::Null);
        assert!(golden["memory_mb"].is_number());
    }
}
