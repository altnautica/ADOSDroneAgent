//! Frozen wire model for the cloud status heartbeat.
//!
//! The cloud relay POSTs a status payload to `{convex}/agent/status` on a 5 s
//! cadence; Mission Control reads it to populate the per-drone view. This module
//! is that payload's wire shape. It was frozen byte-identically against the
//! Python emitter so the relay could move to Rust without the receiver
//! noticing; that emitter has since been deleted, so what the shape is now
//! answerable to is the RECEIVER -- the Convex `pushStatus` validator and the
//! golden fixture in `tests/heartbeat_golden.rs`. A field that stops matching
//! the validator is rejected for the whole payload, and the node goes dark
//! every tick, so the three rules below are load-bearing rather than stylistic.
//!
//! Three wire rules are load-bearing and are encoded in the types here:
//!
//! 1. **Casing.** Every root key is camelCase EXCEPT the `radio` sub-block,
//!    whose keys are snake_case, as the golden fixture pins them. The root struct carries `#[serde(rename_all =
//!    "camelCase")]`; [`RadioBlock`] uses its own field names (already
//!    snake_case) with no rename.
//! 2. **Null-stripping, with a declared exception.** Most of the receiver's
//!    columns are `v.optional(T)`, which accepts "field absent OR T" but
//!    rejects an explicit JSON `null`, so those absent top-level keys must be
//!    omitted. Such fields are `Option<T>` with
//!    `#[serde(skip_serializing_if = "Option::is_none")]`, so a `None` is
//!    omitted, never serialized as `null`.
//!
//!    A SMALL declared set is instead `v.optional(v.union(T, v.null()))` on the
//!    receiver, which preserves an explicit null as a value distinct from
//!    absent. Those fields deliberately carry NO `skip_serializing_if`, because
//!    for them omission is a different (and false) statement: "this agent does
//!    not report this at all", indistinguishable from an agent that predates the
//!    surface, and it leaves the receiver nothing to overwrite a previous tick's
//!    reading with. `null` says "reported, not measured" and clears the column.
//!    `crate::loops::heartbeat::UNMEASURED_NULL_KEYS` is the authoritative list
//!    and the null-strip step honours it.
//! 3. **Required-on-wire.** `deviceId`, `version`, and `uptimeSeconds` are
//!    always present, so they are plain (non-`Option`) fields.
//!
//! The `radio` block is always emitted (an `absent` block
//! when no radio status is available), so it is a plain field, not an option.
//! Inside the block every value is itself optional and null-strips — but Convex
//! validates the nested object's own optionals, so the block is sent whole and
//! nested `None`s are NOT stripped. [`RadioBlock`] therefore serializes its
//! `None` fields as JSON `null` deliberately -- the opposite of the top-level
//! rule, and load-bearing in the same way.

use serde::{Deserialize, Serialize};

mod blocks;
pub use blocks::{CrsfBlock, RadioBlock};

/// One service entry in the heartbeat `services[]` array. `name` and `status`
/// are always present; the per-unit accounting (`cpuPercent`, `memoryMb`,
/// `uptimeSeconds`) and `pid` are included only when a producer measured them —
/// the receiver declares each optional, and a fabricated `0` reads as a real
/// "0 MB, up 0 s".
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEntry {
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<i64>,
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

/// The cloud status heartbeat payload.
///
/// Field set + order mirrors the Python cloud loop's `payload` dict. Order does
/// not affect the receiver (it validates by key), but it is kept aligned with the
/// Python source for readability. Required-on-wire fields are plain; everything
/// the strip step may remove is `Option<T>` + `skip_serializing_if`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatPayload {
    // --- required on wire ---
    pub device_id: String,
    pub version: String,
    // profile/role are optional: a drone-profile heartbeat carries role=None,
    // which the strip step removes. profile is always set in practice but kept
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
    // absent until a workstation is paired (never a fabricated reach).
    pub npu_tops: f64,
    pub has_accelerator: bool,
    pub perception_tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perception_offload_target: Option<String>,

    // --- health ---
    // CPU/memory/disk are measured by the enrichment loop, not the
    // native loop. Optional + skip so a heartbeat with no fresh enrichment OMITS
    // them (honest "unknown") instead of asserting 0.0 as a live reading.
    // The producer folds the real values over the base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    // Absolute resource values, host facts and the history series: all
    // measured by the enrichment producer and optional on the receiver, so an
    // unmeasured one is OMITTED rather than asserted as 0 / [] (a node that
    // reads "0 cores, 0 MB RAM" is lying, not unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used_mb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_total_mb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_used_gb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_total_gb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board_ram_mb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_history: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_history: Option<Vec<f64>>,

    // --- FC link ---
    // The FC connection is observed by the enrichment producer (it reads the
    // state-socket snapshot). Optional + skip so absence reads as "unknown" on
    // the GCS rather than the native loop asserting a hard `false` for a drone
    // whose FC is actually up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fc_connected: Option<bool>,
    // The serial port and baud of the FC link, lifted by the enrichment producer
    // when the router reports a real one; omitted otherwise (never `""` / 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fc_port: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fc_baud: Option<i64>,
    // FC link gated-truth detail. The LAN `/api/status` already carries these; the
    // enrichment producer lifts them from the state snapshot so a cloud-relay drone
    // can render "port open · no MAVLink" + the diagnostic hint, not just a
    // connected boolean. All optional + skip so an older agent (absent fields)
    // reads as honest "unknown" rather than asserting a value.
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
    // needs). Optional + skip so absence reads as honest "unknown".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fc_variant: Option<String>,

    // --- services + URLs ---
    // The service list comes from the enrichment producer (the API process owns
    // the tracker + systemd view). Optional + skip so a no-enrichment heartbeat
    // omits it instead of asserting an empty fleet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<ServiceEntry>>,
    // The node's advertised LAN reach. OPTIONAL + skip: a producer that has not
    // PROVEN a reach must omit these, never send `""`. An empty string is not
    // "unknown" on the receiver — `stringField` type-checks it through and the
    // column is overwritten, so a previously-good `lastIp`/`setupUrl` is
    // destroyed and the GCS builds `http://:8080`. Only advertise a reach name
    // proven to resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdns_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    pub agent_version: String,

    // --- video / mavlink discovery ---
    // The pipeline state is the enrichment producer's to report; absent reads as
    // "unknown" rather than the native loop asserting "stopped" over a live feed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_state: Option<String>,
    // The media and MAVLink WebSocket ports: omitted until a producer reports
    // a port it actually serves on, never a fabricated 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_whep_port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mavlink_ws_port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mavlink_ws_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_whep_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mission_control_url: Option<String>,

    pub remote_access: RemoteAccess,

    // --- optional auxiliary blocks ---
    // This one root key is snake_case on the wire (the contract sets the
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
    // --- CRSF RC lane (optional; snake_case sub-block, omitted when the lane
    // service is not running or its sidecar is stale) ---
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crsf: Option<CrsfBlock>,
    // Adapter verdict hoisted to the root. Both halves are NULL-TOLERANT on the
    // receiver (`v.optional(v.union(v.boolean(), v.null()))`), which is what
    // makes an explicit null meaningful here: it says "this agent reports the
    // verdict and currently has none", and it CLEARS the column. Omitting them
    // instead said "this agent does not report the verdict at all", which is
    // false, and on the receiving side an omitted key is indistinguishable from
    // an agent that predates the surface. So no `skip_serializing_if`: a `None`
    // rides as JSON null. `Some(false)` is still the MEASURED
    // scanned-and-found-none verdict, distinct from both.
    pub wfb_adapter_chipset: Option<String>,
    pub wfb_adapter_injection_ok: Option<bool>,
    /// Per-adapter stable-MAC verdicts, `{adapters: [...]}` from
    /// `/etc/ados/mac-pins.state`. Omitted on a node that has pinned nothing —
    /// the GCS clamp hides its card unless `adapters` is an array, and an empty
    /// one would claim the node looked and found no adapter.
    ///
    /// Already camelCase at the source, so the Convex relay's snake→camel remap
    /// must NOT be applied to it. Carried as a `Value` because the block is a
    /// pass-through the GCS validates, not a shape this crate owns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac_stability: Option<serde_json::Value>,

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
    /// Per-leg video streams a multi-stream node serves, folded from the
    /// /run/ados/video-streams.json sidecar. Absent on a single-stream node so
    /// the heartbeat stays byte-identical.
    ///
    /// Each entry carries the SAME-ORIGIN relative `whepUrl` the node actually
    /// serves, so the GCS resolves it against the node's own reachable base URL
    /// and the media plane keeps going through the agent's :8080 proxy (and its
    /// `ados_session` media-auth gate). Never an absolute `host:port`: this
    /// contract used to instruct the GCS to build `:8889/<id>/whep` against the
    /// node host — a name the agent does not proxy, which an HTTPS ground
    /// station rejects as mixed content, which cannot resolve off-LAN, and which
    /// nothing in the agent proves reachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_streams: Option<Vec<VideoStreamHb>>,
    /// The WFB peers a ground station relays (the drones a GCS paired only to
    /// this ground node transitively enrols), folded from the
    /// /run/ados/linked-peers.json sidecar. Absent on a drone / a peerless
    /// ground node so the heartbeat stays byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_peers: Option<Vec<LinkedPeerHb>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_type: Option<String>,

    // --- FC CAN buses (omitted when no CAN params cached) ---
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_buses: Option<Vec<CanBus>>,

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

/// One advertised video leg on the heartbeat: its stable id (the mediamtx path
/// and WHEP id), a logical role (eo, eo_wide, ir), the codec, and the
/// same-origin relative URL the node serves that leg at.
///
/// Keys are single words, so the casing is identical either way, except
/// `whep_url` → `whepUrl`, which is renamed explicitly to match the
/// `RosterCamera.whep_url` shape the cockpit already consumes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoStreamHb {
    pub id: String,
    pub role: String,
    pub codec: String,
    /// Per-leg liveness, deserialized straight from the sidecar's `live`:
    /// `Some(true)` the path is receiving, `Some(false)` a publisher leg is flat
    /// (degraded), absent = not sampled / an idle on-demand leg (unknown, not
    /// dead — the GCS keeps it selectable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
    /// The leg's WHEP endpoint as a SAME-ORIGIN relative path
    /// (`/whep?camera=<id>`), which is the form the agent's media proxy really
    /// serves. Derived when the sidecar is folded, never read from it, so a
    /// stale sidecar cannot pin an old URL shape.
    ///
    /// Deliberately no `hlsUrl` companion: the only consumer of this payload is
    /// the cloud GCS, which has no HLS player, and advertising a reach nothing
    /// can use is the same defect as the absolute port this field replaced. The
    /// agent's own on-box cockpit gets `hls_url` from `/api/status/full`
    /// instead.
    #[serde(rename = "whepUrl", default, skip_serializing_if = "Option::is_none")]
    pub whep_url: Option<String>,
}

/// One WFB peer a ground station reports it relays, folded from the
/// `/run/ados/linked-peers.json` sidecar the receive-side presence listener
/// writes. A ground station can relay more than one drone, so the payload
/// carries a LIST; each entry is exactly what the PresenceBeacon + the listener
/// decode (device-id, role, channel, RSSI, last-seen), no richer identity than
/// the wire itself provides. `rename_all = "camelCase"` maps the snake fields to
/// the `deviceId`/`rssiDbm`/`seenAtUnix` wire keys the GCS `LinkedPeer` consumes
/// and the strict cloud validator declares. Absent on a drone / a peerless
/// ground node so the heartbeat stays byte-identical.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedPeerHb {
    pub device_id: String,
    pub role: String,
    pub channel: u8,
    pub rssi_dbm: i8,
    /// Wall-clock unix SECONDS of the last decoded beacon (the relay freshness
    /// signal; the GCS multiplies by 1000 for a ms timestamp).
    pub seen_at_unix: f64,
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
    /// value carries exactly the keys the contract requires after its strip step.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("heartbeat payload serializes")
    }
}

#[cfg(test)]
mod tests;
