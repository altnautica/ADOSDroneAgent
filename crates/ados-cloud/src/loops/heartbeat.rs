//! Cloud status heartbeat loop.
//!
//! Every 5 s, when paired, POST the frozen [`HeartbeatPayload`] to
//! `{convex}/agent/status` with `X-ADOS-Key` auth. Ports
//! `src/ados/services/cloud/heartbeat_loop.py`.
//!
//! ## Native enrichment
//!
//! The Python payload folds in psutil/systemctl/board enrichment: CPU/mem/disk
//! samples, per-service status, the radio block, LCD/display fields, CAN buses,
//! etc. The live status the GCS needs (resources + FC link + service fleet) is
//! built natively in Rust by the [`crate::loops::enrichment`] producer each tick
//! and folded over the deterministic native base here via [`build_payload`].
//!
//! The base itself ([`HeartbeatBase`] → [`native_payload`]) carries only the
//! fields the loop always has without probing (device identity, version, board),
//! with every enrichment field left `Option` + skip-if-none on the frozen
//! payload. So even if the enrichment producer returns nothing, the loop emits a
//! valid heartbeat with the required fields (`deviceId`/`version`/
//! `uptimeSeconds`) and absence reads as honest "unknown" rather than a
//! fabricated `0` / `false` / `"stopped"` (operating rule 37). The wire
//! `HeartbeatPayload` (frozen + golden-tested) stays byte-identical.

use std::time::Duration;

use crate::heartbeat::{
    ClusterSlave, ConfigErrorEntry, HeartbeatPayload, RadioBlock, RemoteAccess,
};

/// The compute-node heartbeat sidecar written by `ados-compute`
/// (`/run/ados/compute-heartbeat.json`). Absent on a non-compute node — then
/// every compute field stays `None` and is omitted from the heartbeat.
const COMPUTE_HEARTBEAT_SIDECAR: &str = "/run/ados/compute-heartbeat.json";

/// A compute sidecar not re-written within this window is treated as absent, so
/// a dead/hung `ados-compute` (whose tmpfs file persists) never makes the relay
/// fold a frozen-but-live compute state forever (operating rule 44). 4x the
/// producer's 5 s write cadence.
const COMPUTE_SIDECAR_STALE_MS: i64 = 20_000;

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComputeSidecar {
    /// The producer's sidecar schema version (absent ⇒ `0` from an older writer).
    #[serde(default)]
    version: u16,
    /// The producer's write time; absent/stale ⇒ the sidecar is treated as gone.
    generated_at_ms: Option<i64>,
    compute_role: Option<String>,
    compute_cluster_master_id: Option<String>,
    compute_queue_depth: Option<i64>,
    compute_active_jobs: Option<i64>,
    compute_workers_idle: Option<i64>,
    compute_cluster_aggregate_workers_idle: Option<i64>,
    compute_cluster_slaves: Option<Vec<ClusterSlave>>,
}

/// Read + parse the compute heartbeat sidecar at `path`, or `None` when it is
/// absent, unparseable, missing its write-time, or STALE (older than the
/// staleness budget at `now_ms`). A stale file folds to absent compute fields,
/// so a dead/hung producer stops asserting frozen state on the heartbeat.
fn read_compute_sidecar_from(path: &std::path::Path, now_ms: i64) -> Option<ComputeSidecar> {
    let text = std::fs::read_to_string(path).ok()?;
    let sidecar: ComputeSidecar = serde_json::from_str(&text).ok()?;
    match sidecar.generated_at_ms {
        Some(gen) if now_ms.saturating_sub(gen) <= COMPUTE_SIDECAR_STALE_MS => {
            // Best-effort drift signal: warn (never reject) on a producer/reader
            // version mismatch, then fold the sidecar in anyway.
            ados_protocol::sidecar::check_sidecar_version(
                "compute-heartbeat",
                sidecar.version,
                ados_compute::COMPUTE_HEARTBEAT_SIDECAR_VERSION,
            );
            Some(sidecar)
        }
        _ => None,
    }
}

fn read_compute_sidecar(now_ms: i64) -> Option<ComputeSidecar> {
    read_compute_sidecar_from(std::path::Path::new(COMPUTE_HEARTBEAT_SIDECAR), now_ms)
}

/// Local epoch ms for the staleness gate.
fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The directory every plugin / feature writes its own state sidecar into
/// (`<id>-state.json`): sandboxed plugins via the plugin host, plus first-party
/// services (e.g. `ados-atlas`) that surface telemetry the same way. The
/// heartbeat ferries each slice opaquely under `pluginState[<id>]`.
const PLUGIN_STATE_DIR: &str = "/run/ados/plugins";

/// A plugin sidecar not re-written within this window is treated as absent, so a
/// dead/hung producer (whose tmpfs file persists) never makes the relay fold a
/// frozen-but-live slice forever (operating rule 44). Mirrors the on-box
/// `/api/plugins/{id}/state` 10 s gate, with a little slack.
const PLUGIN_STATE_STALE: Duration = Duration::from_secs(15);

/// Read every fresh plugin/feature state sidecar in `dir` into a map keyed by id
/// (the filename minus `-state.json`), each value the sidecar's JSON verbatim.
/// The core never inspects a slice's shape — each plugin owns + validates its
/// own. Staleness is gated on the file mtime (uniform across every producer's
/// sidecar format, whatever its payload looks like), so a producer that stopped
/// writing drops out. An absent dir (a non-plugin device) yields an empty map.
/// `now` is the reference instant the mtime ages against (injected for tests).
fn read_plugin_state_sidecars_from(
    dir: &std::path::Path,
    now: std::time::SystemTime,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix("-state.json"))
        else {
            continue;
        };
        // mtime staleness gate. A future mtime (clock skew) counts as fresh.
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|mtime| {
                now.duration_since(mtime)
                    .map(|age| age <= PLUGIN_STATE_STALE)
                    .unwrap_or(true)
            })
            .unwrap_or(false);
        if !fresh {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        out.insert(id.to_string(), val);
    }
    out
}

fn read_plugin_state_sidecars() -> serde_json::Map<String, serde_json::Value> {
    read_plugin_state_sidecars_from(
        std::path::Path::new(PLUGIN_STATE_DIR),
        std::time::SystemTime::now(),
    )
}

/// The directory services publish their config-status sidecar into
/// (`config-status-<service>.json`) at startup. Honors the `ADOS_RUN_DIR`
/// override (default `/run/ados`), matching the `ados_config::write_config_status`
/// writer, so a redirected runtime layout (a non-root dev host or a test) reads
/// the same dir it wrote.
fn config_status_dir() -> std::path::PathBuf {
    std::env::var_os("ADOS_RUN_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/run/ados"))
}

/// The readable slice of a `config-status-<service>.json` sidecar. The writer
/// also stamps `generated_at_ms`; the heartbeat only needs the service label and
/// its current error.
#[derive(Debug, Default, serde::Deserialize)]
struct ConfigStatusSidecar {
    /// The sidecar schema version. `#[serde(default)]` makes a file written by an
    /// older agent (no `version` key) read back as `0`, a best-effort drift
    /// signal rather than a parse failure.
    #[serde(default)]
    version: u16,
    service: Option<String>,
    error: Option<String>,
}

/// Read every `config-status-<service>.json` sidecar in `dir` and collect the
/// ones whose current error is non-null into a `{service, error}` list. A service
/// with a valid config publishes `error: null` and is omitted, so the list
/// carries only LIVE config faults. The atomic writer's `.json.tmp.<pid>` staging
/// files do not end in `.json`, so they are skipped, as is any unrelated file.
/// The result is sorted by service so the wire is deterministic (`read_dir` order
/// is unspecified). An absent dir (a node with no config-status sidecars) yields
/// an empty vec.
fn read_config_error_sidecars_from(dir: &std::path::Path) -> Vec<ConfigErrorEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_status = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("config-status-") && n.ends_with(".json"))
            .unwrap_or(false);
        if !is_status {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(status) = serde_json::from_str::<ConfigStatusSidecar>(&text) else {
            continue;
        };
        // Best-effort schema-drift signal: an older file (version 0) warns but is
        // still used. Never a reject.
        ados_protocol::sidecar::check_sidecar_version(
            "config-status",
            status.version,
            ados_config::CONFIG_STATUS_SIDECAR_VERSION,
        );
        if let (Some(service), Some(error)) = (status.service, status.error) {
            out.push(ConfigErrorEntry { service, error });
        }
    }
    out.sort_by(|a, b| a.service.cmp(&b.service));
    out
}

fn read_config_error_sidecars() -> Vec<ConfigErrorEntry> {
    read_config_error_sidecars_from(&config_status_dir())
}

/// Heartbeat cadence. Mirrors the Python loop's 5 s base sleep.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// The deterministic, native inputs the loop always has without any probing:
/// the device identity, version, profile, and uptime. Everything else is
/// enrichment folded from the sidecar.
#[derive(Debug, Clone)]
pub struct HeartbeatBase {
    pub device_id: String,
    pub version: String,
    pub profile: Option<String>,
    pub role: Option<String>,
    pub uptime_seconds: i64,
    pub board_name: String,
    pub board_tier: i64,
    pub board_soc: String,
    pub board_arch: String,
}

/// Build the heartbeat wire object from the native base plus an optional
/// enrichment JSON object. The enrichment is the Python sidecar's contents
/// (already in the frozen wire shape — camelCase root keys, snake_case `radio`);
/// its keys are folded OVER the native base, then the required base fields
/// (`deviceId` / `version` / `uptimeSeconds`) are re-asserted so a producer can
/// never drop or diverge them, and the top level is null-stripped (Convex
/// `v.optional` rejects an explicit null). A `None` enrichment yields the
/// native-only object with an all-`absent` radio block and no optional fields.
///
/// Returns a `serde_json::Value` (the POST body): the producer owns the full
/// payload shape, and operating at the value level keeps the merge faithful
/// without forcing every frozen field to be deserialize-tolerant. The frozen
/// [`HeartbeatPayload`] is the source of the native base object's shape +
/// casing. This is the testable per-tick assembly; the loop calls it then POSTs.
pub fn build_payload(
    base: &HeartbeatBase,
    enrichment: Option<&serde_json::Value>,
) -> serde_json::Value {
    // The native base, in the frozen wire shape (camelCase root, snake_case
    // radio, required fields set, optionals absent).
    let native = native_payload(base).to_value();
    let mut obj = native.as_object().cloned().unwrap_or_default();

    // Fold the enrichment keys over the base.
    if let Some(serde_json::Value::Object(map)) = enrichment {
        for (k, v) in map {
            obj.insert(k.clone(), v.clone());
        }
    }

    // Re-assert the required base identity so a producer cannot drop or diverge
    // it (the wire contract demands these three).
    obj.insert("deviceId".to_string(), serde_json::json!(base.device_id));
    obj.insert("version".to_string(), serde_json::json!(base.version));
    obj.insert(
        "uptimeSeconds".to_string(),
        serde_json::json!(base.uptime_seconds),
    );

    // Null-strip the top level: Convex `v.optional(T)` accepts absent-or-T, not
    // an explicit null. The nested `radio` object keeps its own nulls (matching
    // the Python loop, which strips only top-level keys).
    obj.retain(|_, v| !v.is_null());
    serde_json::Value::Object(obj)
}

/// A payload carrying only the required + native fields, an all-`absent` radio
/// block, and no optional enrichment.
fn native_payload(base: &HeartbeatBase) -> HeartbeatPayload {
    // Fold the compute-node sidecar (compute profile only; None elsewhere, and
    // None when the file is stale so a dead producer is not folded forever).
    let compute = read_compute_sidecar(now_epoch_ms()).unwrap_or_default();
    // Ferry every fresh plugin/feature state slice opaquely (empty map omitted).
    let plugin_state = {
        let slices = read_plugin_state_sidecars();
        if slices.is_empty() {
            None
        } else {
            Some(slices)
        }
    };
    // Surface any LIVE service config-parse fault from the config-status sidecars
    // (empty ⇒ omitted, so a healthy node's wire is unchanged).
    let config_errors = {
        let errs = read_config_error_sidecars();
        if errs.is_empty() {
            None
        } else {
            Some(errs)
        }
    };
    HeartbeatPayload {
        device_id: base.device_id.clone(),
        version: base.version.clone(),
        profile: base.profile.clone(),
        role: base.role.clone(),
        uptime_seconds: base.uptime_seconds,
        board_name: base.board_name.clone(),
        board_tier: base.board_tier,
        board_soc: base.board_soc.clone(),
        board_arch: base.board_arch.clone(),
        // Unmeasured by the native loop: omitted (None) so the wire says
        // "unknown" rather than asserting a 0 / false / "stopped" reading the
        // loop never took (operating rule 37). The Python enrichment producer
        // folds the real values over these absences each tick.
        cpu_percent: None,
        memory_percent: None,
        disk_percent: None,
        temperature: None,
        memory_used_mb: 0,
        memory_total_mb: 0,
        disk_used_gb: 0.0,
        disk_total_gb: 0.0,
        cpu_cores: 0,
        board_ram_mb: 0,
        cpu_history: vec![],
        memory_history: vec![],
        fc_connected: None,
        fc_port: String::new(),
        fc_baud: 0,
        // The FC link gated-truth detail is the enrichment producer's to lift from
        // the state snapshot; the native base leaves it absent (honest "unknown").
        transport_open: None,
        mavlink_alive: None,
        heartbeat_age_s: None,
        fc_source: None,
        fc_link_hint: None,
        services: None,
        last_ip: String::new(),
        mdns_host: String::new(),
        setup_url: String::new(),
        api_url: String::new(),
        agent_version: base.version.clone(),
        video_state: None,
        video_whep_port: 0,
        mavlink_ws_port: 0,
        mavlink_ws_url: None,
        video_whep_url: None,
        mission_control_url: None,
        remote_access: RemoteAccess {
            provider: "none".to_string(),
            public_urls: vec![],
        },
        last_plugin_update_check_at: None,
        peripherals: None,
        radio: RadioBlock::absent(),
        wfb_adapter_chipset: None,
        wfb_adapter_injection_ok: false,
        lcd_active_page: None,
        ui_theme: None,
        lcd_touch_calibrated: None,
        lcd_rotation: None,
        lcd_snapshot_url: None,
        lcd_last_touch_at: None,
        lcd_last_gesture: None,
        video_local_decoder_active: None,
        video_local_decoder_type: None,
        video_local_decoder_fps: None,
        video_recording: None,
        video_pipeline_flavor: None,
        video_encoder_name: None,
        video_encoder_hw_accel: None,
        video_camera_source: None,
        video_pipeline_state: None,
        display_type: None,
        can_buses: None,
        compute_role: compute.compute_role,
        compute_cluster_master_id: compute.compute_cluster_master_id,
        compute_queue_depth: compute.compute_queue_depth,
        compute_active_jobs: compute.compute_active_jobs,
        compute_workers_idle: compute.compute_workers_idle,
        compute_cluster_aggregate_workers_idle: compute.compute_cluster_aggregate_workers_idle,
        compute_cluster_slaves: compute.compute_cluster_slaves,
        plugin_state,
        config_errors,
    }
}

/// POST one heartbeat to `{convex}/agent/status` with `X-ADOS-Key`. Best-effort:
/// a transport error or non-200 is logged, never fatal. Mirrors the Python
/// loop's POST (header auth, not URL).
pub async fn post_heartbeat(
    client: &reqwest::Client,
    convex_url: &str,
    api_key: &str,
    body: &serde_json::Value,
) {
    let url = format!("{}/agent/status", convex_url.trim_end_matches('/'));
    match client
        .post(&url)
        .header("X-ADOS-Key", api_key)
        .json(body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!("cloud status sent");
        }
        Ok(resp) => {
            tracing::warn!(status = resp.status().as_u16(), "cloud status rejected");
        }
        Err(e) => {
            tracing::debug!(error = %e, "cloud heartbeat failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> HeartbeatBase {
        HeartbeatBase {
            device_id: "dev1".to_string(),
            version: "0.1.0".to_string(),
            profile: Some("drone".to_string()),
            role: None,
            uptime_seconds: 42,
            board_name: "rock-5c-lite".to_string(),
            board_tier: 3,
            board_soc: "rk3582".to_string(),
            board_arch: "aarch64".to_string(),
        }
    }

    fn write_sidecar(dir: &std::path::Path, body: serde_json::Value) -> std::path::PathBuf {
        use std::io::Write;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("compute-heartbeat.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(body.to_string().as_bytes())
            .unwrap();
        path
    }

    #[test]
    fn a_fresh_compute_sidecar_folds_but_a_stale_or_missing_one_does_not() {
        let dir = std::env::temp_dir().join(format!("ados-cloud-hb-{}", std::process::id()));
        let path = write_sidecar(
            &dir,
            serde_json::json!({
                "generatedAtMs": 1_000_000,
                "computeRole": "master",
                "computeClusterMasterId": "node-a",
                "computeQueueDepth": 2,
                "computeActiveJobs": 1,
                "computeWorkersIdle": 3,
                "computeClusterAggregateWorkersIdle": 5,
                "computeClusterSlaves": [
                    {"nodeId": "s1", "accelerators": ["mps"], "workersIdle": 1, "queueDepth": 0}
                ]
            }),
        );
        // Fresh (within the 20 s budget) → folds.
        let fresh = read_compute_sidecar_from(&path, 1_000_000 + 5_000).unwrap();
        assert_eq!(fresh.compute_role.as_deref(), Some("master"));
        assert_eq!(fresh.compute_workers_idle, Some(3));
        assert_eq!(fresh.compute_cluster_slaves.unwrap()[0].node_id, "s1");
        // Stale (past the budget) → None: a dead/hung producer is not folded.
        assert!(read_compute_sidecar_from(&path, 1_000_000 + 25_000).is_none());
        // Missing file → None.
        assert!(read_compute_sidecar_from(&dir.join("nope.json"), 1_000_000).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_sidecar_without_a_write_time_is_treated_as_absent() {
        let dir = std::env::temp_dir().join(format!("ados-cloud-hb-nots-{}", std::process::id()));
        // No generatedAtMs → conservative: treated as gone (cannot age-gate it).
        let path = write_sidecar(
            &dir,
            serde_json::json!({ "computeRole": "master", "computeWorkersIdle": 3 }),
        );
        assert!(read_compute_sidecar_from(&path, 1_000_000).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn write_named(dir: &std::path::Path, name: &str, body: &str) {
        use std::io::Write;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::File::create(dir.join(name))
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
    }

    #[test]
    fn fresh_plugin_sidecars_fold_keyed_by_id_with_the_slice_verbatim() {
        let dir = std::env::temp_dir().join(format!("ados-cloud-plugins-{}", std::process::id()));
        write_named(
            &dir,
            "atlas-state.json",
            r#"{"state":"active","gaussianCount":42}"#,
        );
        write_named(&dir, "follow-me-state.json", r#"{"lock":"locked"}"#);
        write_named(&dir, "bad-state.json", "{ not json");
        write_named(&dir, "notes.txt", "ignored: not a *-state.json file");

        let out = read_plugin_state_sidecars_from(&dir, std::time::SystemTime::now());
        // Keyed by id (filename minus -state.json); the slice is opaque/verbatim.
        assert_eq!(out["atlas"]["state"], "active");
        assert_eq!(out["atlas"]["gaussianCount"], 42);
        assert_eq!(out["follow-me"]["lock"], "locked");
        // Malformed JSON + non-state files are skipped, never the whole read.
        assert!(!out.contains_key("bad"));
        assert!(!out.contains_key("notes"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_plugin_sidecar_is_dropped() {
        let dir =
            std::env::temp_dir().join(format!("ados-cloud-plugins-stale-{}", std::process::id()));
        write_named(&dir, "atlas-state.json", r#"{"state":"active"}"#);
        // A reference `now` an hour after the just-written file -> past the gate.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        assert!(read_plugin_state_sidecars_from(&dir, later).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absent_plugin_dir_is_an_empty_map() {
        let dir = std::env::temp_dir().join("ados-cloud-plugins-nope-does-not-exist");
        assert!(read_plugin_state_sidecars_from(&dir, std::time::SystemTime::now()).is_empty());
    }

    #[test]
    fn config_status_sidecars_surface_only_live_errors_sorted_by_service() {
        let dir = std::env::temp_dir().join(format!("ados-cloud-cfgstatus-{}", std::process::id()));
        // Two faulty services (unsorted on disk), one healthy (null error → skip).
        write_named(
            &dir,
            "config-status-mavlink.json",
            r#"{"service":"mavlink","error":"invalid type: string, expected u32","generated_at_ms":1}"#,
        );
        write_named(
            &dir,
            "config-status-cloud.json",
            r#"{"service":"cloud","error":"unknown field `bogus`","generated_at_ms":2}"#,
        );
        write_named(
            &dir,
            "config-status-supervisor.json",
            r#"{"service":"supervisor","error":null,"generated_at_ms":3}"#,
        );
        // Non-matching + staging + malformed files are ignored, never the read.
        write_named(&dir, "config-status-bad.json", "{ not json");
        write_named(
            &dir,
            "config-status-ground_station.json.tmp.999",
            r#"{"service":"ground_station","error":"x"}"#,
        );
        write_named(&dir, "notes.txt", "unrelated");

        let out = read_config_error_sidecars_from(&dir);
        // Only the two live errors, sorted by service (cloud before mavlink).
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].service, "cloud");
        assert_eq!(out[0].error, "unknown field `bogus`");
        assert_eq!(out[1].service, "mavlink");
        // The healthy (null-error) service and the staging/malformed/other files
        // are absent.
        assert!(!out.iter().any(|e| e.service == "supervisor"));
        assert!(!out.iter().any(|e| e.service == "ground_station"));
        assert!(!out.iter().any(|e| e.service == "bad"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absent_config_status_dir_is_an_empty_list() {
        let dir = std::env::temp_dir().join("ados-cloud-cfgstatus-nope-does-not-exist");
        assert!(read_config_error_sidecars_from(&dir).is_empty());
    }

    #[test]
    fn config_status_version_matches_registry() {
        // The const lives in the writer crate (ados-config); this reader crate
        // sees both it and the shared registry, so the drift gate lives here.
        assert_eq!(
            ados_config::CONFIG_STATUS_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("config-status").unwrap()
        );
    }

    #[test]
    fn native_only_payload_has_required_fields_and_strips_optionals() {
        let v = build_payload(&base(), None);
        let obj = v.as_object().unwrap();
        // Required-on-wire present.
        assert_eq!(obj["deviceId"], "dev1");
        assert_eq!(obj["version"], "0.1.0");
        assert_eq!(obj["uptimeSeconds"], 42);
        assert_eq!(obj["boardName"], "rock-5c-lite");
        // role None is stripped; no optional enrichment present.
        assert!(!obj.contains_key("role"));
        assert!(!obj.contains_key("temperature"));
        assert!(!obj.contains_key("peripherals"));
        // The unmeasured-by-native fields are OMITTED, not asserted as 0/false/
        // "stopped"/[] (operating rule 37). They reappear only via the producer.
        assert!(!obj.contains_key("cpuPercent"));
        assert!(!obj.contains_key("memoryPercent"));
        assert!(!obj.contains_key("diskPercent"));
        assert!(!obj.contains_key("fcConnected"));
        assert!(!obj.contains_key("services"));
        assert!(!obj.contains_key("videoState"));
        // No top-level key is JSON null.
        for (k, val) in obj {
            assert!(!val.is_null(), "{k} must not be null on the wire");
        }
        // radio is the absent block.
        assert_eq!(obj["radio"]["state"], "absent");
    }

    #[test]
    fn enrichment_folds_over_the_native_base() {
        // The producer supplies CPU/temperature + a populated radio block, in the
        // frozen wire shape. They overlay the native zeros.
        let enrich = serde_json::json!({
            "cpuPercent": 12.5,
            "temperature": 47.0,
            "videoState": "running",
            "videoWhepPort": 8889,
            "radio": {
                "state": "connected",
                "channel": 149,
                "freq_mhz": 5745,
                "paired": true,
                "adapter_injection_ok": true
            },
            "wfbAdapterInjectionOk": true
        });
        let v = build_payload(&base(), Some(&enrich));
        let obj = v.as_object().unwrap();
        assert_eq!(obj["cpuPercent"], 12.5);
        assert_eq!(obj["temperature"], 47.0);
        assert_eq!(obj["videoState"], "running");
        // The required base fields survive the fold.
        assert_eq!(obj["deviceId"], "dev1");
        assert_eq!(obj["uptimeSeconds"], 42);
        // The radio sub-block stays snake_case after the fold.
        assert_eq!(obj["radio"]["freq_mhz"], 5745);
        assert_eq!(obj["radio"]["state"], "connected");
        assert_eq!(obj["wfbAdapterInjectionOk"], true);
    }
}
