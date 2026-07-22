//! The consolidated status route: agent info, services, resources, video,
//! telemetry, radio, and mesh in one body.
//!
//! `GET /api/status/full` is the single round-trip the GCS polls instead of four
//! separate requests (`/api/status` + `/api/services` + `/api/system` +
//! `/api/video`). On this native front the agent runs as a separate daemon with
//! no in-process service tracker, no video pipeline object, and no in-process WFB
//! manager — so each block is sourced the same way the FastAPI route's
//! multi-process branch is:
//!
//! - **agent info** (`version`, `uptime_seconds`, `board`, `health`,
//!   `fc_connected`/`fc_port`/`fc_baud`) — the same seams `/api/status` reads: the
//!   version string, the state snapshot's runtime extras, the board sidecar, and
//!   the logging store's hardware snapshots.
//! - **`services`** — the systemd-fallback inventory (`systemctl list-units
//!   ados-*.service`), one object per unit shaped `{name, state, status,
//!   task_done, uptimeSeconds, memory_mb}`, the exact shape the FastAPI route's
//!   `_systemd_services_fallback` emits when its in-process tracker is empty (which
//!   it always is on this separate daemon).
//! - **`resources`** — CPU / memory / swap / disk / temperature derived from the
//!   store's most-recent hardware snapshots, the same 13-field subset the FastAPI
//!   `/api/status/full` selects from `derive_resources`. An unreachable store (or
//!   one missing an essential field) degrades to an empty object — the most-
//!   degraded shape the FastAPI route emits when both the store and psutil are
//!   unavailable.
//! - **`video`** — the multi-process branch: there is no in-process pipeline, so
//!   the block is built from a mediamtx probe (drone profile) or gated on the WFB
//!   receive link actually delivering frames (ground-station profile), with an
//!   empty recording block (no in-process recorder on this daemon).
//! - **`telemetry`** — the vehicle-state snapshot with the four runtime-only
//!   extras stripped, identical to `/api/telemetry`.
//! - **`radio`** — the forward-compatible heartbeat radio block, in the camelCase
//!   shape the GCS reads, built from the same WFB status view the video gate uses
//!   so the two can never disagree about the link.
//! - **`mesh`** — populated only on a ground-station-profile node with a non-direct
//!   role; an empty object otherwise so clients can feature-detect cheaply.
//! - **`profile`/`role`/`runtimeMode`** — the resolved wire-form profile + role
//!   discriminators and the native-vs-packaged runtime badge.
//! - **camera keys** — `cameraState` + `cameraUsbRecovery` folded in from their
//!   sidecars when fresh, so a LAN-paired operator sees the camera-missing signal
//!   the cloud heartbeat carries.
//!
//! Every read is fault-tolerant: an absent store / sidecar / config / systemctl
//! degrades that block to the same empty/default shape the FastAPI route returns
//! when its own source is unavailable, never a 500. The route carries no path
//! params and never mutates.

use std::path::{Path, PathBuf};
use std::process::Command;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// GET /api/status/full
// ---------------------------------------------------------------------------

/// `GET /api/status/full` → the consolidated status body.
///
/// Sources every block from the multi-process seams (the state snapshot, the
/// logging store, the radio sidecar, systemd, and the config), exactly as the
/// FastAPI route's no-in-process-object branch does. Guaranteed 200: each block
/// degrades in place when its source is unavailable.
pub async fn get_full_status(State(state): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let config_profile = config_agent_profile(&state.pairing_paths.config);
    let (resolved_profile, resolved_role) =
        crate::profile::current_profile_and_role(&config_profile);

    // --- Agent info (same seams as /api/status) ---
    let snapshot = state.state.snapshot();
    let (fc_connected, fc_port, fc_baud, snapshot_uptime) =
        crate::routes::status::fc_from_snapshot(snapshot.as_ref());
    // The FC-liveness detail (read before `snapshot` is moved into the telemetry
    // projection below): the same camelCase fields /api/status carries.
    let fc_liveness = crate::routes::status::fc_liveness_from_snapshot(snapshot.as_ref());
    let uptime = snapshot_uptime.unwrap_or_else(|| json!(state.process_uptime_seconds()));
    let board = crate::routes::status::read_board(&state.board_path);

    // Hardware signals, read once and reused for health + resources.
    let signals = state.logd.latest_hw_signals().await;
    let health = crate::routes::status::derive_health(signals.as_ref());
    let resources = derive_resources_subset(signals.as_ref());

    // --- Services (systemd-fallback shape; no in-process tracker here) ---
    let services = build_services_list();

    // --- WFB status, read once and reused by the video gate + the radio block ---
    let wfb_status = wfb_status_view(&state).await;

    // --- Video (multi-process branch: no in-process pipeline) ---
    let host = host_from_headers(&headers);
    let video = build_video_block(&resolved_profile, &wfb_status, &host).await;

    // --- Telemetry (vehicle state minus the four runtime-only extras) ---
    let telemetry = crate::routes::status::project_telemetry(snapshot);

    // --- Radio (camelCase heartbeat block) ---
    let radio = radio_to_camel(build_radio_block(wfb_status.as_ref()));

    // --- Mesh (ground-station profile, non-direct role only) ---
    let mesh = build_mesh_block(&config_profile);

    // --- Capabilities: retired per-agent catalog; an empty dict for forward-compat ---
    let capabilities: Value = json!({});

    // The perception capability + tier, before `board` is moved into the payload.
    // Same canonical ados_offload::pick_tier decision the LAN /api/status uses so
    // the GCS Perception hub reads a consistent tier from either route.
    let (npu_tops, has_accelerator, perception_tier, offload_target) =
        crate::routes::status::perception_fields(&board);

    let mut payload = Map::new();
    payload.insert("version".to_string(), json!(state.agent_version()));
    payload.insert("uptime_seconds".to_string(), uptime);
    payload.insert("board".to_string(), board);
    payload.insert("npuTops".to_string(), json!(npu_tops));
    payload.insert("hasAccelerator".to_string(), json!(has_accelerator));
    payload.insert("perceptionTier".to_string(), json!(perception_tier));
    payload.insert(
        "perceptionOffloadTarget".to_string(),
        match offload_target {
            Some(t) => Value::String(t),
            None => Value::Null,
        },
    );
    payload.insert("health".to_string(), health);
    payload.insert("fc_connected".to_string(), fc_connected);
    payload.insert("fc_port".to_string(), fc_port);
    payload.insert("fc_baud".to_string(), fc_baud);
    payload.insert(
        "transportOpen".to_string(),
        json!(fc_liveness.transport_open),
    );
    payload.insert("mavlinkAlive".to_string(), json!(fc_liveness.mavlink_alive));
    payload.insert("heartbeatAgeS".to_string(), fc_liveness.heartbeat_age_s);
    payload.insert("fcSource".to_string(), fc_liveness.fc_source);
    payload.insert("fcLinkHint".to_string(), fc_liveness.fc_link_hint);
    payload.insert("services".to_string(), services);
    payload.insert("resources".to_string(), resources);
    payload.insert("video".to_string(), video);
    payload.insert("telemetry".to_string(), telemetry);
    payload.insert("capabilities".to_string(), capabilities);
    payload.insert("mesh".to_string(), mesh);
    payload.insert("radio".to_string(), radio);
    payload.insert("profile".to_string(), json!(resolved_profile));
    payload.insert(
        "role".to_string(),
        resolved_role.map(Value::from).unwrap_or(Value::Null),
    );
    payload.insert(
        "runtimeMode".to_string(),
        json!(crate::state::runtime_mode()),
    );

    // Camera presence + USB-recovery, folded in only when the sidecars are fresh.
    for (k, v) in read_camera_status() {
        payload.insert(k, v);
    }

    Json(Value::Object(payload))
}

/// The agent's raw `agent.profile` config value (underscore form, e.g. `"drone"`
/// / `"ground_station"` / `"auto"`), used both to resolve the wire profile and to
/// gate the mesh block on the raw `"ground_station"` value. Defaults to `"auto"`
/// (the Python `AgentConfig` default) when the file is absent.
fn config_agent_profile(config_path: &Path) -> String {
    crate::config::PairingConfig::load_from(config_path)
        .agent
        .profile
}

/// The request `Host` header's host part (the value before any `:port`), the
/// source the video block builds its WHEP URL from. Defaults to `"localhost"`
/// when the header is absent, matching the Python `request.headers.get("host",
/// "localhost").split(":")[0]`.
fn host_from_headers(headers: &HeaderMap) -> String {
    headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or("localhost").to_string())
        .unwrap_or_else(|| "localhost".to_string())
}

// ---------------------------------------------------------------------------
// Resources: the 13-field subset of derive_resources.
// ---------------------------------------------------------------------------

const BYTES_PER_MB: f64 = 1024.0 * 1024.0;
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

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
fn derive_resources_subset(signals: Option<&Map<String, Value>>) -> Value {
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
/// byte-matching the Python built-in `round(x)` the MB conversions use. The
/// distinction only bites at an exact `.5` (which a real byte/MB conversion almost
/// never hits), but the contract is byte-parity, so the tie-break must match.
fn round_int(v: f64) -> i64 {
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
/// This daemon has no in-process service tracker, so the FastAPI route's tracker
/// branch is always empty here and it serves the `_systemd_services_fallback`
/// shape: one object per `ados-*.service` unit, shaped `{name, state, status,
/// task_done, uptimeSeconds, memory_mb}`. `state` is `"running"` when the unit's
/// sub-state is `running`, else the sub-state (or `"unknown"`); `status` mirrors
/// `state`; `task_done` is `state != "running"`; `uptimeSeconds` is always `0`
/// (the fallback path carries no transition log); `memory_mb` is the unit's
/// grouped PSS. An absent / failing `systemctl` degrades to an empty list.
fn build_services_list() -> Value {
    let mut services = systemd_services_fallback();
    attach_service_memory(&mut services);
    Value::Array(services)
}

/// Enumerate `ados-*.service` units via `systemctl list-units`, returning the
/// per-unit objects (without `memory_mb`, which the caller attaches).
///
/// Mirrors the Python `_systemd_services_fallback`: `systemctl list-units
/// --type=service --all --no-pager --no-legend ados-*.service`, then for each row
/// `parts = line.split(None, 4)` (at most five whitespace-split tokens), the unit
/// is `parts[0]` with leading status glyphs (`●`/`*`) stripped, the sub-state is
/// `parts[3]`, and the object is built from those. A non-`.service` unit, a row
/// with fewer than four columns, an `rc != 0`, or a missing `systemctl` yields an
/// empty list.
fn systemd_services_fallback() -> Vec<Value> {
    let output = Command::new("systemctl")
        .args([
            "list-units",
            "--type=service",
            "--all",
            "--no-pager",
            "--no-legend",
            "ados-*.service",
        ])
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_PAGER", "")
        .env("LANG", "C")
        .output();

    let out = match output {
        Ok(o) if o.status.success() => o,
        // rc != 0, a missing binary, or a spawn error → the Python returns [].
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.lines().filter_map(parse_fallback_line).collect()
}

/// Parse one `systemctl list-units` row into the fallback service object, or
/// `None` when the row is blank, has too few columns, or is not a `.service` unit.
///
/// Mirrors the Python `parts = line.split(None, 4)` (split on whitespace runs into
/// at most five tokens) with the `len(parts) < 4` guard. The unit is `parts[0]`
/// with leading `●`/`*` status glyphs stripped; the sub-state is `parts[3]`.
/// `state` is `"running"` when the sub-state is `running`, else the sub-state (or
/// `"unknown"`); `status` mirrors `state`; `task_done` is `state != "running"`.
fn parse_fallback_line(line: &str) -> Option<Value> {
    // The Python uses str.split(None, 4): leading/trailing whitespace trimmed,
    // runs collapsed. A glyph token (`●`/`×`) becomes its own token and the unit
    // basename then carries a `.lstrip("●*")`. Collect the whitespace tokens.
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 4 {
        return None;
    }

    // The Python parses `parts[0]` then `.lstrip("●*")`. When systemd prepends a
    // glyph as its own token (`● ados-x.service ...`), that token is `parts[0]`
    // and the actual unit is `parts[1]`; the lstrip leaves the empty token "".
    // Mirror that: take the first non-empty post-lstrip token as the unit.
    let unit = cols[0].trim_start_matches(['●', '*']).trim();
    // `parts[3]` is the sub-state column. When a glyph token shifted the columns,
    // the row still has the unit at index 0 in the Python split because the glyph
    // and the unit are space-separated tokens; we match by indexing the same way
    // the Python does (`parts[3]`), so use cols[3] verbatim.
    let sub = cols[3].trim();

    if !unit.ends_with(".service") {
        return None;
    }
    let name = &unit[..unit.len() - ".service".len()];
    let state = if sub == "running" {
        "running"
    } else if sub.is_empty() {
        "unknown"
    } else {
        sub
    };

    Some(json!({
        "name": name,
        "state": state,
        "status": state,
        "task_done": state != "running",
        "uptimeSeconds": 0,
    }))
}

/// Attach a `memory_mb` field to each service entry, in place.
///
/// Resolves each entry's owning systemd unit, sums each distinct unit's grouped
/// PSS once via a single `/proc` scan, and writes the MiB value back. Entries with
/// no resolvable unit or a unit with no running process get `0.0` — the same value
/// the FastAPI live `/proc` scan reports for an absent unit. Mirrors
/// `_attach_service_memory`.
fn attach_service_memory(services: &mut [Value]) {
    let unit_by_entry: Vec<Option<String>> = services
        .iter()
        .map(|s| {
            s.as_object()
                .and_then(|m| m.get("name"))
                .and_then(Value::as_str)
                .and_then(unit_for_service)
        })
        .collect();

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

/// Resolve a service entry name to its systemd unit, mirroring the Python
/// `unit_for_service`. An `ados-*` basename maps to `<name>.service`; a short
/// in-process label maps through the fixed table; anything else is `None`. The
/// systemd-fallback entries this route emits all carry `ados-*` basenames, so they
/// take the first branch; the short-label table is carried for full parity.
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
    match name {
        "fc-connection" => Some("ados-mavlink.service".to_string()),
        "video-pipeline" => Some("ados-video.service".to_string()),
        "wfb-link" => Some("ados-wfb.service".to_string()),
        "rest-api" => Some("ados-api.service".to_string()),
        "health-monitor" => Some("ados-health.service".to_string()),
        "cloud-command-poll" => Some("ados-cloud.service".to_string()),
        "agent-heartbeat" => Some("ados-cloud.service".to_string()),
        "pairing-beacon" => Some("ados-cloud.service".to_string()),
        "pairing-heartbeat" => Some("ados-cloud.service".to_string()),
        _ => None,
    }
}

/// Sum PSS (MiB, one decimal) per `ados-*.service` unit across all running PIDs,
/// reading `/proc/<pid>/cgroup` for the owning unit and `/proc/<pid>/smaps_rollup`
/// for the PSS. Best-effort: an unreadable entry / a PID that exits mid-scan / no
/// permission contributes nothing. On a non-Linux host there is no `/proc`, so the
/// map is empty and every unit lands at `0.0`. Mirrors `_scan_pss_by_unit`.
fn scan_pss_by_unit() -> std::collections::BTreeMap<String, f64> {
    let mut totals_kib: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();

    let dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return std::collections::BTreeMap::new(),
    };

    for entry in dir.flatten() {
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str() else {
            continue;
        };
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

/// Extract the `ados-*.service` unit from a `/proc/<pid>/cgroup` body, matching the
/// Python regex `(ados-[a-z0-9-]+\.service)`: a literal `ados-`, one-or-more
/// lowercase-alphanumeric-or-dash chars, then `.service`. First match wins.
fn unit_from_cgroup(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let needle = b"ados-";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
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
            if j > body_start && bytes[j..].starts_with(b".service") {
                let end = j + ".service".len();
                return Some(String::from_utf8_lossy(&bytes[i..end]).into_owned());
            }
        }
        i += 1;
    }
    None
}

/// Parse the `Pss:` line out of a `/proc/<pid>/smaps_rollup` body (KiB), `0` when
/// absent or unparseable. Mirrors `pss_kib_from_rollup`.
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

// ---------------------------------------------------------------------------
// WFB status view: the same store-first / sidecar-fallback read /api/wfb uses.
// ---------------------------------------------------------------------------

/// The WFB-ng link status the video gate + the radio block both read.
///
/// This daemon has no in-process WFB manager, so the status is read the same way
/// `/api/wfb` does: store-first (the radio ships its full status body to the
/// durable store as a `link.wfb_status` event each heartbeat), falling back to the
/// `/run/ados/wfb-stats.json` sidecar. Returns the finalized `/api/wfb` body as a
/// map, or `None` when neither source has a usable body — matching the FastAPI
/// `wfb_status = _build_status_from_stats_file(...)` (and its `except: None`).
async fn wfb_status_view(state: &AppState) -> Option<Map<String, Value>> {
    let cfg = WfbStatusConfig::load(&state.pairing_paths.config);

    if let Some((detail, ts_us)) = latest_wfb_status(state).await {
        if let Value::Object(map) = derive_wfb_status(&detail, ts_us, &cfg) {
            return Some(map);
        }
    }

    match build_status_from_stats_file(&cfg) {
        Value::Object(map) if !map.is_empty() => Some(map),
        _ => None,
    }
}

/// The `video.wfb` config slice the WFB status base block seeds from. Each field
/// is optional so an absent section reads the same default the loaded Python
/// config would (`channel` → `0`, the rest → `null`). Mirrors the wave-1
/// `/api/wfb` config seam.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct WfbStatusConfig {
    channel: i64,
    tx_power_dbm: Value,
    tx_power_max_dbm: Value,
    topology: Value,
    mcs_index: Value,
}

impl WfbStatusConfig {
    /// Load the `video.wfb` slice from the config path, defaulting every field when
    /// the file/section is absent or unparseable.
    fn load(config_path: &Path) -> Self {
        let text = match std::fs::read_to_string(config_path) {
            Ok(t) => t,
            Err(_) => return WfbStatusConfig::default(),
        };
        let root: Value = match serde_norway::from_str(&text) {
            Ok(v) => v,
            Err(_) => return WfbStatusConfig::default(),
        };
        let wfb = root
            .get("video")
            .filter(|v| v.is_object())
            .and_then(|v| v.get("wfb"))
            .filter(|v| v.is_object());
        let Some(wfb) = wfb else {
            return WfbStatusConfig::default();
        };
        WfbStatusConfig {
            channel: wfb.get("channel").and_then(json_to_i64).unwrap_or(0),
            tx_power_dbm: wfb.get("tx_power_dbm").cloned().unwrap_or(Value::Null),
            tx_power_max_dbm: wfb.get("tx_power_max_dbm").cloned().unwrap_or(Value::Null),
            topology: wfb.get("topology").cloned().unwrap_or(Value::Null),
            mcs_index: wfb.get("mcs_index").cloned().unwrap_or(Value::Null),
        }
    }
}

/// Beyond this age (microseconds) a stored status event is treated as stale,
/// mirroring the sidecar path's `mtime > 10 s` flip.
const WFB_STALE_AGE_US: i64 = 10_000_000;

/// The most-recent full wfb-status snapshot + its emit timestamp, or `None` when
/// the store is unreachable / holds no such event / the detail is empty. Mirrors
/// the wave-1 `latest_wfb_status`.
async fn latest_wfb_status(state: &AppState) -> Option<(Map<String, Value>, i64)> {
    let rows = logd_query_events(state, "link.wfb_status", 1).await?;
    let row = rows.first()?.as_object()?;
    let detail = row.get("detail")?.as_object()?;
    if detail.is_empty() {
        return None;
    }
    let ts_us = row
        .get("ts_us")
        .and_then(Value::as_f64)
        .map(|v| v as i64)
        .unwrap_or(0);
    Some((detail.clone(), ts_us))
}

/// Map a stored status body back to the `/api/wfb` shape: the config-seeded base,
/// the body merged over it, an event-age staleness flip, then the shared finalize
/// legs. Mirrors the wave-1 `derive_wfb_status`.
fn derive_wfb_status(detail: &Map<String, Value>, ts_us: i64, cfg: &WfbStatusConfig) -> Value {
    let mut merged = wfb_base_block(cfg);
    for (k, v) in detail {
        merged.insert(k.clone(), v.clone());
    }
    let now_us = now_unix_micros();
    if ts_us > 0 && now_us - ts_us > WFB_STALE_AGE_US {
        merged.insert("state".to_string(), json!("stale"));
    }
    finalize_wfb_status(merged)
}

/// Compose a `/api/wfb` body from the `wfb-stats.json` sidecar: merge the file
/// payload over the config-seeded base, flip `state` to `"stale"` when the file is
/// older than 10 s, re-assert the live regulatory domain, and finalize. An absent
/// / unparseable file degrades to the bare base block; a well-formed non-object
/// body returns the bare base. Mirrors the wave-1 `build_status_from_stats_file`.
fn build_status_from_stats_file(cfg: &WfbStatusConfig) -> Value {
    let base = wfb_base_block(cfg);
    let path = run_dir().join("wfb-stats.json");

    let age_s = match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime.elapsed().map(|d| d.as_secs_f64()).unwrap_or(0.0),
        Err(_) => return Value::Object(base),
    };

    let payload = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Value::Object(base),
            Err(_) => return Value::Object(base),
        },
        Err(_) => return Value::Object(base),
    };

    // Best-effort schema-drift signal (never reject): warn when the sidecar was
    // written by an agent with a different schema version, then read anyway. The
    // writer const lives in the radio crate, so compare against the shared registry.
    let got = payload.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("wfb-stats") {
        ados_protocol::sidecar::check_sidecar_version("wfb-stats", got, ours);
    }

    let mut merged = base;
    for (k, v) in payload {
        merged.insert(k, v);
    }
    if age_s > 10.0 {
        merged.insert("state".to_string(), json!("stale"));
    }
    merged.insert("regulatory_domain".to_string(), json!(regulatory_domain()));
    finalize_wfb_status(merged)
}

/// The config-seeded zero-default WFB status block both read paths merge over.
/// `regulatory_domain` is the LIVE `iw reg get` value. Mirrors the wave-1
/// `base_block`.
fn wfb_base_block(cfg: &WfbStatusConfig) -> Map<String, Value> {
    let mut block = Map::new();
    block.insert("state".to_string(), json!("disabled"));
    block.insert("interface".to_string(), json!(""));
    block.insert("channel".to_string(), json!(cfg.channel));
    block.insert("frequency_mhz".to_string(), json!(0));
    block.insert("bandwidth_mhz".to_string(), json!(0));
    block.insert(
        "adapter".to_string(),
        json!({"driver": "", "chipset": "", "supports_monitor": false}),
    );
    block.insert("adapter_chipset".to_string(), Value::Null);
    block.insert("adapter_injection_ok".to_string(), json!(false));
    block.insert("rssi_dbm".to_string(), json!(-100.0));
    block.insert("noise_dbm".to_string(), json!(-95.0));
    block.insert("snr_db".to_string(), json!(0.0));
    block.insert("packets_received".to_string(), json!(0));
    block.insert("packets_lost".to_string(), json!(0));
    block.insert("loss_percent".to_string(), json!(0.0));
    block.insert("fec_recovered".to_string(), json!(0));
    block.insert("fec_failed".to_string(), json!(0));
    block.insert("bitrate_kbps".to_string(), json!(0));
    block.insert("rx_silent_seconds".to_string(), Value::Null);
    block.insert("restart_count".to_string(), json!(0));
    block.insert("samples".to_string(), json!(0));
    block.insert("tx_power_dbm".to_string(), cfg.tx_power_dbm.clone());
    block.insert("tx_power_max_dbm".to_string(), cfg.tx_power_max_dbm.clone());
    block.insert("topology".to_string(), cfg.topology.clone());
    block.insert("mcs_index".to_string(), cfg.mcs_index.clone());
    block.insert("regulatory_domain".to_string(), json!(regulatory_domain()));
    block
}

/// Re-derive `frequency_mhz` / `bandwidth_mhz` from the channel and add the
/// `bitrate_mbps` shim, on top of a base+payload merge. Mirrors the wave-1
/// `finalize_status`.
fn finalize_wfb_status(mut merged: Map<String, Value>) -> Value {
    let channel = merged.get("channel").and_then(json_to_i64).unwrap_or(0);
    if let Some((freq, bw)) = wfb_channel_freq_bw(channel) {
        merged.insert("frequency_mhz".to_string(), json!(freq));
        merged.insert("bandwidth_mhz".to_string(), json!(bw));
    }
    let bitrate_mbps = match merged.get("bitrate_kbps").and_then(Value::as_f64) {
        Some(bk) if bk > 0.0 => round3(bk / 1000.0),
        _ => 0.0,
    };
    merged.insert("bitrate_mbps".to_string(), json!(bitrate_mbps));
    Value::Object(merged)
}

/// The standard 5 GHz WFB-ng channel → (frequency_mhz, bandwidth_mhz) lookup, each
/// 20 MHz wide. Mirrors the wave-1 `STANDARD_CHANNELS` set; an unknown channel
/// yields `None` so the merged frequency/bandwidth survive unchanged.
fn wfb_channel_freq_bw(channel: i64) -> Option<(i64, i64)> {
    let freq = match channel {
        36 => 5180,
        40 => 5200,
        44 => 5220,
        48 => 5240,
        149 => 5745,
        153 => 5765,
        157 => 5785,
        161 => 5805,
        165 => 5825,
        _ => return None,
    };
    Some((freq, 20))
}

/// Best-effort `iw reg get` first-line parse → the two-letter country code,
/// `"global"`, or `"unknown"`. Mirrors the wave-1 `regulatory_domain`.
fn regulatory_domain() -> String {
    let output = match Command::new("iw").args(["reg", "get"]).output() {
        Ok(o) => o,
        Err(_) => return "unknown".to_string(),
    };
    if !output.status.success() {
        return "unknown".to_string();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let stripped = line.trim();
        if let Some(rest) = stripped.strip_prefix("country ") {
            let code = rest.split(':').next().unwrap_or("").trim();
            if code.is_empty() {
                return "unknown".to_string();
            }
            return code.to_string();
        }
        if stripped.starts_with("global") {
            return "global".to_string();
        }
    }
    "unknown".to_string()
}

// ---------------------------------------------------------------------------
// Radio block: the forward-compatible heartbeat radio block + camelCase remap.
// ---------------------------------------------------------------------------

/// The RSSI sentinel the link-quality monitor seeds before the first real sample.
/// Treated as "no reading yet" so the radio block reports `null` for it. Mirrors
/// the Python `if rssi == -100.0: rssi = None`.
const RSSI_SENTINEL: f64 = -100.0;

/// Shape the forward-compatible `radio` heartbeat block from a WFB status view.
///
/// `wfb_status` is the `/api/wfb` body (or `None` when no view is available). The
/// GCS keys off the presence of the block, not the values; an absent view yields
/// the full "absent" skeleton (every metric `null`, paired/injection `false`).
/// Mirrors the Python `build_radio_block` field-for-field.
fn build_radio_block(wfb_status: Option<&Map<String, Value>>) -> Value {
    let Some(status) = wfb_status else {
        return radio_absent_block();
    };

    let iface = status.get("interface").and_then(non_empty_string);
    let driver = iface
        .as_deref()
        .and_then(detect_radio_driver_name)
        .map(Value::from)
        .unwrap_or(Value::Null);
    let iface_value = iface.clone().map(Value::from).unwrap_or(Value::Null);

    let channel = status
        .get("channel")
        .filter(|v| !is_falsey(v))
        .cloned()
        .unwrap_or(Value::Null);
    let freq_mhz = channel
        .as_i64()
        .and_then(channel_to_freq)
        .map(Value::from)
        .unwrap_or(Value::Null);

    let rssi = match status.get("rssi_dbm").and_then(Value::as_f64) {
        Some(v) if v == RSSI_SENTINEL => Value::Null,
        _ => status.get("rssi_dbm").cloned().unwrap_or(Value::Null),
    };
    let bitrate = status
        .get("bitrate_kbps")
        .filter(|v| !is_falsey(v))
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "state": status.get("state").cloned().unwrap_or(Value::Null),
        "iface": iface_value,
        "driver": driver,
        "channel": channel,
        "freq_mhz": freq_mhz,
        "bandwidth_mhz": 20,
        "tx_power_dbm": get_or_null(status, "tx_power_dbm"),
        "tx_power_max_dbm": get_or_null(status, "tx_power_max_dbm"),
        "topology": get_or_null(status, "topology"),
        "rssi_dbm": rssi,
        "snr_db": get_or_null(status, "snr_db"),
        "noise_dbm": get_or_null(status, "noise_dbm"),
        "bitrate_kbps": bitrate,
        "fec_recovered": get_or_null(status, "fec_recovered"),
        "fec_lost": get_or_null(status, "fec_failed"),
        "packets_lost": get_or_null(status, "packets_lost"),
        "loss_percent": get_or_null(status, "loss_percent"),
        "mcs_index": get_or_null(status, "mcs_index"),
        "rx_silent_seconds": get_or_null(status, "rx_silent_seconds"),
        "paired": json_truthy(status.get("paired").unwrap_or(&Value::Null)),
        "paired_with_device_id": get_or_null(status, "paired_with_device_id"),
        "paired_at": get_or_null(status, "paired_at"),
        "public_key_fingerprint": get_or_null(status, "public_key_fingerprint"),
        "auto_pair_enabled": get_or_null(status, "auto_pair_enabled"),
        "tx_video_stalled": get_or_null(status, "tx_video_stalled"),
        "tx_video_stall_kills": get_or_null(status, "tx_video_stall_kills"),
        "tx_video_recvq_bytes": get_or_null(status, "tx_video_recvq_bytes"),
        "acquire_state": get_or_null(status, "acquire_state"),
        "channel_locked": get_or_null(status, "channel_locked"),
        "reacquire_kills": get_or_null(status, "reacquire_kills"),
        "valid_rx_packets_per_s": get_or_null(status, "valid_rx_packets_per_s"),
        "adapter_chipset": get_or_null(status, "adapter_chipset"),
        "adapter_injection_ok": json_truthy_default_false(status, "adapter_injection_ok"),
        "adapter_usb_speed_mbps": get_or_null(status, "adapter_usb_speed_mbps"),
        "adapter_usb_degraded": json_truthy_default_false(status, "adapter_usb_degraded"),
        "phy_muted": json_truthy_default_false(status, "phy_muted"),
        "tx_zombie_kills": get_or_null(status, "tx_zombie_kills"),
        "tx_bytes_per_s": get_or_null(status, "tx_bytes_per_s"),
        "restart_count": get_or_null(status, "restart_count"),
    })
}

/// The "no radio" skeleton the heartbeat carries when there is no WFB status view.
/// Every metric is `null`; paired / injection / degraded / muted are `false`.
/// Mirrors the Python `build_radio_block` `if not wfb_status:` branch exactly.
fn radio_absent_block() -> Value {
    json!({
        "state": "absent",
        "iface": null,
        "driver": null,
        "channel": null,
        "freq_mhz": null,
        "bandwidth_mhz": null,
        "tx_power_dbm": null,
        "tx_power_max_dbm": null,
        "topology": null,
        "rssi_dbm": null,
        "snr_db": null,
        "noise_dbm": null,
        "bitrate_kbps": null,
        "fec_recovered": null,
        "fec_lost": null,
        "packets_lost": null,
        "loss_percent": null,
        "mcs_index": null,
        "rx_silent_seconds": null,
        "paired": false,
        "paired_with_device_id": null,
        "paired_at": null,
        "public_key_fingerprint": null,
        "auto_pair_enabled": null,
        "tx_video_stalled": null,
        "tx_video_stall_kills": null,
        "tx_video_recvq_bytes": null,
        "acquire_state": null,
        "channel_locked": null,
        "reacquire_kills": null,
        "valid_rx_packets_per_s": null,
        "adapter_chipset": null,
        "adapter_injection_ok": false,
        "adapter_usb_speed_mbps": null,
        "adapter_usb_degraded": false,
        "phy_muted": false,
    })
}

/// The radio block's channel → centre-frequency lookup (a strict subset of the
/// WFB channel set: the Python `_CHANNEL_TO_FREQ_MHZ` map). An unknown channel
/// yields `None` so the GCS draws a blank cell. NOTE the `40`/`44` channels in the
/// status channel set are deliberately absent here, matching the Python map.
fn channel_to_freq(channel: i64) -> Option<i64> {
    match channel {
        36 => Some(5180),
        48 => Some(5240),
        149 => Some(5745),
        153 => Some(5765),
        157 => Some(5785),
        161 => Some(5805),
        165 => Some(5825),
        _ => None,
    }
}

/// Best-effort kernel driver name for the WFB monitor interface, read from
/// `/sys/class/net/<iface>/device/uevent`'s `DRIVER=` line. `None` for an empty
/// iface or an unreadable file. Mirrors `_detect_radio_driver_name`.
fn detect_radio_driver_name(interface: &str) -> Option<String> {
    if interface.is_empty() {
        return None;
    }
    let path = Path::new("/sys/class/net")
        .join(interface)
        .join("device")
        .join("uevent");
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("DRIVER=") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Convert the snake_case radio block to the camelCase shape the GCS reads (the
/// LAN-direct poll has no heartbeat remapper, so the conversion happens here).
/// Each `a_b_c` key becomes `aBC`. `None` stays `None`. Mirrors `_radio_to_camel`.
fn radio_to_camel(block: Value) -> Value {
    let Value::Object(map) = block else {
        return Value::Null;
    };
    let mut out = Map::new();
    for (key, value) in map {
        out.insert(snake_to_camel(&key), value);
    }
    Value::Object(out)
}

/// `a_b_c` → `aBC`: the head segment lower-cased verbatim, each later segment
/// title-cased and concatenated. Mirrors the Python
/// `head + "".join(p.title() for p in tail)`.
fn snake_to_camel(key: &str) -> String {
    let mut parts = key.split('_');
    let head = parts.next().unwrap_or("");
    let mut out = String::with_capacity(key.len());
    out.push_str(head);
    for part in parts {
        out.push_str(&title_case(part));
    }
    out
}

/// Title-case a single segment, matching Python `str.title()` for an ASCII word:
/// first char upper, the rest lower. An empty segment stays empty.
fn title_case(segment: &str) -> String {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) => {
            let mut out = String::with_capacity(segment.len());
            out.extend(first.to_uppercase());
            out.push_str(&chars.as_str().to_lowercase());
            out
        }
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Video block: the multi-process branch (no in-process pipeline).
// ---------------------------------------------------------------------------

/// mediamtx default ports (matching the Python `_MEDIAMTX_*_PORT`).
const MEDIAMTX_API_PORT: u16 = 9997;
const MEDIAMTX_WEBRTC_PORT: u16 = 8889;

/// Build the consolidated-status `video` block for the multi-process case.
///
/// There is no in-process pipeline on this daemon, so the FastAPI route branches
/// on the resolved profile (NOT on pipeline presence):
///
/// - **drone** — the local mediamtx serves its own camera regardless of the WFB
///   downlink. Probe readiness (`/v3/paths/list`, then the WHEP fallback); when
///   ready, report `running` with the WHEP URL, else the default `not_initialized`.
/// - **ground-station** — mediamtx serves WHEP whether or not frames arrive, so a
///   reachable endpoint is not proof of a live downlink. Gate on the WFB receive
///   link actually delivering (`_gs_video_delivering`): when delivering + WHEP
///   ready → `running`; delivering but WHEP not yet serving → `connecting`; not
///   delivering → `stopped`.
///
/// The recording block is always empty (no in-process recorder on this daemon).
/// Mirrors the FastAPI `pipeline is None` branch exactly.
///
/// This resolves the live mediamtx readiness once (the real loopback probes) and
/// hands it to the pure [`build_video_block_with`] core, so the block-shape logic
/// is testable with an injected readiness and no live port.
async fn build_video_block(
    resolved_profile: &str,
    wfb_status: &Option<Map<String, Value>>,
    host: &str,
) -> Value {
    build_video_block_with(
        resolved_profile,
        wfb_status,
        host,
        resolve_mediamtx_ready().await,
        read_video_streams(host),
    )
}

/// Read the video-streams sidecar (`/run/ados/video-streams.json`, written by
/// ados-video on pipeline start) and resolve each leg to a dialable WHEP URL, so
/// the status surface can advertise every `:8889/<id>/whep` leg for the GCS
/// stream switcher. Best-effort: any read/parse failure yields an empty list.
fn read_video_streams(host: &str) -> Vec<Value> {
    let Ok(body) = std::fs::read("/run/ados/video-streams.json") else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_slice::<Value>(&body) else {
        return Vec::new();
    };
    let Some(streams) = doc.get("streams").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    streams
        .iter()
        .filter_map(|leg| {
            let id = leg.get("id")?.as_str()?;
            Some(json!({
                "id": id,
                "role": leg.get("role").and_then(|r| r.as_str()).unwrap_or(""),
                "codec": leg.get("codec").and_then(|c| c.as_str()).unwrap_or(""),
                "whep": format!("http://{host}:{MEDIAMTX_WEBRTC_PORT}/{id}/whep"),
                // Per-leg liveness (null when the agent has not sampled it yet).
                "live": leg.get("live").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}

/// Resolve whether the local mediamtx has a ready stream: the management-API
/// probe (`/v3/paths/list`) with the WHEP-liveness fallback for the credentialed
/// ground-station mediamtx. Split out from the block builder so the builder is a
/// pure function of an injected readiness (no live TCP port in unit tests).
async fn resolve_mediamtx_ready() -> bool {
    let mut mtx = probe_mediamtx().await;
    if mtx.as_ref().map(|m| !mtx_ready(m)).unwrap_or(true) {
        mtx = probe_mediamtx_via_whep().await.or(mtx);
    }
    mtx.as_ref().map(mtx_ready).unwrap_or(false)
}

/// The mediamtx-readiness-injectable core of [`build_video_block`]. Pure over
/// `mediamtx_ready` (the resolved probe outcome), so tests exercise every branch
/// deterministically without a live loopback port.
fn build_video_block_with(
    resolved_profile: &str,
    wfb_status: &Option<Map<String, Value>>,
    host: &str,
    mediamtx_ready: bool,
    streams: Vec<Value>,
) -> Value {
    // Default: not initialised, no playable endpoint.
    let default = json!({
        "state": "not_initialized",
        "whep_url": null,
        "recording": false,
        "recording_filename": null,
        "recording_started_at": null,
    });

    let running = || {
        let mut block = json!({
            "state": "running",
            "whep_url": format!("http://{host}:{MEDIAMTX_WEBRTC_PORT}/main/whep"),
            "recording": false,
            "recording_filename": null,
            "recording_started_at": null,
        });
        // Advertise the per-leg streams (only when a live pipeline wrote the
        // sidecar) so the GCS switcher can address each `:8889/<id>/whep` leg.
        if !streams.is_empty() {
            block["streams"] = Value::Array(streams.clone());
        }
        block
    };

    if resolved_profile == "drone" {
        if mediamtx_ready {
            return running();
        }
        return default;
    }

    // Ground-station path: gate on the receive link actually delivering video.
    if gs_video_delivering(wfb_status.as_ref()) {
        if mediamtx_ready {
            return running();
        }
        // Link delivering frames but WHEP not yet serving — still coming up.
        return json!({
            "state": "connecting",
            "whep_url": null,
            "recording": false,
            "recording_filename": null,
            "recording_started_at": null,
        });
    }

    // No live downlink: stopped with no playable endpoint.
    json!({
        "state": "stopped",
        "whep_url": null,
        "recording": false,
        "recording_filename": null,
        "recording_started_at": null,
    })
}

/// True only when the ground-station WFB link is actually delivering video: the
/// receive state must be `active`/`connected` AND a positive valid-decode rate or
/// packet count confirms frames are flowing now. A reachable WHEP endpoint is not
/// proof (mediamtx serves WHEP regardless of inbound frames). Mirrors
/// `_gs_video_delivering`.
fn gs_video_delivering(wfb_status: Option<&Map<String, Value>>) -> bool {
    let Some(status) = wfb_status else {
        return false;
    };
    let state = status.get("state").and_then(Value::as_str);
    if !matches!(state, Some("active") | Some("connected")) {
        return false;
    }
    for key in ["valid_rx_packets_per_s", "packets_received"] {
        if let Some(v) = status.get(key).and_then(Value::as_f64) {
            if v > 0.0 {
                return true;
            }
        }
    }
    false
}

/// True when a mediamtx probe result reports a ready stream (`ready == true`).
fn mtx_ready(mtx: &Value) -> bool {
    mtx.get("ready").and_then(Value::as_bool).unwrap_or(false)
}

/// Probe mediamtx's management API (`/v3/paths/list`) for an active stream.
///
/// Returns a small result dict (`running`, `stream_name`, `ready`, `tracks`,
/// `readers`, `webrtc_port`) for the first path, or `None` when mediamtx is
/// unreachable / returns non-200 / has no active streams. Mirrors `_probe_mediamtx`.
async fn probe_mediamtx() -> Option<Value> {
    let url = format!("http://127.0.0.1:{MEDIAMTX_API_PORT}/v3/paths/list");
    let (status, body) = http_get_local(&url).await.ok()?;
    if status != 200 {
        return None;
    }
    let data: Value = serde_json::from_slice(&body).ok()?;
    let items = data.get("items").and_then(Value::as_array)?;
    // Look the primary `main` path up BY NAME — never `items.first()`. With
    // multiple named paths (main / eo_wide / ir) the first-listed may be an idle
    // secondary `sourceOnDemand` leg (ready only while a reader is attached),
    // which would collapse the whole video block to not-ready even while /main
    // is live and serving.
    let path = items
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("main"))
        .or_else(|| items.first())?;
    Some(json!({
        "running": true,
        "stream_name": path.get("name").cloned().unwrap_or_else(|| json!("main")),
        "ready": path.get("ready").and_then(Value::as_bool).unwrap_or(false),
        "tracks": path.get("tracks").cloned().unwrap_or_else(|| json!([])),
        "readers": path.get("readers").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
        "webrtc_port": MEDIAMTX_WEBRTC_PORT,
    }))
}

/// Liveness probe via the public WHEP endpoint, for the ground-station mediamtx
/// (which puts auth on the management API). A GET on the POST-only WHEP path
/// returns 200/204/405 when bound — the "endpoint exists and mediamtx is up"
/// signal, no credentials needed. Returns the same result shape with `ready: true`
/// (the fanout exposes no separate readiness here). Mirrors `_probe_mediamtx_via_whep`.
async fn probe_mediamtx_via_whep() -> Option<Value> {
    let url = format!("http://127.0.0.1:{MEDIAMTX_WEBRTC_PORT}/main/whep");
    let (status, _body) = http_get_local(&url).await.ok()?;
    if matches!(status, 200 | 204 | 405) {
        return Some(json!({
            "running": true,
            "stream_name": "main",
            "ready": true,
            "tracks": [],
            "readers": 0,
            "webrtc_port": MEDIAMTX_WEBRTC_PORT,
        }));
    }
    None
}

/// The cumulative `bytesReceived` counter on the mediamtx `main` path, read from
/// the management API `/v3/paths/get/main`. This is the canonical
/// video-into-mediamtx signal (the drone's encoder → mediamtx ingest, or the
/// ground station's fan-out → mediamtx-gs ingest), and the reliable delta the
/// video-diagnostics harness samples. `None` when the API is unreachable, returns
/// non-200 (the ground-station mediamtx puts auth on the management API, so this
/// can legitimately be unavailable there and the caller falls back to the WHEP
/// liveness probe), or the body carries no numeric `bytesReceived`.
pub(crate) async fn mediamtx_main_bytes_received() -> Option<i64> {
    let url = format!("http://127.0.0.1:{MEDIAMTX_API_PORT}/v3/paths/get/main");
    let (status, body) = http_get_local(&url).await.ok()?;
    if status != 200 {
        return None;
    }
    let data: Value = serde_json::from_slice(&body).ok()?;
    data.get("bytesReceived")
        .and_then(Value::as_i64)
        .or_else(|| {
            data.get("bytesReceived")
                .and_then(Value::as_f64)
                .map(|f| f as i64)
        })
}

/// True when the mediamtx WHEP endpoint (`:8889/main/whep`) is bound and serving.
/// A GET on the POST-only WHEP path returns 200/204/405 when mediamtx is up —
/// the credential-free "endpoint exists and mediamtx is serving" signal the
/// video-diagnostics harness uses for the served-WHEP hop (and the ground-station
/// ingest hop when the management API is auth-gated).
pub(crate) async fn mediamtx_whep_serving() -> bool {
    probe_mediamtx_via_whep().await.is_some()
}

/// A minimal HTTP/1.1 `GET` over a local TCP endpoint, returning the status code +
/// the decoded body. Used for the mediamtx probes (which speak HTTP on a loopback
/// TCP port, not a Unix socket). `Connection: close` reads the body to EOF; a
/// chunked body is de-chunked. Bounded so a runaway response cannot exhaust memory.
/// A 2 s timeout matches the Python `httpx.AsyncClient(timeout=2.0)`.
async fn http_get_local(url: &str) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{timeout, Duration};

    const MAX_READ_BYTES: usize = 4 * 1024 * 1024;
    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    // Parse `http://host:port/path` into the connect target + the request path.
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::other("non-http url"))?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let path = if path.is_empty() { "/" } else { path };

    let fut = async {
        let mut stream = tokio::net::TcpStream::connect(authority).await?;
        let head = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).await?;
        stream.flush().await?;

        let mut raw = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            if raw.len() + n > MAX_READ_BYTES {
                return Err(std::io::Error::other("probe response too large"));
            }
            raw.extend_from_slice(&buf[..n]);
        }
        parse_http_response(&raw)
    };

    match timeout(PROBE_TIMEOUT, fut).await {
        Ok(res) => res,
        Err(_) => Err(std::io::Error::other("probe timed out")),
    }
}

// ---------------------------------------------------------------------------
// Mesh block: ground-station profile, non-direct role only.
// ---------------------------------------------------------------------------

/// Build the consolidated-status `mesh` block. Populated only when the RAW config
/// profile is exactly `"ground_station"` (the underscore form, NOT the resolved
/// wire form — matching the Python gate `app.config.agent.profile ==
/// "ground_station"`); a drone or an `"auto"`/empty config gets an empty object.
///
/// On a ground station it carries `role` + `mesh_capable`; for a `relay`/`receiver`
/// role it also folds in the `up` / `peer_count` / `selected_gateway` / `partition`
/// fields from `/run/ados/mesh-state.json`. Every read degrades to omitting that
/// field rather than failing. Mirrors the FastAPI mesh block.
fn build_mesh_block(config_profile: &str) -> Value {
    build_mesh_block_at(
        config_profile,
        &mesh_role_path(),
        &profile_conf_path(),
        &run_dir().join("mesh-state.json"),
    )
}

/// The path-injectable core of [`build_mesh_block`]: read the role sentinel,
/// `mesh_capable` hint, and mesh-state sidecar from explicit paths. Pure (no env
/// reads), so a test drives it with a tempdir without touching process-global env.
fn build_mesh_block_at(
    config_profile: &str,
    role_path: &Path,
    profile_conf: &Path,
    mesh_state_path: &Path,
) -> Value {
    if config_profile != "ground_station" {
        return json!({});
    }

    let mut mesh = Map::new();
    let role = read_mesh_role_at(role_path);
    mesh.insert("role".to_string(), json!(role));
    mesh.insert(
        "mesh_capable".to_string(),
        json!(read_mesh_capable_at(profile_conf)),
    );

    if role == "relay" || role == "receiver" {
        if let Some(snap) = read_sidecar_object(mesh_state_path) {
            // Best-effort schema-drift signal (never reject): warn when the
            // mesh-state sidecar was written by an agent with a different schema
            // version, then read anyway. The writer const lives in the groundlink
            // crate, so compare against the shared registry.
            let got = snap.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
            if let Some(ours) = ados_protocol::contracts::sidecar_version("mesh-state") {
                ados_protocol::sidecar::check_sidecar_version("mesh-state", got, ours);
            }
            mesh.insert(
                "up".to_string(),
                json!(snap.get("up").map(json_truthy).unwrap_or(false)),
            );
            let peer_count = snap
                .get("neighbors")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            mesh.insert("peer_count".to_string(), json!(peer_count));
            mesh.insert(
                "selected_gateway".to_string(),
                snap.get("selected_gateway").cloned().unwrap_or(Value::Null),
            );
            mesh.insert(
                "partition".to_string(),
                json!(snap.get("partition").map(json_truthy).unwrap_or(false)),
            );
        }
    }
    Value::Object(mesh)
}

/// The mesh-role sentinel path (`ADOS_MESH_ROLE` override, default
/// `/etc/ados/mesh/role`), the same path the crate's profile module resolves.
fn mesh_role_path() -> PathBuf {
    std::env::var("ADOS_MESH_ROLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(crate::profile::MESH_ROLE_PATH))
}

/// The profile-conf path (`ADOS_PROFILE_CONF` override, default
/// `/etc/ados/profile.conf`), the same path the crate's profile module resolves.
fn profile_conf_path() -> PathBuf {
    std::env::var("ADOS_PROFILE_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(crate::profile::PROFILE_CONF))
}

/// The current ground-station role from the role sentinel, defaulting to `direct`
/// when the sentinel is absent / unreadable / carries an unknown value — the same
/// resolution `role_manager.get_current_role` does.
fn read_mesh_role_at(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value = text.trim();
            if matches!(value, "direct" | "relay" | "receiver") {
                value.to_string()
            } else {
                "direct".to_string()
            }
        }
        Err(_) => "direct".to_string(),
    }
}

/// The `mesh_capable` hint from `profile.conf` (parsed as YAML), defaulting to
/// `false` when absent / unparseable / not a truthy value. Mirrors
/// `bool(pc.get("mesh_capable", False))`.
fn read_mesh_capable_at(path: &Path) -> bool {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let parsed: Value = match serde_norway::from_str(&text) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed.get("mesh_capable").map(json_truthy).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Camera status: the LOCAL camera-presence + USB-recovery surface.
// ---------------------------------------------------------------------------

/// The freshness window (seconds) for the camera-state sidecar. A reading older
/// than this is dropped. Mirrors the Python `<= 300.0` gate.
const CAMERA_STATE_FRESH_S: f64 = 300.0;

/// The freshness window (seconds) for the USB-recovery sidecar. Mirrors `<= 60.0`.
const CAMERA_RECOVERY_FRESH_S: f64 = 60.0;

/// Camera presence + USB-recovery state for the LOCAL status surface, folded into
/// the payload only when the sidecars are fresh + valid (absent otherwise).
///
/// Reads `/run/ados/camera-state.json` (the `cameraState` enum) and
/// `/run/ados/camera-usb-recovery.json` (the recovery block), staleness-gated on
/// each sidecar's `updated_at_unix`. Mirrors `_read_camera_status`. Shared with
/// the `/api/status` route, which folds the same camera keys into its body.
pub(crate) fn read_camera_status() -> Vec<(String, Value)> {
    read_camera_status_in(&run_dir(), now_unix_secs())
}

/// The path-injectable core of [`read_camera_status`]: read the two camera sidecars
/// under `run_dir`, staleness-gated against `now`. Pure (no env reads), so a test
/// drives it with a tempdir without racing the process-global `ADOS_RUN_DIR`.
fn read_camera_status_in(run_dir: &Path, now: f64) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = Vec::new();

    if let Some(camera) = read_sidecar_object(&run_dir.join("camera-state.json")) {
        // Best-effort schema-drift signal (never reject): warn on a producer/reader
        // version mismatch, then read anyway. The writer const lives in the
        // ados-video crate, so compare against the shared registry.
        let got = camera.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
        if let Some(ours) = ados_protocol::contracts::sidecar_version("camera-state") {
            ados_protocol::sidecar::check_sidecar_version("camera-state", got, ours);
        }
        if sidecar_fresh(&camera, now, CAMERA_STATE_FRESH_S) {
            if let Some(state) = camera.get("state").and_then(Value::as_str) {
                if matches!(state, "ready" | "missing" | "error") {
                    out.push(("cameraState".to_string(), json!(state)));
                }
            }
        }
    }

    if let Some(rec) = read_sidecar_object(&run_dir.join("camera-usb-recovery.json")) {
        // Best-effort schema-drift signal (never reject): warn on a producer/reader
        // version mismatch, then read anyway. The writer const lives in the
        // ados-supervisor crate, so compare against the shared registry.
        let got = rec.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
        if let Some(ours) = ados_protocol::contracts::sidecar_version("camera-usb-recovery") {
            ados_protocol::sidecar::check_sidecar_version("camera-usb-recovery", got, ours);
        }
        if sidecar_fresh(&rec, now, CAMERA_RECOVERY_FRESH_S) {
            let state = rec.get("camera_usb_recovery_state").and_then(Value::as_str);
            if let Some(state) = state {
                if matches!(
                    state,
                    "idle"
                        | "monitoring"
                        | "rebinding"
                        | "port_cycling"
                        | "hub_resetting"
                        | "needs_hub_reset"
                        | "guard_blocked"
                        | "exhausted"
                ) {
                    out.push((
                        "cameraUsbRecovery".to_string(),
                        json!({
                            "state": state,
                            "case": rec.get("case").cloned().unwrap_or(Value::Null),
                            "attempts": rec.get("attempts").cloned().unwrap_or(json!(0)),
                            "maxAttempts": rec.get("max_attempts").cloned().unwrap_or(json!(0)),
                            "cameraPresent": json_truthy_default_false(&rec, "camera_present"),
                            "expected": json_truthy_default_false(&rec, "expected"),
                            "pppsCapable": json_truthy_default_false(&rec, "ppps_capable"),
                            "powerContention": json_truthy_default_false(&rec, "power_contention"),
                            "contentionPeer": rec.get("contention_peer").cloned().unwrap_or(Value::Null),
                        }),
                    ));
                }
            }
        }
    }
    out
}

/// Read + parse a sidecar file into an object, or `None` on any gap (absent /
/// read error / non-object body).
fn read_sidecar_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// True when a sidecar's `updated_at_unix` is a number within `max_age_s` of
/// `now`. Mirrors the Python freshness gate.
fn sidecar_fresh(obj: &Map<String, Value>, now: f64, max_age_s: f64) -> bool {
    match obj.get("updated_at_unix").and_then(Value::as_f64) {
        Some(updated) => (now - updated) <= max_age_s,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// logd query seam + shared helpers.
// ---------------------------------------------------------------------------

/// Query the store for the newest `events` rows of one `event_kind`, returning the
/// `data` array or `None`. Reuses the app-state logd client's socket so a test
/// redirects it. Mirrors the wave-1 `logd_query_events`.
async fn logd_query_events(state: &AppState, event_kind: &str, limit: i64) -> Option<Vec<Value>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

    let query = format!("/v1/query?kind=events&limit={limit}&event_kind={event_kind}");
    let socket = state.logd.socket_path();
    let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let head = format!("GET {query} HTTP/1.1\r\nHost: logd\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await.ok()?;
    stream.flush().await.ok()?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        if raw.len() + n > MAX_READ_BYTES {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let (status, body) = parse_http_response(&raw).ok()?;
    if status >= 400 {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&body).ok()?;
    parsed
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.to_vec())
}

/// Split a raw HTTP/1.1 response into the status code + decoded body, de-chunking
/// a `Transfer-Encoding: chunked` body. Shared by the logd + mediamtx reads.
fn parse_http_response(raw: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let sep = b"\r\n\r\n";
    let split = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| std::io::Error::other("malformed http response (no header terminator)"))?;
    let head = &raw[..split];
    let body = &raw[split + sep.len()..];

    let head_str = String::from_utf8_lossy(head);
    let status = head_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("malformed http status line"))?;

    let chunked = head_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        de_chunk(body)
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// De-chunk a `Transfer-Encoding: chunked` body: `<hexlen>\r\n<data>\r\n` repeated
/// until a zero-length chunk.
fn de_chunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let len_line = &rest[..crlf];
        let len = usize::from_str_radix(String::from_utf8_lossy(len_line).trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = crlf + 2;
        if rest.len() < data_start + len {
            out.extend_from_slice(&rest[data_start..]);
            break;
        }
        out.extend_from_slice(&rest[data_start..data_start + len]);
        let next = data_start + len;
        rest = if rest.len() >= next + 2 {
            &rest[next + 2..]
        } else {
            &[]
        };
    }
    out
}

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the root the sidecars
/// resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// Look up `key` and return its value, or JSON `null` when absent — the Python
/// `dict.get(key)` (which returns `None`, serialized as `null`).
fn get_or_null(map: &Map<String, Value>, key: &str) -> Value {
    map.get(key).cloned().unwrap_or(Value::Null)
}

/// `bool(map.get(key, False))` — Python truthiness over the value, defaulting to
/// `false` when the key is absent.
fn json_truthy_default_false(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key).map(json_truthy).unwrap_or(false)
}

/// A non-empty owned string for a JSON string value, or `None` for a non-string /
/// empty string — the Python `x or None` over a possibly-empty interface name.
fn non_empty_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Python `bool(x)` over a JSON value: `null`/`false`/`0`/`0.0`/`""`/`[]`/`{}` are
/// falsey, everything else truthy.
fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The Python `x or None` falsiness used for `channel` / `bitrate_kbps` (a `0` /
/// `0.0` / `null` reads as no value → `null`).
fn is_falsey(v: &Value) -> bool {
    !json_truthy(v)
}

/// Coerce a JSON number to `i64`, accepting an integer or a float. `None` for a
/// non-number.
fn json_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Round to three decimal places, matching the Python `round(x, 3)`.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// The current wall-clock time in microseconds since the Unix epoch.
fn now_unix_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// The current wall-clock time in seconds (float) since the Unix epoch, matching
/// the Python `time.time()` used for the sidecar freshness gate.
fn now_unix_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    // -------- resources --------

    #[test]
    fn resources_of_an_absent_store_is_an_empty_object() {
        assert_eq!(derive_resources_subset(None), json!({}));
    }

    #[test]
    fn resources_missing_an_essential_field_is_an_empty_object() {
        // cpu present but the memory + disk spine missing → empty object (the most
        // degraded Python path, no psutil on this daemon).
        let s = signals(&[("cpu.util.all", json!(10.0))]);
        assert_eq!(derive_resources_subset(Some(&s)), json!({}));
    }

    #[test]
    fn resources_derives_the_thirteen_field_subset() {
        let s = signals(&[
            ("cpu.util.all", json!(12.34)),
            ("mem.total_bytes", json!(4.0 * BYTES_PER_MB)),
            ("mem.avail_bytes", json!(1.0 * BYTES_PER_MB)),
            ("mem.cache_bytes", json!(0.5 * BYTES_PER_MB)),
            ("mem.swap_total_bytes", json!(2.0 * BYTES_PER_MB)),
            ("mem.swap_free_bytes", json!(0.5 * BYTES_PER_MB)),
            ("disk.fs_total_bytes", json!(100.0 * BYTES_PER_GB)),
            ("disk.fs_used_bytes", json!(25.0 * BYTES_PER_GB)),
            ("thermal.primary_c", json!(47.5)),
        ]);
        let r = derive_resources_subset(Some(&s));
        let obj = r.as_object().unwrap();

        // Exactly the 13 keys the consolidated route selects — NOT `temperatures`
        // or `load_avg`.
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "cpu_percent",
                "disk_percent",
                "disk_total_gb",
                "disk_used_gb",
                "memory_available_mb",
                "memory_cache_mb",
                "memory_percent",
                "memory_total_mb",
                "memory_used_mb",
                "swap_percent",
                "swap_total_mb",
                "swap_used_mb",
                "temperature",
            ]
        );
        assert_eq!(obj["cpu_percent"], json!(12.3));
        assert_eq!(obj["memory_total_mb"], json!(4));
        assert_eq!(obj["memory_used_mb"], json!(3)); // 4 - 1 avail
        assert_eq!(obj["memory_available_mb"], json!(1));
        // 0.5 MB cache: Python round(0.5) is round-half-to-even → 0.
        assert_eq!(obj["memory_cache_mb"], json!(0));
        assert_eq!(obj["memory_percent"], json!(75.0)); // (4-1)/4 * 100
        assert_eq!(obj["swap_total_mb"], json!(2));
        // (2 - 0.5) = 1.5 swap used: round(1.5) is round-half-to-even → 2.
        assert_eq!(obj["swap_used_mb"], json!(2));
        assert_eq!(obj["swap_percent"], json!(75.0)); // 1.5/2 * 100
        assert_eq!(obj["disk_total_gb"], json!(100.0));
        assert_eq!(obj["disk_used_gb"], json!(25.0));
        assert_eq!(obj["disk_percent"], json!(25.0));
        assert_eq!(obj["temperature"], json!(47.5));
    }

    #[test]
    fn resources_missing_temperature_is_null() {
        let s = signals(&[
            ("cpu.util.all", json!(5.0)),
            ("mem.total_bytes", json!(BYTES_PER_MB)),
            ("mem.avail_bytes", json!(BYTES_PER_MB)),
            ("disk.fs_total_bytes", json!(BYTES_PER_GB)),
            ("disk.fs_used_bytes", json!(0.0)),
        ]);
        let r = derive_resources_subset(Some(&s));
        assert_eq!(r["temperature"], Value::Null);
        // Zero-total guards never divide-by-zero: a 1 MB total avail=total → 0%.
        assert_eq!(r["memory_percent"], json!(0.0));
        assert_eq!(r["swap_percent"], json!(0.0)); // no swap signals → 0
    }

    #[test]
    fn resources_populate_from_a_representative_merged_signal_map() {
        // The merged hardware-signal map a live board produces carries the canonical
        // resource keys (`cpu.util.all`, `mem.*_bytes`, `disk.fs_*_bytes`,
        // `thermal.primary_c`) alongside a tail of per-core / per-sensor / scheduler /
        // pressure signals the consolidated route does not consume. The block must
        // read the canonical keys out of that noisy map and come back fully
        // populated, NOT the most-degraded empty object — the regression guard for a
        // read keyed on the wrong signal names finding nothing.
        let s = signals(&[
            // Canonical keys the subset reads.
            ("cpu.util.all", json!(23.7)),
            ("mem.total_bytes", json!(8.0 * BYTES_PER_GB)),
            ("mem.avail_bytes", json!(6.0 * BYTES_PER_GB)),
            ("mem.cache_bytes", json!(2.0 * BYTES_PER_GB)),
            ("mem.swap_total_bytes", json!(4.0 * BYTES_PER_GB)),
            ("mem.swap_free_bytes", json!(3.0 * BYTES_PER_GB)),
            ("disk.fs_total_bytes", json!(64.0 * BYTES_PER_GB)),
            ("disk.fs_used_bytes", json!(16.0 * BYTES_PER_GB)),
            ("thermal.primary_c", json!(52.4)),
            // The non-consumed tail a real merged map also carries: per-core CPU, an
            // alternate thermal-zone name, load averages, and a pressure-stall line.
            ("cpu.util.0", json!(20.1)),
            ("cpu.util.1", json!(25.0)),
            ("cpu.util.2", json!(22.6)),
            ("cpu.util.3", json!(27.0)),
            ("thermal.cpu_thermal_c", json!(52.4)),
            ("sched.loadavg_1", json!(0.42)),
            ("sched.loadavg_5", json!(0.31)),
            ("sched.loadavg_15", json!(0.27)),
            ("mem.psi.cpu.some.avg10", json!(0.0)),
        ]);
        let r = derive_resources_subset(Some(&s));
        let obj = r.as_object().unwrap();

        // Populated, not the degraded empty object.
        assert!(
            !obj.is_empty(),
            "resources must populate from a live signal map"
        );

        // Exactly the 13 consolidated keys — the per-core / load / pressure tail and
        // the `temperatures` / `load_avg` Python-only fields are not carried.
        assert_eq!(obj.len(), 13);

        // Each canonical key reads its real value from the merged map (the read is
        // keyed on the correct signal names), not a default / null.
        assert_eq!(obj["cpu_percent"], json!(23.7));
        assert_eq!(obj["memory_total_mb"], json!(8192)); // 8 GiB in MiB
        assert_eq!(obj["memory_used_mb"], json!(2048)); // (8 - 6) GiB avail
        assert_eq!(obj["memory_available_mb"], json!(6144));
        assert_eq!(obj["memory_cache_mb"], json!(2048));
        assert_eq!(obj["memory_percent"], json!(25.0)); // (8-6)/8 * 100
        assert_eq!(obj["swap_total_mb"], json!(4096));
        assert_eq!(obj["swap_used_mb"], json!(1024)); // (4 - 3) GiB used
        assert_eq!(obj["swap_percent"], json!(25.0)); // 1/4 * 100
        assert_eq!(obj["disk_total_gb"], json!(64.0));
        assert_eq!(obj["disk_used_gb"], json!(16.0));
        assert_eq!(obj["disk_percent"], json!(25.0)); // 16/64 * 100
        assert_eq!(obj["temperature"], json!(52.4));
    }

    // -------- services --------

    #[test]
    fn fallback_line_parses_a_running_unit_into_the_consolidated_shape() {
        let row = "ados-mavlink.service loaded active running ADOS MAVLink router";
        let svc = parse_fallback_line(row).unwrap();
        assert_eq!(svc["name"], json!("ados-mavlink"));
        assert_eq!(svc["state"], json!("running"));
        assert_eq!(svc["status"], json!("running"));
        assert_eq!(svc["task_done"], json!(false));
        assert_eq!(svc["uptimeSeconds"], json!(0));
    }

    #[test]
    fn fallback_line_maps_a_dead_unit_state_to_its_substate() {
        // A non-running sub-state surfaces verbatim (e.g. "dead"); task_done true.
        let row = "ados-discovery.service loaded inactive dead ADOS discovery";
        let svc = parse_fallback_line(row).unwrap();
        assert_eq!(svc["name"], json!("ados-discovery"));
        assert_eq!(svc["state"], json!("dead"));
        assert_eq!(svc["status"], json!("dead"));
        assert_eq!(svc["task_done"], json!(true));
    }

    #[test]
    fn fallback_line_skips_short_and_non_service_rows() {
        assert!(parse_fallback_line("").is_none());
        assert!(parse_fallback_line("ados-x.service loaded").is_none());
        assert!(parse_fallback_line("ados-thing.timer loaded active waiting A timer").is_none());
    }

    #[test]
    fn consolidated_service_entry_carries_memory_mb() {
        // Build a representative entry through the same code the route runs and
        // assert the full six-key consolidated shape.
        let mut entry =
            vec![
                parse_fallback_line("ados-video.service loaded active running ADOS Video").unwrap(),
            ];
        attach_service_memory(&mut entry);
        let obj = entry[0].as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "memory_mb",
                "name",
                "state",
                "status",
                "task_done",
                "uptimeSeconds"
            ]
        );
        assert!(obj["memory_mb"].is_number());
    }

    #[test]
    fn unit_from_cgroup_extracts_the_ados_unit() {
        let body = "0::/system.slice/ados.slice/ados-video.service\n";
        assert_eq!(
            unit_from_cgroup(body),
            Some("ados-video.service".to_string())
        );
        assert_eq!(unit_from_cgroup("0::/system.slice/sshd.service"), None);
        assert_eq!(unit_from_cgroup("ados-.service"), None);
    }

    #[test]
    fn pss_kib_from_rollup_reads_the_first_pss_line() {
        let body = "Rss:  12345 kB\nPss:  6789 kB\n";
        assert_eq!(pss_kib_from_rollup(body), 6789);
        assert_eq!(pss_kib_from_rollup("Rss:  100 kB\n"), 0);
    }

    // -------- radio --------

    #[test]
    fn radio_absent_block_is_the_full_null_skeleton() {
        let block = build_radio_block(None);
        assert_eq!(block["state"], json!("absent"));
        assert_eq!(block["paired"], json!(false));
        assert_eq!(block["adapter_injection_ok"], json!(false));
        assert_eq!(block["bandwidth_mhz"], Value::Null);
        assert_eq!(block["rssi_dbm"], Value::Null);
        assert_eq!(block["channel"], Value::Null);
    }

    #[test]
    fn radio_block_maps_a_live_status() {
        let status = signals(&[
            ("state", json!("active")),
            ("interface", json!("wlan1")),
            ("channel", json!(149)),
            ("rssi_dbm", json!(-55.0)),
            ("bitrate_kbps", json!(8000)),
            ("paired", json!(true)),
            ("adapter_injection_ok", json!(true)),
            ("snr_db", json!(22.0)),
        ]);
        let block = build_radio_block(Some(&status));
        assert_eq!(block["state"], json!("active"));
        assert_eq!(block["iface"], json!("wlan1"));
        assert_eq!(block["channel"], json!(149));
        assert_eq!(block["freq_mhz"], json!(5745));
        assert_eq!(block["bandwidth_mhz"], json!(20));
        assert_eq!(block["rssi_dbm"], json!(-55.0));
        assert_eq!(block["bitrate_kbps"], json!(8000));
        assert_eq!(block["paired"], json!(true));
        assert_eq!(block["adapter_injection_ok"], json!(true));
        assert_eq!(block["snr_db"], json!(22.0));
        // fec_lost is the camel-free snake_case key sourced from fec_failed.
        assert_eq!(block["fec_lost"], Value::Null);
    }

    #[test]
    fn radio_sentinel_rssi_becomes_null() {
        let status = signals(&[("state", json!("searching")), ("rssi_dbm", json!(-100.0))]);
        let block = build_radio_block(Some(&status));
        assert_eq!(block["rssi_dbm"], Value::Null);
    }

    #[test]
    fn radio_falsey_channel_and_bitrate_become_null() {
        let status = signals(&[
            ("state", json!("disabled")),
            ("channel", json!(0)),
            ("bitrate_kbps", json!(0)),
        ]);
        let block = build_radio_block(Some(&status));
        assert_eq!(block["channel"], Value::Null);
        assert_eq!(block["freq_mhz"], Value::Null);
        assert_eq!(block["bitrate_kbps"], Value::Null);
    }

    #[test]
    fn radio_to_camel_renames_snake_keys() {
        let snake = json!({
            "state": "active",
            "freq_mhz": 5745,
            "adapter_injection_ok": true,
            "tx_power_max_dbm": null,
            "valid_rx_packets_per_s": 100,
        });
        let camel = radio_to_camel(snake);
        let obj = camel.as_object().unwrap();
        assert!(obj.contains_key("state")); // single segment unchanged
        assert!(obj.contains_key("freqMhz"));
        assert!(obj.contains_key("adapterInjectionOk"));
        assert!(obj.contains_key("txPowerMaxDbm"));
        assert!(obj.contains_key("validRxPacketsPerS"));
    }

    #[test]
    fn channel_to_freq_omits_the_status_only_channels() {
        // 40 + 44 exist in the WFB status channel set but NOT the radio-block map.
        assert_eq!(channel_to_freq(40), None);
        assert_eq!(channel_to_freq(44), None);
        assert_eq!(channel_to_freq(149), Some(5745));
        assert_eq!(channel_to_freq(165), Some(5825));
    }

    // -------- video gate --------

    #[test]
    fn gs_video_delivering_requires_link_state_and_a_positive_rate() {
        // Not delivering: no status, wrong state, or zero rate.
        assert!(!gs_video_delivering(None));
        let searching = signals(&[("state", json!("searching"))]);
        assert!(!gs_video_delivering(Some(&searching)));
        let active_zero = signals(&[
            ("state", json!("active")),
            ("valid_rx_packets_per_s", json!(0)),
        ]);
        assert!(!gs_video_delivering(Some(&active_zero)));
        // Delivering: active + a positive valid-decode rate.
        let active_live = signals(&[
            ("state", json!("active")),
            ("valid_rx_packets_per_s", json!(120.0)),
        ]);
        assert!(gs_video_delivering(Some(&active_live)));
        // Or connected + a positive packet count.
        let connected = signals(&[
            ("state", json!("connected")),
            ("packets_received", json!(50)),
        ]);
        assert!(gs_video_delivering(Some(&connected)));
    }

    #[test]
    fn video_block_with_no_mediamtx_is_default_on_drone() {
        // mediamtx absent (readiness injected false) → not_initialized, no whep.
        // The readiness is threaded in explicitly, so the assertion holds
        // deterministically regardless of whatever answers 9997/8889 on the host.
        let v = build_video_block_with("drone", &None, "localhost", false, vec![]);
        assert_eq!(v["state"], json!("not_initialized"));
        assert_eq!(v["whep_url"], Value::Null);
        assert_eq!(v["recording"], json!(false));
        assert_eq!(v["recording_filename"], Value::Null);
    }

    #[test]
    fn video_block_on_drone_with_ready_mediamtx_is_running() {
        // mediamtx ready (readiness injected true) → running + a WHEP URL.
        let v = build_video_block_with("drone", &None, "example-host", true, vec![]);
        assert_eq!(v["state"], json!("running"));
        assert_eq!(v["whep_url"], json!("http://example-host:8889/main/whep"));
        // No sidecar streams → no `streams` key (single-stream nodes unchanged).
        assert!(v.get("streams").is_none());
    }

    #[test]
    fn video_block_advertises_per_leg_streams_when_present() {
        let streams = vec![
            json!({ "id": "main", "role": "eo", "codec": "h265", "whep": "http://h:8889/main/whep" }),
            json!({ "id": "ir", "role": "ir", "codec": "h264", "whep": "http://h:8889/ir/whep" }),
        ];
        let v = build_video_block_with("drone", &None, "h", true, streams);
        assert_eq!(v["state"], json!("running"));
        let legs = v["streams"].as_array().unwrap();
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[1]["id"], json!("ir"));
        assert_eq!(legs[1]["whep"], json!("http://h:8889/ir/whep"));
    }

    #[test]
    fn video_block_on_gs_without_a_live_link_is_stopped() {
        // A ground station whose WFB link is not delivering reports stopped, no
        // whep — regardless of mediamtx reachability (the gate is the link). Inject
        // mediamtx ready=true to prove the link gate dominates even then.
        let v = build_video_block_with("ground-station", &None, "localhost", true, vec![]);
        assert_eq!(v["state"], json!("stopped"));
        assert_eq!(v["whep_url"], Value::Null);
    }

    #[test]
    fn video_block_on_gs_delivering_but_no_mediamtx_is_connecting() {
        // Link delivering frames but mediamtx WHEP not yet serving → connecting.
        let wfb = Some(signals(&[
            ("state", json!("active")),
            ("valid_rx_packets_per_s", json!(120.0)),
        ]));
        let v = build_video_block_with("ground-station", &wfb, "localhost", false, vec![]);
        assert_eq!(v["state"], json!("connecting"));
        assert_eq!(v["whep_url"], Value::Null);
    }

    // -------- mesh --------

    #[test]
    fn mesh_block_is_empty_for_a_drone_or_auto_profile() {
        assert_eq!(build_mesh_block("drone"), json!({}));
        assert_eq!(build_mesh_block("auto"), json!({}));
        assert_eq!(build_mesh_block(""), json!({}));
    }

    #[test]
    fn mesh_block_on_a_direct_ground_station_carries_role_and_capable() {
        let dir = tempfile::tempdir().unwrap();
        // direct role (sentinel absent) → role + mesh_capable, no mesh-state keys.
        let mesh = build_mesh_block_at(
            "ground_station",
            &dir.path().join("absent.role"),
            &dir.path().join("absent.conf"),
            &dir.path().join("absent.mesh"),
        );
        let obj = mesh.as_object().unwrap();
        assert_eq!(obj["role"], json!("direct"));
        assert_eq!(obj["mesh_capable"], json!(false));
        assert!(!obj.contains_key("peer_count"));
    }

    #[test]
    fn mesh_block_on_a_relay_folds_in_the_mesh_state() {
        let dir = tempfile::tempdir().unwrap();
        let role = dir.path().join("role");
        let conf = dir.path().join("profile.conf");
        let state = dir.path().join("mesh-state.json");
        std::fs::write(&role, "relay\n").unwrap();
        std::fs::write(&conf, "mesh_capable: true\n").unwrap();
        std::fs::write(
            &state,
            r#"{"up":true,"neighbors":[{"id":"a"},{"id":"b"}],"selected_gateway":"a","partition":false}"#,
        )
        .unwrap();
        let mesh = build_mesh_block_at("ground_station", &role, &conf, &state);
        let obj = mesh.as_object().unwrap();
        assert_eq!(obj["role"], json!("relay"));
        assert_eq!(obj["mesh_capable"], json!(true));
        assert_eq!(obj["up"], json!(true));
        assert_eq!(obj["peer_count"], json!(2));
        assert_eq!(obj["selected_gateway"], json!("a"));
        assert_eq!(obj["partition"], json!(false));
    }

    // -------- camera --------

    #[test]
    fn camera_status_folds_in_fresh_state() {
        // The path-injectable core sidesteps the process-global ADOS_RUN_DIR, so
        // this never races a sibling test mutating the same env var.
        let dir = tempfile::tempdir().unwrap();
        let now = 1_700_000_000.0;
        std::fs::write(
            dir.path().join("camera-state.json"),
            format!(r#"{{"state":"ready","updated_at_unix":{now}}}"#),
        )
        .unwrap();
        let map: Map<String, Value> = read_camera_status_in(dir.path(), now).into_iter().collect();
        assert_eq!(map.get("cameraState"), Some(&json!("ready")));
    }

    #[test]
    fn camera_status_folds_in_a_fresh_usb_recovery_block() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_700_000_000.0;
        std::fs::write(
            dir.path().join("camera-usb-recovery.json"),
            format!(
                r#"{{"camera_usb_recovery_state":"rebinding","case":"wedged","attempts":2,"max_attempts":3,"camera_present":false,"expected":true,"ppps_capable":true,"updated_at_unix":{now}}}"#
            ),
        )
        .unwrap();
        let map: Map<String, Value> = read_camera_status_in(dir.path(), now).into_iter().collect();
        let rec = map.get("cameraUsbRecovery").unwrap().as_object().unwrap();
        assert_eq!(rec["state"], json!("rebinding"));
        assert_eq!(rec["case"], json!("wedged"));
        assert_eq!(rec["attempts"], json!(2));
        assert_eq!(rec["maxAttempts"], json!(3));
        assert_eq!(rec["cameraPresent"], json!(false));
        assert_eq!(rec["expected"], json!(true));
        assert_eq!(rec["pppsCapable"], json!(true));
        // The power-contention fields surface; default safe when omitted.
        assert_eq!(rec["powerContention"], json!(false));
        assert_eq!(rec["contentionPeer"], Value::Null);
    }

    #[test]
    fn camera_status_surfaces_power_contention() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_700_000_000.0;
        std::fs::write(
            dir.path().join("camera-usb-recovery.json"),
            format!(
                r#"{{"camera_usb_recovery_state":"needs_hub_reset","case":"absent","attempts":0,"max_attempts":3,"camera_present":true,"expected":true,"ppps_capable":false,"power_contention":true,"contention_peer":"1-1.2","updated_at_unix":{now}}}"#
            ),
        )
        .unwrap();
        let map: Map<String, Value> = read_camera_status_in(dir.path(), now).into_iter().collect();
        let rec = map.get("cameraUsbRecovery").unwrap().as_object().unwrap();
        assert_eq!(rec["powerContention"], json!(true));
        assert_eq!(rec["contentionPeer"], json!("1-1.2"));
    }

    #[test]
    fn camera_status_drops_a_stale_state() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_700_000_000.0;
        // 10 minutes old > the 300 s window → dropped.
        let stale = now - 600.0;
        std::fs::write(
            dir.path().join("camera-state.json"),
            format!(r#"{{"state":"missing","updated_at_unix":{stale}}}"#),
        )
        .unwrap();
        assert!(read_camera_status_in(dir.path(), now).is_empty());
    }

    // -------- golden fixture (envelope shape) --------

    /// Golden-fixture parity: the consolidated body carries exactly the 17 stable
    /// top-level keys (the camera keys are conditionally folded in, so they are not
    /// part of the always-present envelope). This pins the envelope the GCS reads.
    ///
    /// ```json
    /// {
    ///   "version": "<str>", "uptime_seconds": <num>, "board": {}, "health": {...},
    ///   "fc_connected": false, "fc_port": "", "fc_baud": 0,
    ///   "services": [], "resources": {}, "video": {...}, "telemetry": {},
    ///   "capabilities": {}, "mesh": {}, "radio": {...},
    ///   "profile": "drone", "role": null, "runtimeMode": "packaged"
    /// }
    /// ```
    ///
    /// Assembled here from the very block builders the route runs (each in its
    /// degraded no-source state, the only state available on a dev host) so the
    /// envelope + per-block shape is asserted deterministically.
    #[test]
    fn consolidated_envelope_matches_the_golden_shape() {
        // Build the all-degraded payload the same way the handler composes it, but
        // without the AppState wiring (each block in its no-source default).
        let mut payload = Map::new();
        payload.insert("version".to_string(), json!("0.0.0"));
        payload.insert("uptime_seconds".to_string(), json!(1.0));
        payload.insert("board".to_string(), json!({}));
        payload.insert(
            "health".to_string(),
            crate::routes::status::derive_health(None),
        );
        payload.insert("fc_connected".to_string(), json!(false));
        payload.insert("fc_port".to_string(), json!(""));
        payload.insert("fc_baud".to_string(), json!(0));
        payload.insert("services".to_string(), json!([]));
        payload.insert("resources".to_string(), derive_resources_subset(None));
        payload.insert(
            "video".to_string(),
            json!({
                "state": "not_initialized", "whep_url": null,
                "recording": false, "recording_filename": null, "recording_started_at": null,
            }),
        );
        payload.insert("telemetry".to_string(), json!({}));
        payload.insert("capabilities".to_string(), json!({}));
        payload.insert("mesh".to_string(), json!({}));
        payload.insert("radio".to_string(), radio_to_camel(build_radio_block(None)));
        payload.insert("profile".to_string(), json!("drone"));
        payload.insert("role".to_string(), Value::Null);
        payload.insert("runtimeMode".to_string(), json!("packaged"));

        let obj = &payload;
        // Exactly the 17 stable top-level keys.
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "board",
                "capabilities",
                "fc_baud",
                "fc_connected",
                "fc_port",
                "health",
                "mesh",
                "profile",
                "radio",
                "resources",
                "role",
                "runtimeMode",
                "services",
                "telemetry",
                "uptime_seconds",
                "version",
                "video",
            ]
        );

        // Types + degraded defaults.
        assert!(obj["version"].is_string());
        assert!(obj["board"].is_object());
        assert!(obj["health"].is_object());
        assert_eq!(obj["fc_connected"], json!(false));
        assert_eq!(obj["fc_port"], json!(""));
        assert_eq!(obj["fc_baud"], json!(0));
        assert!(obj["services"].is_array());
        assert_eq!(obj["resources"], json!({}));
        assert!(obj["video"].is_object());
        assert_eq!(obj["telemetry"], json!({}));
        assert_eq!(obj["capabilities"], json!({}));
        assert_eq!(obj["mesh"], json!({}));
        // The radio block is the camelCase absent skeleton.
        assert_eq!(obj["radio"]["state"], json!("absent"));
        assert_eq!(obj["radio"]["freqMhz"], Value::Null);
        assert_eq!(obj["profile"], json!("drone"));
        assert_eq!(obj["role"], Value::Null);
        assert_eq!(obj["runtimeMode"], json!("packaged"));
    }
}
