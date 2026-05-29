//! Cloud status heartbeat loop.
//!
//! Every 5 s, when paired, POST the frozen [`HeartbeatPayload`] to
//! `{convex}/agent/status` with `X-ADOS-Key` auth. Ports
//! `src/ados/services/cloud/heartbeat_loop.py`.
//!
//! ## Enrichment seam (the 748-LOC judgment call)
//!
//! The Python payload folds in psutil/systemctl/board enrichment
//! (`heartbeat.py` + `core/main/heartbeat_payload.py`): CPU/mem/disk samples,
//! per-service status, the radio block, LCD/display fields, CAN buses, etc.
//! Porting that probing into Rust is high-risk and high-churn (it shells
//! systemctl, reads many sidecars, computes psutil deltas).
//!
//! The seam chosen here is the **lowest-risk faithful one**: the enrichment
//! stays the Python agent's job, written to an OPTIONAL JSON sidecar at
//! `/run/ados/cloud-enrichment.json` (a thin producer the agent already has the
//! plumbing for). The Rust loop reads that sidecar each tick and folds its keys
//! over the deterministic native fields, then null-strips and POSTs. When the
//! sidecar is absent or stale the loop still emits a valid heartbeat with the
//! required fields (`deviceId`/`version`/`uptimeSeconds`) plus whatever native
//! fields it has — every enrichment field is `Option` + skip-if-none on the
//! frozen payload, so absence is wire-valid and the GCS degrades gracefully.
//! No subprocess is spawned from Rust, no psutil is reimplemented, and the wire
//! `HeartbeatPayload` (frozen + golden-tested) stays byte-identical.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::heartbeat::{HeartbeatPayload, RadioBlock, RemoteAccess};

/// The enrichment sidecar the Python producer writes. Read each tick; absent or
/// stale → the loop emits the native-only payload.
pub const ENRICHMENT_SIDECAR: &str = "/run/ados/cloud-enrichment.json";

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
        cpu_percent: 0.0,
        memory_percent: 0.0,
        disk_percent: 0.0,
        temperature: None,
        memory_used_mb: 0,
        memory_total_mb: 0,
        disk_used_gb: 0.0,
        disk_total_gb: 0.0,
        cpu_cores: 0,
        board_ram_mb: 0,
        cpu_history: vec![],
        memory_history: vec![],
        fc_connected: false,
        fc_port: String::new(),
        fc_baud: 0,
        services: vec![],
        last_ip: String::new(),
        mdns_host: String::new(),
        setup_url: String::new(),
        api_url: String::new(),
        agent_version: base.version.clone(),
        video_state: "stopped".to_string(),
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
    }
}

/// Read the enrichment sidecar, returning its JSON object or `None` when absent
/// / unparseable. The path is overridable via `ADOS_CLOUD_ENRICHMENT` for tests.
pub fn read_enrichment() -> Option<serde_json::Value> {
    let path = std::env::var("ADOS_CLOUD_ENRICHMENT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(ENRICHMENT_SIDECAR));
    read_enrichment_from(&path)
}

/// Read the enrichment sidecar from an explicit path.
pub fn read_enrichment_from(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if v.is_object() {
        Some(v)
    } else {
        None
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

    #[test]
    fn read_enrichment_absent_is_none() {
        assert!(read_enrichment_from(Path::new("/nonexistent/ados/enrich.json")).is_none());
    }

    #[test]
    fn read_enrichment_malformed_is_none() {
        let mut p = std::env::temp_dir();
        p.push(format!("ados-enrich-bad-{}.json", std::process::id()));
        std::fs::write(&p, b"not json").unwrap();
        assert!(read_enrichment_from(&p).is_none());
        let _ = std::fs::remove_file(&p);
    }
}
