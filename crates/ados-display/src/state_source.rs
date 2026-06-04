//! Live device-state reader that builds the page [`PageContext`].
//!
//! The native-resolution page UI in [`crate::pages`] reads from a single
//! [`PageContext`]; this module is the seam that fills it from the running
//! agent, mirroring the same sources the Python LCD service polled:
//!
//! * The agent's local REST API on `127.0.0.1:8080` — the ground-station
//!   status snapshot (`/api/v1/ground-station/status`, the union of link /
//!   network / system / role / mesh / video the dashboard reads) and the setup
//!   wizard state (`/api/v1/setup/status`, completion + next step + device
//!   identity). Authentication is the `X-ADOS-Key` header read from
//!   `/etc/ados/pairing.json`; the pairing routes stay reachable while
//!   unpaired, and an empty key is correct for an unpaired box.
//! * The `/run/ados` JSON sidecars the channel-hops and link-stats surfaces
//!   read directly because the data lives cross-process (the radio service
//!   owns it): `hop-supervisor.json` (band + hop history) and `health.json`
//!   (cpu / memory / disk / temperature).
//!
//! The history buffers the sparkline surfaces read (RSSI, CPU, temperature,
//! battery) are not carried in any one snapshot; the source keeps a rolling
//! 60-sample ring per series and pushes the freshest reading each refresh, the
//! same way the Python screen objects accumulated trend points across ticks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::pages::{
    CloudCtx, DeviceCtx, DroneCtx, FcCtx, HardwareItem, HealthCtx, HopEntry, HoppingCtx, LinkCtx,
    MeshCtx, MeshPeer, NetworkCtx, PageContext, PairedDroneCtx, PairingCtx, RadioCtx, RoleCtx,
    SystemCtx, UplinkCtx, VideoCtx, WifiClientCtx,
};

/// The agent's local HTTP API base. Matches the Python LCD service's
/// `http://{api_host}:{api_port}` default (`127.0.0.1:8080`).
pub const DEFAULT_API_BASE: &str = "http://127.0.0.1:8080";

/// Pairing-key file. The agent writes the persisted `X-ADOS-Key` here; the LCD
/// process reads it so its status polls authenticate against a paired agent
/// instead of getting 401'd (which would leave the panel rendering blanks).
pub const PAIRING_JSON_PATH: &str = "/etc/ados/pairing.json";

/// Device-id file (`/etc/ados/device-id`, hyphen) — the canonical id the about
/// and diagnostics surfaces show when the setup snapshot doesn't carry one.
pub const DEVICE_ID_PATH: &str = "/etc/ados/device-id";

/// Build-stamp breadcrumb (`/etc/ados/build.txt`) for the about surface.
pub const BUILD_STAMP_PATH: &str = "/etc/ados/build.txt";

/// Per-request timeout for the local status polls. Matches the Python LCD
/// service's `httpx` 0.9 s ceiling so a wedged agent never stalls the panel.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(900);

/// Rolling trend-buffer length for the sparkline surfaces (RSSI, CPU, temp,
/// battery). The pages render the last 60 samples; the source keeps exactly
/// that many, oldest first.
const HISTORY_LEN: usize = 60;

/// `/run/ados` sidecar paths the surfaces read directly.
fn hop_supervisor_path() -> PathBuf {
    PathBuf::from("/run/ados/hop-supervisor.json")
}
fn health_path() -> PathBuf {
    PathBuf::from("/run/ados/health.json")
}

/// A fixed-length rolling history of optional samples, oldest first. A `None`
/// push marks a gap so the sparkline can break the line, matching how the
/// Python trend buffers stored a sentinel for a missing reading.
#[derive(Debug, Clone, Default)]
struct History {
    samples: Vec<Option<f64>>,
}

impl History {
    fn push(&mut self, value: Option<f64>) {
        self.samples.push(value);
        if self.samples.len() > HISTORY_LEN {
            let overflow = self.samples.len() - HISTORY_LEN;
            self.samples.drain(..overflow);
        }
    }

    fn to_vec(&self) -> Vec<Option<f64>> {
        self.samples.clone()
    }
}

/// Reads the live agent state and composes a [`PageContext`] each refresh.
///
/// One instance is owned by the render mode in the daemon. It holds the HTTP
/// agent (reused connection pool), the resolved api-key, the resolved hostname,
/// and the rolling trend buffers that persist across refreshes.
pub struct StateSource {
    base: String,
    api_key: Option<String>,
    agent: ureq::Agent,
    hostname: String,
    hop_path: PathBuf,
    health_path: PathBuf,
    rssi_history: History,
    cpu_history: History,
    temp_history: History,
    battery_history: History,
}

impl StateSource {
    /// Build a source against the default local agent, resolving the api-key
    /// from `/etc/ados/pairing.json` and the hostname from the OS.
    pub fn new() -> Self {
        Self::with_paths(
            DEFAULT_API_BASE,
            Path::new(PAIRING_JSON_PATH),
            hop_supervisor_path(),
            health_path(),
        )
    }

    /// Build a source with explicit base URL and sidecar paths (used by tests
    /// so the polls and reads round-trip without touching `/run` or `/etc`).
    pub fn with_paths(
        base: impl Into<String>,
        pairing_json: &Path,
        hop_path: PathBuf,
        health_path: PathBuf,
    ) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            api_key: load_api_key(pairing_json),
            agent,
            hostname: read_hostname(),
            hop_path,
            health_path,
            rssi_history: History::default(),
            cpu_history: History::default(),
            temp_history: History::default(),
            battery_history: History::default(),
        }
    }

    /// GET `path` on the agent's local API and parse the JSON body. Returns
    /// `None` on any transport / status / decode error so the panel keeps the
    /// last good frame instead of going blank while the agent restarts.
    fn get_json(&self, path: &str) -> Option<Value> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.get(&url);
        if let Some(ref key) = self.api_key {
            req = req.set("X-ADOS-Key", key);
        }
        match req.call() {
            Ok(resp) => resp.into_json::<Value>().ok(),
            Err(_) => None,
        }
    }

    /// Refresh every source and build the full [`PageContext`] for this tick.
    ///
    /// The status snapshot and the setup snapshot come from the agent's local
    /// API; the hop-supervisor + health sidecars are read from disk. Each
    /// source is independent — a missing one degrades only the surfaces that
    /// read it, never the whole frame.
    pub fn build_context(&mut self) -> PageContext {
        let status = self.get_json("/api/v1/ground-station/status");
        let setup = self.get_json("/api/v1/setup/status");
        let hop = read_run_json(&self.hop_path);
        let health = read_run_json(&self.health_path);
        self.compose(
            status.as_ref(),
            setup.as_ref(),
            hop.as_ref(),
            health.as_ref(),
        )
    }

    /// Map the four already-fetched JSON sources into a [`PageContext`],
    /// advancing the rolling history buffers. Split out from
    /// [`StateSource::build_context`] so the mapping is unit-testable without a
    /// live agent.
    pub fn compose(
        &mut self,
        status: Option<&Value>,
        setup: Option<&Value>,
        hop: Option<&Value>,
        health: Option<&Value>,
    ) -> PageContext {
        let mut ctx = PageContext {
            hostname: self.hostname.clone(),
            clock: current_clock(),
            ..PageContext::default()
        };

        if let Some(s) = status {
            ctx.link = link_ctx(get(s, "link"));
            ctx.radio = radio_ctx(get(s, "radio"));
            ctx.drone = drone_ctx(get(s, "drone").or_else(|| get(s, "paired_drone")));
            ctx.paired_drone = paired_drone_ctx(get(s, "paired_drone"));
            ctx.fc = fc_ctx(get(s, "fc"));
            ctx.cloud = cloud_ctx(get(s, "cloud"));
            ctx.pairing = pairing_ctx(get(s, "pairing"));
            ctx.role = role_ctx(get(s, "role"));
            ctx.mesh = mesh_ctx(get(s, "mesh"));
            ctx.network = network_ctx(get(s, "network"));
            ctx.uplink = uplink_ctx(get(s, "uplink").or_else(|| get(s, "modem")));
            ctx.system = system_ctx(get(s, "system"));
            ctx.video = video_ctx(get(s, "video"));
            ctx.hardware_check = hardware_items(get(s, "hardware_check"));
        }

        if let Some(su) = setup {
            apply_setup(&mut ctx, su);
        }

        ctx.hopping = hopping_ctx(hop, ctx.link.channel);
        ctx.health = health_ctx(health);
        ctx.device = self.device_ctx(setup, &ctx.system);

        // Advance the rolling trend buffers from this tick's fresh readings.
        // Each series breaks its line on a missing reading (None push).
        self.rssi_history.push(ctx.link.rssi_dbm);
        self.cpu_history.push(ctx.system.cpu_pct);
        self.temp_history.push(ctx.system.temp_c);
        self.battery_history.push(ctx.fc.battery_remaining);

        ctx.link.rssi_history = self.rssi_history.to_vec();
        ctx.system.cpu_history = self.cpu_history.to_vec();
        ctx.system.temp_history = self.temp_history.to_vec();
        ctx.fc.battery_history = self.battery_history.to_vec();

        ctx
    }

    /// Compose device identity from the setup snapshot, the system block, and
    /// the on-disk device-id / build-stamp breadcrumbs.
    fn device_ctx(&self, setup: Option<&Value>, system: &SystemCtx) -> DeviceCtx {
        let mut device = DeviceCtx::default();
        if let Some(su) = setup {
            device.device_id = string_field(su, "device_id");
            device.device_name =
                string_field(su, "device_name").or_else(|| string_field(su, "name"));
            device.version = string_field(su, "version");
            device.board_name =
                string_field(su, "board_name").or_else(|| string_field(su, "board"));
            device.primary_ip = string_field(su, "primary_ip");
            device.primary_mac = string_field(su, "primary_mac");
            device.mac_eth0 = string_field(su, "mac_eth0");
            device.mac_wlan0 = string_field(su, "mac_wlan0");
        }
        if device.device_id.is_none() {
            let id = read_trimmed(Path::new(DEVICE_ID_PATH));
            if !id.is_empty() {
                device.device_id = Some(id);
            }
        }
        if device.version.is_none() {
            device.version = system.agent_version.clone();
        }
        let stamp = read_trimmed(Path::new(BUILD_STAMP_PATH));
        if !stamp.is_empty() {
            device.build_stamp = Some(stamp);
        }
        device
    }
}

impl Default for StateSource {
    fn default() -> Self {
        Self::new()
    }
}

// ── JSON helpers ────────────────────────────────────────────────────

/// Borrow a child object/value by key, when present.
fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key)
}

/// A string field on `v`, when it is present and a JSON string.
fn string_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// A float field on `v`, accepting integer or float JSON numbers.
fn f64_field(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// An integer field on `v`. Truncates a float number toward zero.
fn i64_field(v: &Value, key: &str) -> Option<i64> {
    let n = v.get(key)?;
    n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))
}

/// A boolean field on `v`, defaulting to `false` when absent or non-bool.
fn bool_field(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

// ── per-context mappers ─────────────────────────────────────────────

fn link_ctx(v: Option<&Value>) -> LinkCtx {
    let Some(v) = v else {
        return LinkCtx::default();
    };
    LinkCtx {
        state: string_field(v, "state"),
        rssi_dbm: f64_field(v, "rssi_dbm"),
        snr_db: f64_field(v, "snr_db"),
        noise_dbm: f64_field(v, "noise_dbm"),
        loss_percent: f64_field(v, "loss_percent"),
        bitrate_mbps: f64_field(v, "bitrate_mbps"),
        bitrate_kbps: f64_field(v, "bitrate_kbps"),
        // The producer key is `fec_failed`; the view also mirrors it as
        // `fec_lost`. Read either so both producer + view shapes resolve.
        fec_recovered: i64_field(v, "fec_recovered"),
        fec_lost: i64_field(v, "fec_lost").or_else(|| i64_field(v, "fec_failed")),
        channel: i64_field(v, "channel"),
        frequency_mhz: i64_field(v, "frequency_mhz"),
        bandwidth_mhz: i64_field(v, "bandwidth_mhz"),
        tx_power_dbm: i64_field(v, "tx_power_dbm"),
        mcs_index: i64_field(v, "mcs_index"),
        fec_k: i64_field(v, "fec_k"),
        fec_n: i64_field(v, "fec_n"),
        // Option<bool> (not the defaulted bool_field) so the page can tell a
        // reported `false` from an older agent that omits the field.
        adaptive_bitrate_enabled: v.get("adaptive_bitrate_enabled").and_then(Value::as_bool),
        recommended_tier_name: string_field(v, "recommended_tier_name"),
        packets_received: i64_field(v, "packets_received"),
        packets_lost: i64_field(v, "packets_lost"),
        rssi_history: Vec::new(),
    }
}

fn radio_ctx(v: Option<&Value>) -> RadioCtx {
    // Mirror `_normalize_radio_fields`: default the topology to `host_vbus`
    // when the block (or the field) is absent, so the brownout badge has a
    // stable source on agents that predate the WFB-status REST exposure.
    let topology = v
        .and_then(|v| string_field(v, "topology"))
        .or_else(|| Some("host_vbus".to_string()));
    RadioCtx { topology }
}

fn drone_ctx(v: Option<&Value>) -> DroneCtx {
    let Some(v) = v else {
        return DroneCtx::default();
    };
    DroneCtx {
        device_id: string_field(v, "device_id"),
        fc_mode: string_field(v, "fc_mode"),
        battery_pct: f64_field(v, "battery_pct"),
        gps_sats: i64_field(v, "gps_sats"),
        armed: v.get("armed").and_then(Value::as_bool),
        key_fingerprint: string_field(v, "key_fingerprint"),
    }
}

fn paired_drone_ctx(v: Option<&Value>) -> PairedDroneCtx {
    let Some(v) = v else {
        return PairedDroneCtx::default();
    };
    PairedDroneCtx {
        device_id: string_field(v, "device_id"),
        key_fingerprint: string_field(v, "key_fingerprint"),
        paired_at_seconds: f64_field(v, "paired_at_seconds"),
        paired_at: f64_field(v, "paired_at"),
    }
}

fn fc_ctx(v: Option<&Value>) -> FcCtx {
    let Some(v) = v else {
        return FcCtx::default();
    };
    FcCtx {
        vehicle: string_field(v, "vehicle"),
        mode: string_field(v, "mode"),
        armed: bool_field(v, "armed"),
        battery_voltage: f64_field(v, "battery_voltage"),
        battery_remaining: f64_field(v, "battery_remaining"),
        gps_fix_type: i64_field(v, "gps_fix_type"),
        gps_satellites_visible: i64_field(v, "gps_satellites_visible"),
        battery_history: Vec::new(),
    }
}

fn cloud_ctx(v: Option<&Value>) -> CloudCtx {
    let Some(v) = v else {
        return CloudCtx::default();
    };
    CloudCtx {
        paired: bool_field(v, "paired"),
        pair_code: string_field(v, "pair_code"),
        pairing_code: string_field(v, "pairing_code"),
        latency_ms: f64_field(v, "latency_ms"),
        rtt_ms: f64_field(v, "rtt_ms"),
        broadcasting: bool_field(v, "broadcasting"),
        pair_url: string_field(v, "pair_url"),
        mqtt_state: string_field(v, "mqtt_state"),
        http_state: string_field(v, "http_state"),
        drone_id: string_field(v, "drone_id"),
    }
}

fn pairing_ctx(v: Option<&Value>) -> PairingCtx {
    let Some(v) = v else {
        return PairingCtx::default();
    };
    PairingCtx {
        code: string_field(v, "code"),
        pair_url: string_field(v, "pair_url"),
        window_active: bool_field(v, "window_active"),
        window_remaining_seconds: f64_field(v, "window_remaining_seconds"),
    }
}

fn role_ctx(v: Option<&Value>) -> RoleCtx {
    let Some(v) = v else {
        return RoleCtx::default();
    };
    RoleCtx {
        current: string_field(v, "current"),
        configured: string_field(v, "configured"),
        mesh_capable: bool_field(v, "mesh_capable"),
    }
}

fn mesh_ctx(v: Option<&Value>) -> MeshCtx {
    let Some(v) = v else {
        return MeshCtx::default();
    };
    let peers = v
        .get("peers")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(mesh_peer).collect())
        .unwrap_or_default();
    MeshCtx {
        up: bool_field(v, "up"),
        partition: bool_field(v, "partition"),
        peer_count: i64_field(v, "peer_count").unwrap_or(0),
        selected_gateway: string_field(v, "selected_gateway"),
        mesh_id: string_field(v, "mesh_id"),
        peers,
    }
}

fn mesh_peer(v: &Value) -> MeshPeer {
    MeshPeer {
        device_id: string_field(v, "device_id"),
        role: string_field(v, "role"),
        last_seen_seconds_ago: f64_field(v, "last_seen_seconds_ago"),
    }
}

fn network_ctx(v: Option<&Value>) -> NetworkCtx {
    let Some(v) = v else {
        return NetworkCtx::default();
    };
    let wifi_client = v
        .get("wifi_client")
        .map(|w| WifiClientCtx {
            connected: bool_field(w, "connected"),
            ssid: string_field(w, "ssid"),
            signal_dbm: f64_field(w, "signal_dbm").or_else(|| f64_field(w, "signal")),
        })
        .unwrap_or_default();
    NetworkCtx {
        ap_ssid: string_field(v, "ap_ssid"),
        ap_ip: string_field(v, "ap_ip"),
        usb_ip: string_field(v, "usb_ip"),
        uplink_type: string_field(v, "uplink_type"),
        uplink_reachable: bool_field(v, "uplink_reachable"),
        mdns_host: string_field(v, "mdns_host"),
        hotspot_ssid: string_field(v, "hotspot_ssid"),
        hotspot_enabled: bool_field(v, "hotspot_enabled"),
        wifi_client,
    }
}

fn uplink_ctx(v: Option<&Value>) -> UplinkCtx {
    let Some(v) = v else {
        return UplinkCtx::default();
    };
    // The status modem block reports presence via `connected` / `enabled`;
    // accept an explicit `modem_present` first, then fall back to those.
    let modem_present = v
        .get("modem_present")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| bool_field(v, "connected") || bool_field(v, "enabled"));
    UplinkCtx {
        modem_present,
        rsrp_dbm: f64_field(v, "rsrp_dbm"),
        rsrq_db: f64_field(v, "rsrq_db"),
        sinr_db: f64_field(v, "sinr_db"),
        band: string_field(v, "band"),
        ip: string_field(v, "ip"),
        tech: string_field(v, "tech").or_else(|| string_field(v, "technology")),
        reason: string_field(v, "reason"),
    }
}

fn system_ctx(v: Option<&Value>) -> SystemCtx {
    let Some(v) = v else {
        return SystemCtx::default();
    };
    SystemCtx {
        cpu_pct: f64_field(v, "cpu_pct"),
        ram_used_mb: f64_field(v, "ram_used_mb"),
        ram_total_mb: f64_field(v, "ram_total_mb"),
        temp_c: f64_field(v, "temp_c"),
        uptime_seconds: f64_field(v, "uptime_seconds"),
        agent_version: string_field(v, "agent_version"),
        cpu_history: Vec::new(),
        temp_history: Vec::new(),
    }
}

fn video_ctx(v: Option<&Value>) -> VideoCtx {
    let Some(v) = v else {
        return VideoCtx::default();
    };
    VideoCtx {
        decoder: string_field(v, "decoder"),
        active: bool_field(v, "active"),
        recording: bool_field(v, "recording"),
        fps: f64_field(v, "fps"),
        latency_ms: f64_field(v, "latency_ms"),
        bitrate_kbps: f64_field(v, "bitrate_kbps"),
        mediamtx_ready: bool_field(v, "mediamtx_ready"),
        mediamtx_inbound_kbps: f64_field(v, "mediamtx_inbound_kbps"),
        camera_label: string_field(v, "camera_label"),
        camera_count: i64_field(v, "camera_count").unwrap_or(0),
    }
}

fn hardware_items(v: Option<&Value>) -> Vec<HardwareItem> {
    let Some(v) = v else {
        return Vec::new();
    };
    // The block may be a bare array or an object carrying an `items` array.
    let arr = v
        .as_array()
        .or_else(|| v.get("items").and_then(Value::as_array));
    let Some(arr) = arr else {
        return Vec::new();
    };
    arr.iter()
        .map(|item| HardwareItem {
            id: string_field(item, "id"),
            label: string_field(item, "label"),
            state: string_field(item, "state"),
            fix_hint: string_field(item, "fix_hint"),
        })
        .collect()
}

/// Apply the setup-status snapshot onto the context: completion percent, the
/// next-step copy, the wizard-finalized flag, and the local pair code the
/// dashboard shows before a cloud relay binds one.
fn apply_setup(ctx: &mut PageContext, su: &Value) {
    ctx.setup_finalized = bool_field(su, "finalized")
        || bool_field(su, "setup_complete")
        || bool_field(su, "complete");
    ctx.completion_percent =
        f64_field(su, "completion_percent").or_else(|| f64_field(su, "percent"));
    ctx.next_action = string_field(su, "next_action").or_else(|| string_field(su, "next_step"));

    // A local pair code carried on the setup snapshot seeds the pairing +
    // cloud code surfaces when the status block didn't already populate them.
    if let Some(code) = string_field(su, "pairing_code").or_else(|| string_field(su, "pair_code")) {
        if ctx.pairing.code.is_none() {
            ctx.pairing.code = Some(code.clone());
        }
        if ctx.cloud.pair_code.is_none() {
            ctx.cloud.pair_code = Some(code);
        }
    }
    if !ctx.cloud.paired {
        ctx.cloud.paired = bool_field(su, "paired");
    }
}

fn health_ctx(v: Option<&Value>) -> HealthCtx {
    let Some(v) = v else {
        return HealthCtx::default();
    };
    HealthCtx {
        cpu_percent: f64_field(v, "cpu_percent"),
        memory_percent: f64_field(v, "memory_percent"),
        disk_percent: f64_field(v, "disk_percent"),
        temperature: f64_field(v, "temperature"),
    }
}

/// Build the channel-hopping context from `hop-supervisor.json`. The reference
/// channel is the live radio channel taken from the link block (matching the
/// Python channel-hops page, which reads the link channel for the chart's
/// reference line).
fn hopping_ctx(hop: Option<&Value>, link_channel: Option<i64>) -> HoppingCtx {
    let Some(v) = hop else {
        return HoppingCtx {
            radio_channel: link_channel,
            ..HoppingCtx::default()
        };
    };
    let history = v
        .get("history")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(hop_entry).collect())
        .unwrap_or_default();
    HoppingCtx {
        band: string_field(v, "band"),
        history,
        radio_channel: link_channel,
    }
}

/// One hop-history row. The producer keys are `at` / `from` / `to` / `ok` /
/// `trigger`; a row missing any of the first four is dropped, matching the
/// Python page's `_history` validity filter.
fn hop_entry(v: &Value) -> Option<HopEntry> {
    let at = f64_field(v, "at")?;
    let from_channel = i64_field(v, "from")?;
    let to_channel = i64_field(v, "to")?;
    let ok = v.get("ok").and_then(Value::as_bool)?;
    Some(HopEntry {
        at,
        from_channel,
        to_channel,
        ok,
        trigger: string_field(v, "trigger"),
    })
}

// ── disk + OS helpers ───────────────────────────────────────────────

/// Read the `X-ADOS-Key` from `pairing.json`. `None` when the file is absent,
/// unreadable, malformed, or carries no `api_key` — all of which are the
/// unpaired case where the header should simply be omitted.
fn load_api_key(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let blob: Value = serde_json::from_str(&text).ok()?;
    blob.get("api_key")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// Read a `/run/ados` JSON sidecar into a `Value`. `None` on a missing,
/// unreadable, or non-JSON file — the surface that reads it then falls back to
/// its empty state, mirroring the Python `_read_run_json` best-effort drain.
fn read_run_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read a small one-line breadcrumb file (device-id, build stamp), trimmed.
/// Empty string on any read error.
fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Resolve the node hostname for the top status bar + setup URL. Reads
/// `/etc/hostname` first (cheap, no syscall surprises in a container), then
/// falls back to the `HOSTNAME` env, then a stable placeholder.
fn read_hostname() -> String {
    let from_file = read_trimmed(Path::new("/etc/hostname"));
    if !from_file.is_empty() {
        return from_file;
    }
    if let Ok(h) = std::env::var("HOSTNAME") {
        let h = h.trim().to_string();
        if !h.is_empty() {
            return h;
        }
    }
    "ados".to_string()
}

/// The local wall clock as `HH:MM:SS`, matching the Python top bar's
/// `time.strftime("%H:%M:%S")`. Uses the system local offset; if the offset
/// can't be determined on this thread (the `time` crate refuses it in some
/// multi-threaded contexts) it falls back to UTC rather than blanking.
fn current_clock() -> String {
    use time::OffsetDateTime;
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source() -> StateSource {
        // A source with no real disk/api dependency: an unreachable base and
        // tmp sidecar paths. `compose` is driven with hand-built JSON so the
        // mapping is exercised without a live agent.
        StateSource::with_paths(
            "http://127.0.0.1:1",
            Path::new("/nonexistent/pairing.json"),
            PathBuf::from("/nonexistent/hop.json"),
            PathBuf::from("/nonexistent/health.json"),
        )
    }

    #[test]
    fn clock_is_hh_mm_ss() {
        let c = current_clock();
        assert_eq!(c.len(), 8);
        assert_eq!(&c[2..3], ":");
        assert_eq!(&c[5..6], ":");
        // Hours/minutes/seconds are all parseable two-digit fields.
        let parts: Vec<&str> = c.split(':').collect();
        assert_eq!(parts.len(), 3);
        for p in parts {
            assert_eq!(p.len(), 2);
            assert!(p.parse::<u32>().is_ok());
        }
    }

    #[test]
    fn missing_sources_yield_a_safe_default_context() {
        let mut src = source();
        let ctx = src.compose(None, None, None, None);
        // The chrome fields still resolve from the OS / clock.
        assert!(!ctx.hostname.is_empty());
        assert_eq!(ctx.clock.len(), 8);
        // Every sub-context is its default (no panic, no blank-field crash).
        assert!(ctx.link.rssi_dbm.is_none());
        assert_eq!(ctx.mesh.peer_count, 0);
        assert!(ctx.video.camera_label.is_none());
        // With NO status payload at all, the radio topology stays unset — the
        // `host_vbus` backfill only runs when a status snapshot exists (it
        // mirrors `_normalize_radio_fields`, which the Python poller runs on a
        // received payload, not on a missing one).
        assert!(ctx.radio.topology.is_none());
        // History buffers got their first (None) sample.
        assert_eq!(ctx.link.rssi_history.len(), 1);
        assert_eq!(ctx.system.cpu_history.len(), 1);
    }

    #[test]
    fn status_with_no_radio_block_backfills_host_vbus() {
        // When a status payload exists but carries no `radio` block, the
        // topology defaults to host_vbus (the brownout badge always has a
        // source on the polled path), matching `_normalize_radio_fields`.
        let mut src = source();
        let status = json!({"link": {"rssi_dbm": -60.0}});
        let ctx = src.compose(Some(&status), None, None, None);
        assert_eq!(ctx.radio.topology.as_deref(), Some("host_vbus"));
    }

    #[test]
    fn status_block_maps_link_role_mesh_and_system() {
        let mut src = source();
        let status = json!({
            "link": {
                "state": "connected",
                "rssi_dbm": -67.0,
                "snr_db": 22.0,
                "bitrate_kbps": 20000,
                "bitrate_mbps": 20.0,
                "fec_recovered": 1247,
                "fec_failed": 3,
                "channel": 149,
                "tx_power_dbm": 5,
                "packets_received": 5000,
                "packets_lost": 12,
                "loss_percent": 0.2
            },
            "role": {"current": "receiver", "configured": "receiver", "mesh_capable": true},
            "mesh": {"up": true, "peer_count": 2, "selected_gateway": "gw-2", "partition": false},
            "network": {"ap_ssid": "ados-ap", "uplink_type": "eth", "uplink_reachable": true},
            "system": {
                "cpu_pct": 22.0, "ram_used_mb": 1234, "ram_total_mb": 16384,
                "temp_c": 47.0, "uptime_seconds": 3600, "agent_version": "0.49.41"
            },
            "video": {"recording": true, "camera_count": 1, "mediamtx_ready": true}
        });
        let ctx = src.compose(Some(&status), None, None, None);

        assert_eq!(ctx.link.state.as_deref(), Some("connected"));
        assert_eq!(ctx.link.rssi_dbm, Some(-67.0));
        assert_eq!(ctx.link.channel, Some(149));
        // fec_lost falls back to the producer's fec_failed key.
        assert_eq!(ctx.link.fec_lost, Some(3));
        assert_eq!(ctx.link.tx_power_dbm, Some(5));

        assert_eq!(ctx.role.current.as_deref(), Some("receiver"));
        assert!(ctx.role.mesh_capable);

        assert!(ctx.mesh.up);
        assert_eq!(ctx.mesh.peer_count, 2);
        assert_eq!(ctx.mesh.selected_gateway.as_deref(), Some("gw-2"));

        assert_eq!(ctx.network.uplink_type.as_deref(), Some("eth"));
        assert!(ctx.network.uplink_reachable);

        assert_eq!(ctx.system.cpu_pct, Some(22.0));
        assert_eq!(ctx.system.agent_version.as_deref(), Some("0.49.41"));

        assert!(ctx.video.recording);
        assert_eq!(ctx.video.camera_count, 1);
        assert!(ctx.video.mediamtx_ready);
    }

    #[test]
    fn paired_drone_and_radio_topology_map() {
        let mut src = source();
        let status = json!({
            "paired_drone": {"device_id": "drone-aabbcc", "key_fingerprint": "deadbeef"},
            "radio": {"topology": "external_5v"}
        });
        let ctx = src.compose(Some(&status), None, None, None);
        assert_eq!(ctx.paired_drone.device_id.as_deref(), Some("drone-aabbcc"));
        assert_eq!(
            ctx.paired_drone.key_fingerprint.as_deref(),
            Some("deadbeef")
        );
        // The drone tile falls back to the paired_drone block when no live
        // `drone` block is present.
        assert_eq!(ctx.drone.device_id.as_deref(), Some("drone-aabbcc"));
        assert_eq!(ctx.radio.topology.as_deref(), Some("external_5v"));
    }

    #[test]
    fn hop_history_filters_and_maps_entries() {
        let mut src = source();
        let hop = json!({
            "band": "u-nii-3",
            "history": [
                {"at": 100.0, "from": 149, "to": 161, "ok": true, "trigger": "reactive"},
                {"at": 200.0, "from": 161, "to": 149, "ok": false},
                {"from": 1, "to": 2, "ok": true},
                "garbage"
            ]
        });
        let link = json!({"channel": 161});
        let status = json!({"link": link});
        let ctx = src.compose(Some(&status), None, Some(&hop), None);

        assert_eq!(ctx.hopping.band.as_deref(), Some("u-nii-3"));
        // The third row (missing `at`) and the bare string are dropped.
        assert_eq!(ctx.hopping.history.len(), 2);
        assert_eq!(ctx.hopping.history[0].from_channel, 149);
        assert_eq!(ctx.hopping.history[0].to_channel, 161);
        assert!(ctx.hopping.history[0].ok);
        assert_eq!(ctx.hopping.history[0].trigger.as_deref(), Some("reactive"));
        // The reference line is the live link channel.
        assert_eq!(ctx.hopping.radio_channel, Some(161));
    }

    #[test]
    fn health_sidecar_maps() {
        let mut src = source();
        let health = json!({
            "cpu_percent": 31.5, "memory_percent": 48.0,
            "disk_percent": 12.0, "temperature": 52.3
        });
        let ctx = src.compose(None, None, None, Some(&health));
        assert_eq!(ctx.health.cpu_percent, Some(31.5));
        assert_eq!(ctx.health.temperature, Some(52.3));
    }

    #[test]
    fn setup_snapshot_drives_completion_and_pair_code() {
        let mut src = source();
        let setup = json!({
            "finalized": false,
            "completion_percent": 70.0,
            "next_action": "pair with Mission Control",
            "pairing_code": "7YTFC7",
            "board_name": "rpi4b",
            "version": "0.49.41"
        });
        let ctx = src.compose(None, Some(&setup), None, None);
        assert!(!ctx.setup_finalized);
        assert_eq!(ctx.completion_percent, Some(70.0));
        assert_eq!(
            ctx.next_action.as_deref(),
            Some("pair with Mission Control")
        );
        assert_eq!(ctx.pairing.code.as_deref(), Some("7YTFC7"));
        assert_eq!(ctx.cloud.pair_code.as_deref(), Some("7YTFC7"));
        assert_eq!(ctx.device.board_name.as_deref(), Some("rpi4b"));
        assert_eq!(ctx.device.version.as_deref(), Some("0.49.41"));
    }

    #[test]
    fn history_buffers_accumulate_and_cap_at_sixty() {
        let mut src = source();
        let status = json!({
            "link": {"rssi_dbm": -55.0},
            "system": {"cpu_pct": 10.0, "temp_c": 40.0},
            "fc": {"battery_remaining": 88.0}
        });
        // Drive 65 ticks; the buffers should cap at HISTORY_LEN.
        let mut ctx = src.compose(Some(&status), None, None, None);
        for _ in 0..64 {
            ctx = src.compose(Some(&status), None, None, None);
        }
        assert_eq!(ctx.link.rssi_history.len(), HISTORY_LEN);
        assert_eq!(ctx.system.cpu_history.len(), HISTORY_LEN);
        assert_eq!(ctx.system.temp_history.len(), HISTORY_LEN);
        assert_eq!(ctx.fc.battery_history.len(), HISTORY_LEN);
        // Newest sample is at the tail.
        assert_eq!(ctx.link.rssi_history.last().copied().flatten(), Some(-55.0));
        assert_eq!(ctx.fc.battery_history.last().copied().flatten(), Some(88.0));
    }

    #[test]
    fn api_key_loads_from_pairing_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(&path, r#"{"api_key":"abc123","paired":true}"#).unwrap();
        assert_eq!(load_api_key(&path).as_deref(), Some("abc123"));
        // Missing file / empty key both yield None (the unpaired case).
        assert!(load_api_key(Path::new("/nonexistent/pairing.json")).is_none());
        std::fs::write(&path, r#"{"api_key":""}"#).unwrap();
        assert!(load_api_key(&path).is_none());
    }

    #[test]
    fn run_json_reads_present_and_tolerates_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, r#"{"a":1}"#).unwrap();
        assert!(read_run_json(&path).is_some());
        std::fs::write(&path, "not json").unwrap();
        assert!(read_run_json(&path).is_none());
        assert!(read_run_json(Path::new("/nonexistent/x.json")).is_none());
    }
}
