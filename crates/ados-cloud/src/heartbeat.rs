//! Frozen wire model for the cloud status heartbeat.
//!
//! The cloud relay POSTs a status payload to `{convex}/agent/status` on a 5 s
//! cadence; Mission Control reads it to populate the per-drone view. This module
//! reproduces that payload's WIRE SHAPE byte-identically with the Python emit in
//! `src/ados/services/cloud/heartbeat_loop.py` (the source of truth) so the
//! relay can move to Rust without the receiver noticing.
//!
//! Three wire rules are load-bearing and are encoded in the types here:
//!
//! 1. **Casing.** Every root key is camelCase EXCEPT the `radio` sub-block,
//!    whose keys are snake_case (the Python `build_radio_block` emits snake_case
//!    keys verbatim). The root struct carries `#[serde(rename_all =
//!    "camelCase")]`; [`RadioBlock`] uses its own field names (already
//!    snake_case) with no rename.
//! 2. **Null-stripping.** The receiver's schema is `v.optional(T)`, which
//!    accepts "field absent OR T" but rejects an explicit JSON `null`. The
//!    Python loop strips every top-level `None`-valued key before the POST
//!    (`payload = {k: v for k, v in payload.items() if v is not None}`). Every
//!    optional field here is `Option<T>` with
//!    `#[serde(skip_serializing_if = "Option::is_none")]`, so a `None` is
//!    omitted, never serialized as `null`.
//! 3. **Required-on-wire.** `deviceId`, `version`, and `uptimeSeconds` are
//!    always present, so they are plain (non-`Option`) fields.
//!
//! The `radio` block is always emitted by the Python loop (an `absent` block
//! when no radio status is available), so it is a plain field, not an option.
//! Inside the block every value is itself optional and null-strips — but Convex
//! validates the nested object's own optionals, so the block is sent whole (the
//! Python loop does not strip nested `None`s). [`RadioBlock`] therefore serializes
//! its `None` fields as JSON `null` deliberately, matching the Python nested
//! dict which keeps the `None` values.

use serde::{Deserialize, Serialize};

/// One service entry in the heartbeat `services[]` array. The cloud loop sources
/// these from `get_services_status()`; the always-present keys are `name`,
/// `status`, `cpuPercent`, `memoryMb`, `uptimeSeconds`, `category`, with `pid`
/// included only when it is a real positive value (Convex rejects `null` for a
/// `v.number()`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEntry {
    pub name: String,
    pub status: String,
    pub cpu_percent: f64,
    pub memory_mb: f64,
    pub uptime_seconds: i64,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
}

/// The `remoteAccess` sub-object. Always present in the payload; both fields are
/// always set (provider string + list of public URLs).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteAccess {
    pub provider: String,
    pub public_urls: Vec<String>,
}

/// One attached-peripheral entry (the SPI LCD today). The `peripherals[]` array
/// is omitted entirely when no peripheral is provisioned, so the model carries
/// the entry shape and the payload holds `Option<Vec<Peripheral>>`. The keys are
/// snake_case in the Python `collect_attached_display` output (it is consumed by
/// the infer-capabilities pipeline, which reads snake_case), so this struct
/// keeps snake_case field names with no rename. `extra` is an open map.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Peripheral {
    pub id: String,
    pub name: String,
    pub category: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub bus: String,
    pub address: String,
    pub rate_hz: i64,
    pub status: String,
    pub last_reading: String,
    pub extra: serde_json::Value,
}

/// The snake_case `radio` link block. Mirrors `build_radio_block` field-for-field
/// (`state`, `iface`, `driver`, `channel`, `freq_mhz`, ...). The Python builder
/// keeps `None` values in this nested dict (only top-level keys are stripped), so
/// these `Option`s serialize as JSON `null` when absent — that is intentional
/// parity. The block is always present in the payload (an all-`absent` block when
/// no radio status is available).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RadioBlock {
    pub state: Option<String>,
    pub iface: Option<String>,
    pub driver: Option<String>,
    pub channel: Option<i64>,
    pub freq_mhz: Option<i64>,
    pub bandwidth_mhz: Option<i64>,
    // tx power is an integer dBm on the source (the wfb manager's effective
    // power is `int | None`); model as i64 so the wire is `20`, not `20.0`.
    pub tx_power_dbm: Option<i64>,
    pub tx_power_max_dbm: Option<i64>,
    pub topology: Option<String>,
    pub rssi_dbm: Option<f64>,
    pub snr_db: Option<f64>,
    pub noise_dbm: Option<f64>,
    pub bitrate_kbps: Option<i64>,
    pub fec_recovered: Option<i64>,
    pub fec_lost: Option<i64>,
    pub packets_lost: Option<i64>,
    pub loss_percent: Option<f64>,
    pub mcs_index: Option<i64>,
    pub rx_silent_seconds: Option<f64>,
    pub paired: bool,
    pub paired_with_device_id: Option<String>,
    pub paired_at: Option<String>,
    pub public_key_fingerprint: Option<String>,
    pub auto_pair_enabled: Option<bool>,
    pub tx_video_stalled: Option<bool>,
    pub tx_video_stall_kills: Option<i64>,
    pub tx_video_recvq_bytes: Option<i64>,
    pub acquire_state: Option<String>,
    pub channel_locked: Option<bool>,
    pub reacquire_kills: Option<i64>,
    pub valid_rx_packets_per_s: Option<f64>,
    pub adapter_chipset: Option<String>,
    pub adapter_injection_ok: bool,
}

impl RadioBlock {
    /// The all-`absent` block the Python builder emits when no radio status is
    /// available (manager absent, RTL not plugged in, ground-station profile).
    /// Mirrors the `if not wfb_status:` branch of `build_radio_block`.
    pub fn absent() -> Self {
        RadioBlock {
            state: Some("absent".to_string()),
            iface: None,
            driver: None,
            channel: None,
            freq_mhz: None,
            bandwidth_mhz: None,
            tx_power_dbm: None,
            tx_power_max_dbm: None,
            topology: None,
            rssi_dbm: None,
            snr_db: None,
            noise_dbm: None,
            bitrate_kbps: None,
            fec_recovered: None,
            fec_lost: None,
            packets_lost: None,
            loss_percent: None,
            mcs_index: None,
            rx_silent_seconds: None,
            paired: false,
            paired_with_device_id: None,
            paired_at: None,
            public_key_fingerprint: None,
            auto_pair_enabled: None,
            tx_video_stalled: None,
            tx_video_stall_kills: None,
            tx_video_recvq_bytes: None,
            acquire_state: None,
            channel_locked: None,
            reacquire_kills: None,
            valid_rx_packets_per_s: None,
            adapter_chipset: None,
            adapter_injection_ok: false,
        }
    }
}

/// The cloud status heartbeat payload.
///
/// Field set + order mirrors the Python cloud loop's `payload` dict. Order does
/// not affect the receiver (it validates by key), but it is kept aligned with the
/// Python source for readability. Required-on-wire fields are plain; everything
/// the Python loop may strip is `Option<T>` + `skip_serializing_if`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatPayload {
    // --- required on wire ---
    pub device_id: String,
    pub version: String,
    // profile/role are optional: a drone-profile heartbeat carries role=None,
    // which the Python loop strips. profile is always set in practice but kept
    // optional to honor the strip rule for any future None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub uptime_seconds: i64,

    // --- board ---
    pub board_name: String,
    pub board_tier: i64,
    pub board_soc: String,
    pub board_arch: String,

    // --- perception ---
    // The NPU capability + the perception tier this node runs on. `npu_tops`
    // comes from the board sidecar; `perception_tier` is the canonical
    // ados_offload::pick_tier decision (not a second impl). The offload target is
    // absent until a workstation is paired (never a fabricated reach, rule 44).
    pub npu_tops: f64,
    pub has_accelerator: bool,
    pub perception_tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perception_offload_target: Option<String>,

    // --- health ---
    // CPU/memory/disk are measured by the Python enrichment producer, not the
    // native loop. Optional + skip so a heartbeat with no fresh enrichment OMITS
    // them (honest "unknown") instead of asserting 0.0 as a live reading
    // (operating rule 37). The producer folds the real values over the base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_percent: Option<f64>,
    // temperature is deleted when None (Convex v.float64() rejects null).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    pub memory_used_mb: i64,
    pub memory_total_mb: i64,
    pub disk_used_gb: f64,
    pub disk_total_gb: f64,
    pub cpu_cores: i64,
    pub board_ram_mb: i64,
    pub cpu_history: Vec<f64>,
    pub memory_history: Vec<f64>,

    // --- FC link ---
    // The FC connection is observed by the enrichment producer (it reads the
    // state-socket snapshot). Optional + skip so absence reads as "unknown" on
    // the GCS rather than the native loop asserting a hard `false` for a drone
    // whose FC is actually up (operating rule 37).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fc_connected: Option<bool>,
    pub fc_port: String,
    pub fc_baud: i64,
    // FC link gated-truth detail. The LAN `/api/status` already carries these; the
    // enrichment producer lifts them from the state snapshot so a cloud-relay drone
    // can render "port open · no MAVLink" + the diagnostic hint, not just a
    // connected boolean. All optional + skip so an older agent (absent fields)
    // reads as honest "unknown" rather than asserting a value (operating rule 37).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_open: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mavlink_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_age_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fc_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fc_link_hint: Option<String>,
    // The FC firmware family from the port's USB descriptor (betaflight/inav), or
    // absent for a MAVLink/unknown FC. Lifted by the enrichment producer from the
    // state snapshot so a cloud-relay GCS can badge "Betaflight (MSP)" instead of a
    // misleading "not connected" (an MSP FC never emits the HEARTBEAT the alive gate
    // needs). Optional + skip so absence reads as honest "unknown" (operating rule 37).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fc_variant: Option<String>,

    // --- services + URLs ---
    // The service list comes from the enrichment producer (the API process owns
    // the tracker + systemd view). Optional + skip so a no-enrichment heartbeat
    // omits it instead of asserting an empty fleet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<ServiceEntry>>,
    pub last_ip: String,
    pub mdns_host: String,
    pub setup_url: String,
    pub api_url: String,
    pub agent_version: String,

    // --- video / mavlink discovery ---
    // The pipeline state is the enrichment producer's to report; absent reads as
    // "unknown" rather than the native loop asserting "stopped" over a live feed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_state: Option<String>,
    pub video_whep_port: i64,
    pub mavlink_ws_port: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mavlink_ws_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_whep_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mission_control_url: Option<String>,

    pub remote_access: RemoteAccess,

    // --- optional auxiliary blocks ---
    // This one root key is snake_case on the wire (the Python loop sets the
    // literal key `last_plugin_update_check_at`), unlike every other camelCase
    // root key — so it carries an explicit rename that overrides the container
    // `rename_all = "camelCase"`.
    #[serde(
        rename = "last_plugin_update_check_at",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_plugin_update_check_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peripherals: Option<Vec<Peripheral>>,

    // --- radio (always present; snake_case sub-block) ---
    pub radio: RadioBlock,
    // Adapter verdict hoisted to the root. chipset null-strips; injectionOk is a
    // plain bool (false when no injection adapter verified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wfb_adapter_chipset: Option<String>,
    pub wfb_adapter_injection_ok: bool,

    // --- LCD / display enrichment (all optional, omitted when absent) ---
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcd_active_page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcd_touch_calibrated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcd_rotation: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcd_snapshot_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcd_last_touch_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcd_last_gesture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_local_decoder_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_local_decoder_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_local_decoder_fps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_recording: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_pipeline_flavor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_encoder_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_encoder_hw_accel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_camera_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_pipeline_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_type: Option<String>,

    // --- FC CAN buses (omitted when no CAN params cached) ---
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_buses: Option<Vec<CanBus>>,

    // --- Compute-node cluster + queue state (compute profile only) ---
    // Folded from the /run/ados/compute-heartbeat.json sidecar written by
    // ados-compute. All `None` on a non-compute node, so they are omitted and
    // the heartbeat stays byte-identical (the frozen golden case has no
    // sidecar). The keys are the cmd_droneStatus compute* field names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_cluster_master_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_queue_depth: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_active_jobs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_workers_idle: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_cluster_aggregate_workers_idle: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_cluster_slaves: Option<Vec<ClusterSlave>>,

    // --- Generic plugin/feature state (any profile) ---
    // A map from plugin id to that plugin's own opaque telemetry slice, ferried
    // verbatim from /run/ados/plugins/<id>-state.json. The core never inspects a
    // slice; each plugin owns its shape. Omitted when no plugin reports state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_state: Option<serde_json::Map<String, serde_json::Value>>,

    // --- Service config faults (any profile) ---
    // Each entry is a service whose on-box config file currently fails to parse
    // (its config-status sidecar carries a non-null error). Folded from the
    // `config-status-<service>.json` sidecars every service publishes at startup.
    // Omitted entirely when every service's config is valid, so a healthy node's
    // wire is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_errors: Option<Vec<ConfigErrorEntry>>,
}

/// One compute slave node's capacity, as folded onto the heartbeat under
/// `computeClusterSlaves`. camelCase keys (the nested struct does not inherit
/// the root rename), matching the cmd_droneStatus shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSlave {
    pub node_id: String,
    pub accelerators: Vec<String>,
    pub workers_idle: i64,
    pub queue_depth: i64,
}

/// One service's live config-parse fault, surfaced from its config-status
/// sidecar onto the heartbeat under `configErrors`. `service` is the sidecar's
/// service label (e.g. `"cloud"`, `"mavlink"`), `error` the exact parser
/// message. Both keys are single words so the casing is identical; the rename is
/// explicit for consistency with the other nested wire structs. Present only for
/// services whose current config is malformed — a valid config publishes a null
/// error and is omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigErrorEntry {
    pub service: String,
    pub error: String,
}

/// One FC CAN bus entry. The block is omitted from the payload when no CAN
/// params are cached; each present entry carries `port`, `driver`, `bitrate`,
/// `protocol` (camelCase via the root rename does not apply to a nested struct,
/// but these keys are single-word so casing is identical either way; the Python
/// emit uses these exact snake-equal-to-camel names).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CanBus {
    pub port: i64,
    pub driver: i64,
    pub bitrate: i64,
    pub protocol: i64,
}

impl HeartbeatPayload {
    /// Serialize to a `serde_json::Value`. The relay POSTs the JSON body; the
    /// `skip_serializing_if` rules have already stripped the `None` keys, so the
    /// value carries exactly the keys the Python loop would after its strip step.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("heartbeat payload serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_keys_are_camel_case_and_radio_is_snake_case() {
        let mut p = minimal_payload();
        p.radio = RadioBlock {
            state: Some("connected".to_string()),
            channel: Some(149),
            freq_mhz: Some(5745),
            ..RadioBlock::absent()
        };
        let v = p.to_value();
        let obj = v.as_object().unwrap();
        // Root keys: camelCase.
        assert!(obj.contains_key("deviceId"));
        assert!(obj.contains_key("uptimeSeconds"));
        assert!(obj.contains_key("boardName"));
        assert!(obj.contains_key("wfbAdapterInjectionOk"));
        // No snake_case leak at the root.
        assert!(!obj.contains_key("device_id"));
        assert!(!obj.contains_key("uptime_seconds"));
        // The radio sub-block keys are snake_case.
        let radio = obj.get("radio").unwrap().as_object().unwrap();
        assert!(radio.contains_key("freq_mhz"));
        assert!(radio.contains_key("rssi_dbm"));
        assert!(radio.contains_key("adapter_injection_ok"));
        assert!(!radio.contains_key("freqMhz"));
    }

    #[test]
    fn none_root_fields_are_omitted_not_null() {
        // temperature None must be absent, not JSON null (Convex v.optional).
        let p = minimal_payload();
        let v = p.to_value();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("temperature"));
        assert!(!obj.contains_key("missionControlUrl"));
        assert!(!obj.contains_key("peripherals"));
        // No top-level key serializes as explicit null.
        for (k, val) in obj.iter() {
            assert!(!val.is_null(), "root key {k} must not be JSON null");
        }
    }

    #[test]
    fn present_temperature_serializes() {
        let mut p = minimal_payload();
        p.temperature = Some(48.5);
        let v = p.to_value();
        assert_eq!(v["temperature"], serde_json::json!(48.5));
    }

    fn minimal_payload() -> HeartbeatPayload {
        HeartbeatPayload {
            device_id: "ados-test".to_string(),
            version: "0.1.0".to_string(),
            profile: Some("drone".to_string()),
            role: None,
            uptime_seconds: 10,
            board_name: "unknown".to_string(),
            board_tier: 0,
            board_soc: String::new(),
            board_arch: String::new(),
            npu_tops: 0.0,
            has_accelerator: false,
            perception_tier: "none".to_string(),
            perception_offload_target: None,
            cpu_percent: Some(0.0),
            memory_percent: Some(0.0),
            disk_percent: Some(0.0),
            temperature: None,
            memory_used_mb: 0,
            memory_total_mb: 0,
            disk_used_gb: 0.0,
            disk_total_gb: 0.0,
            cpu_cores: 0,
            board_ram_mb: 0,
            cpu_history: vec![],
            memory_history: vec![],
            fc_connected: Some(false),
            fc_port: String::new(),
            fc_baud: 0,
            transport_open: None,
            mavlink_alive: None,
            heartbeat_age_s: None,
            fc_source: None,
            fc_link_hint: None,
            fc_variant: None,
            services: Some(vec![]),
            last_ip: "127.0.0.1".to_string(),
            mdns_host: String::new(),
            setup_url: "http://127.0.0.1:8080".to_string(),
            api_url: "http://127.0.0.1:8080/api".to_string(),
            agent_version: "0.1.0".to_string(),
            video_state: Some("stopped".to_string()),
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
            compute_role: None,
            compute_cluster_master_id: None,
            compute_queue_depth: None,
            compute_active_jobs: None,
            compute_workers_idle: None,
            compute_cluster_aggregate_workers_idle: None,
            compute_cluster_slaves: None,
            plugin_state: None,
            config_errors: None,
        }
    }
}
