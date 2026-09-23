//! Live device-state reader that builds the page [`PageContext`].
//!
//! The native-resolution page UI in [`crate::pages`] reads from a single
//! [`PageContext`]; this module is the seam that fills it from the running
//! agent:
//!
//! * The agent's local REST API on `127.0.0.1:8080` — the ground-station
//!   status snapshot (`/api/v1/ground-station/status`, the union of link /
//!   network / system / role / mesh / video the dashboard reads) and the setup
//!   wizard state (`/api/v1/setup/status`: completion, next step, device
//!   identity, LAN host, the advertised Mission Control URL and the hardware
//!   checklist), plus the WFB radio-pair read (`/api/wfb/pair`) for the
//!   auto-pair flag the pair-drone page shows. Authentication is the `X-ADOS-Key` header read from
//!   `/etc/ados/pairing.json`; the pairing routes stay reachable while
//!   unpaired, and an empty key is correct for an unpaired box.
//! * The `/run/ados/hop-supervisor.json` sidecar the channel-hops surface reads
//!   directly because the data lives cross-process (the radio service owns
//!   it), used only while fresh.
//! * The logging store's query socket, for the diagnostics log tail
//!   ([`crate::log_tail`]), and the HAL board sidecar plus sysfs for the
//!   identity rows ([`crate::host_identity`]).
//!
//! The history buffers the sparkline surfaces read (RSSI, CPU, temperature,
//! battery) are not carried in any one snapshot; the source keeps a rolling
//! 60-sample ring per series and pushes the freshest reading each refresh.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::host_identity::{HostIdentity, HostPaths};
use crate::pages::{
    CloudCtx, DeviceCtx, DroneCtx, FcCtx, HardwareItem, HopEntry, HoppingCtx, LinkCtx, MeshCtx,
    MeshPeer, NetworkCtx, PageContext, PairedDroneCtx, PairingCtx, RoleCtx, SystemCtx, UplinkCtx,
    VideoCtx, WifiClientCtx, LINK_STATE_STALE,
};

/// The agent's local HTTP API base. Matches the Python LCD service's
/// `http://{api_host}:{api_port}` default (`127.0.0.1:8080`).
pub const DEFAULT_API_BASE: &str = "http://127.0.0.1:8080";

/// Where the network daemon writes the access point's passphrase, 0600.
/// Mirrors `ados_net::paths::AP_PASSPHRASE_PATH`; duplicated rather than
/// depended on, because the display crate does not otherwise link the network
/// daemon and one path constant is a cheaper coupling than a whole crate.
const AP_PASSPHRASE_PATH: &str = "/etc/ados/ap-passphrase";

/// Pairing-key file. The agent writes the persisted `X-ADOS-Key` here; the LCD
/// process reads it so its status polls authenticate against a paired agent
/// instead of getting 401'd (which would leave the panel rendering blanks).
pub const PAIRING_JSON_PATH: &str = "/etc/ados/pairing.json";

/// Device-id file (`/etc/ados/device-id`, hyphen) — the canonical id the about
/// and diagnostics surfaces show when the setup snapshot doesn't carry one.
pub const DEVICE_ID_PATH: &str = "/etc/ados/device-id";

/// Per-request timeout for the local status polls. Matches the Python LCD
/// service's `httpx` 0.9 s ceiling so a wedged agent never stalls the panel.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(900);

/// Rolling trend-buffer length for the sparkline surfaces (RSSI, CPU, temp,
/// battery). The pages render the last 60 samples; the source keeps exactly
/// that many, oldest first.
const HISTORY_LEN: usize = 60;

/// How old `hop-supervisor.json` may be and still describe a running
/// supervisor. Its writer refreshes it every 5 s, so three missed writes mean
/// the supervisor is not running and its last history is not current.
const HOP_SIDECAR_FRESH: Duration = Duration::from_secs(15);

/// `/run/ados` sidecar paths the surfaces read directly.
fn hop_supervisor_path() -> PathBuf {
    PathBuf::from("/run/ados/hop-supervisor.json")
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
    logd_socket: PathBuf,
    host_paths: HostPaths,
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
        )
    }

    /// Build a source with explicit base URL and sidecar paths (used by tests
    /// so the polls and reads round-trip without touching `/run` or `/etc`).
    pub fn with_paths(base: impl Into<String>, pairing_json: &Path, hop_path: PathBuf) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            api_key: load_api_key(pairing_json),
            agent,
            hostname: read_hostname(),
            hop_path,
            logd_socket: PathBuf::from(crate::log_tail::LOGD_QUERY_SOCKET),
            host_paths: HostPaths::default(),
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
    /// API; the hop-supervisor sidecar, the log tail and the host identity are
    /// read on-box. Each source is independent — a missing one degrades only the
    /// surfaces that read it, never the whole frame.
    pub fn build_context(&mut self) -> PageContext {
        let status = self.get_json("/api/v1/ground-station/status");
        let setup = self.get_json("/api/v1/setup/status");
        let hop = read_run_json(&self.hop_path)
            .filter(|_| file_age(&self.hop_path).is_some_and(|age| age <= HOP_SIDECAR_FRESH));
        // Best-effort schema-drift signal (never reject): warn when the
        // hop-supervisor sidecar was written by an agent with a different schema
        // version, then render anyway. The writer const lives in the radio crate,
        // so compare against the shared registry.
        if let Some(v) = hop.as_ref() {
            let got = v.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
            if let Some(ours) = ados_protocol::contracts::sidecar_version("hop-supervisor") {
                ados_protocol::sidecar::check_sidecar_version("hop-supervisor", got, ours);
            }
        }
        let mut ctx = self.compose(status.as_ref(), setup.as_ref(), hop.as_ref());
        ctx.paired_drone.auto_pair_enabled =
            auto_pair_enabled(self.get_json("/api/wfb/pair").as_ref());
        apply_host_identity(&mut ctx.device, HostIdentity::read(&self.host_paths));
        ctx.diagnostics.agent_logs = crate::log_tail::fetch(&self.logd_socket).unwrap_or_default();
        ctx
    }

    /// Map the three already-fetched JSON sources into a [`PageContext`],
    /// advancing the rolling history buffers. Split out from
    /// [`StateSource::build_context`] so the mapping is unit-testable without a
    /// live agent.
    pub fn compose(
        &mut self,
        status: Option<&Value>,
        setup: Option<&Value>,
        hop: Option<&Value>,
    ) -> PageContext {
        let mut ctx = PageContext {
            hostname: self.hostname.clone(),
            clock: current_clock(),
            ..PageContext::default()
        };

        if let Some(s) = status {
            ctx.link = link_ctx(get(s, "link"));
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
        }

        if let Some(su) = setup {
            apply_setup(&mut ctx, su);
        }

        ctx.hopping = hopping_ctx(hop, ctx.link.channel);
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

    /// Compose the setup-status identity fields (id, name, version) with the
    /// on-disk device-id breadcrumb and the system block's version.
    fn device_ctx(&self, setup: Option<&Value>, system: &SystemCtx) -> DeviceCtx {
        let mut device = DeviceCtx::default();
        if let Some(su) = setup {
            device.device_id = string_field(su, "device_id");
            device.device_name = string_field(su, "device_name");
            device.version = string_field(su, "version");
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
        device
    }
}

/// Fill the identity rows read off the box.
fn apply_host_identity(device: &mut DeviceCtx, host: HostIdentity) {
    device.board_name = host.board_name;
    device.mac_wired = host.mac_wired;
    device.mac_wireless = host.mac_wireless;
    device.primary_ip = host.primary_ip;
    device.primary_mac = host.primary_mac;
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

/// The `link` block as the pages read it.
///
/// A `stale` block keeps only its state. The agent flips a snapshot older than
/// its freshness window to `stale` but still carries the dead producer's last
/// RSSI, bitrate, FEC, channel and TX power; painting those would show a dead
/// radio as a live one, so every measured field is dropped here, once, for
/// every page.
fn link_ctx(v: Option<&Value>) -> LinkCtx {
    let Some(v) = v else {
        return LinkCtx::default();
    };
    let state = string_field(v, "state");
    // The declared power path is configuration, not a reading, so it survives a
    // stale snapshot.
    let topology = string_field(v, "topology");
    if state.as_deref() == Some(LINK_STATE_STALE) {
        return LinkCtx {
            state,
            topology,
            ..LinkCtx::default()
        };
    }
    LinkCtx {
        state,
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
        topology,
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
        auto_pair_enabled: None,
    }
}

/// The WFB auto-pair arm flag off a `GET /api/wfb/pair` body. `None` when the
/// read failed or the body carries no boolean flag.
fn auto_pair_enabled(wfb_pair: Option<&Value>) -> Option<bool> {
    wfb_pair?.get("auto_pair_enabled").and_then(Value::as_bool)
}

fn fc_ctx(v: Option<&Value>) -> FcCtx {
    let Some(v) = v else {
        return FcCtx::default();
    };
    FcCtx {
        vehicle: string_field(v, "vehicle"),
        mode: string_field(v, "mode"),
        armed: v.get("armed").and_then(Value::as_bool),
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
        // One pair-code field whichever spelling the producer used.
        pair_code: string_field(v, "pair_code").or_else(|| string_field(v, "pairing_code")),
        latency_ms: f64_field(v, "latency_ms"),
        rtt_ms: f64_field(v, "rtt_ms"),
        broadcasting: bool_field(v, "broadcasting"),
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

/// The mesh block. The agent reports `up` / `peer_count` as null (and
/// `stale: true`) when it has no current snapshot; those stay unknown here
/// rather than becoming "down" and "0 peers". The roster is `None` when the
/// block carries no `peers` array, which is different from an empty one.
fn mesh_ctx(v: Option<&Value>) -> MeshCtx {
    let Some(v) = v else {
        return MeshCtx::default();
    };
    let peers = v
        .get("peers")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(mesh_peer).collect());
    MeshCtx {
        up: v.get("up").and_then(Value::as_bool),
        stale: bool_field(v, "stale"),
        partition: bool_field(v, "partition"),
        peer_count: i64_field(v, "peer_count"),
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
        uplink_reachable: v.get("uplink_reachable").and_then(Value::as_bool),
        hotspot_ssid: string_field(v, "hotspot_ssid"),
        // Read from disk, never from the polled response: those routes answer
        // the LAN as well as loopback, so a passphrase in a payload is a
        // passphrase published to anyone who can reach the box.
        ap_passphrase: read_ap_passphrase(),
        wifi_client,
    }
}

/// The access point's passphrase, from its 0600 file on this box.
///
/// `None` for absent or unreadable — the page says so rather than rendering a
/// blank, because a blank where a passphrase belongs reads as "there isn't one"
/// and sends the operator looking for a network that needs no password.
fn read_ap_passphrase() -> Option<String> {
    let path =
        std::env::var("ADOS_AP_PASSPHRASE_PATH").unwrap_or_else(|_| AP_PASSPHRASE_PATH.to_string());
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
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
        disk_pct: f64_field(v, "disk_pct"),
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
        active: v.get("active").and_then(Value::as_bool),
        recording: bool_field(v, "recording"),
        fps: f64_field(v, "fps"),
        latency_ms: f64_field(v, "latency_ms"),
        bitrate_kbps: f64_field(v, "bitrate_kbps"),
        mediamtx_ready: v.get("mediamtx_ready").and_then(Value::as_bool),
        mediamtx_inbound_kbps: f64_field(v, "mediamtx_inbound_kbps"),
        camera_label: string_field(v, "camera_label"),
        camera_count: i64_field(v, "camera_count").unwrap_or(0),
    }
}

/// The setup status `hardware_check` block (`{profile, items: [...]}`), each
/// item carrying the producer's `id`, `label`, `required`, `state` and
/// `fix_hint`.
fn hardware_items(v: Option<&Value>) -> Vec<HardwareItem> {
    let Some(arr) = v.and_then(|v| v.get("items")).and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .map(|item| HardwareItem {
            id: string_field(item, "id"),
            label: string_field(item, "label"),
            required: bool_field(item, "required"),
            state: string_field(item, "state"),
            fix_hint: string_field(item, "fix_hint"),
        })
        .collect()
}

/// The Mission Control URL the setup service advertises: the `access_urls`
/// entry of kind `mission_control`.
fn mission_control_url(su: &Value) -> Option<String> {
    su.get("access_urls")?
        .as_array()?
        .iter()
        .find(|u| u.get("kind").and_then(Value::as_str) == Some("mission_control"))
        .and_then(|u| string_field(u, "url"))
}

/// Apply the setup-status snapshot onto the context: completion percent, the
/// next-step copy, the wizard-finalized flag, the LAN host and Mission Control
/// URL the setup and pairing links use, the hardware checklist, and the local
/// pair code the dashboard shows before a cloud relay binds one.
fn apply_setup(ctx: &mut PageContext, su: &Value) {
    ctx.setup_finalized = bool_field(su, "finalized")
        || bool_field(su, "setup_complete")
        || bool_field(su, "complete");
    ctx.completion_percent =
        f64_field(su, "completion_percent").or_else(|| f64_field(su, "percent"));
    ctx.next_action = string_field(su, "next_action").or_else(|| string_field(su, "next_step"));
    ctx.lan_host = string_field(su, "lan_host");
    ctx.mission_control_url = mission_control_url(su);
    ctx.hardware_check = hardware_items(get(su, "hardware_check"));

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

/// Build the channel-hopping context from a fresh `hop-supervisor.json` (the
/// caller drops a stale one). The reference channel is the live radio channel
/// taken from the link block.
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
        present: true,
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
pub(crate) fn load_api_key(path: &Path) -> Option<String> {
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

/// How long ago `path` was last modified. `None` when it cannot be read or its
/// time is in the future.
fn file_age(path: &Path) -> Option<Duration> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
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
        let ctx = src.compose(None, None, None);
        // The chrome fields still resolve from the OS / clock.
        assert!(!ctx.hostname.is_empty());
        assert_eq!(ctx.clock.len(), 8);
        // Every sub-context is its default (no panic, no blank-field crash).
        assert!(ctx.link.rssi_dbm.is_none());
        assert_eq!(ctx.mesh.peer_count, None);
        assert!(ctx.video.camera_label.is_none());
        // No snapshot, no supply path: the brownout warning has nothing to key on.
        assert!(ctx.link.topology.is_none());
        assert!(!ctx.hopping.present);
        // History buffers got their first (None) sample.
        assert_eq!(ctx.link.rssi_history.len(), 1);
        assert_eq!(ctx.system.cpu_history.len(), 1);
    }

    /// The power path comes from the link block the radio writes. A status with
    /// no topology leaves it unknown; it is never assumed to be host VBUS.
    #[test]
    fn topology_is_read_from_the_link_block_and_never_assumed() {
        let mut src = source();
        let status = json!({"link": {"rssi_dbm": -60.0}});
        let ctx = src.compose(Some(&status), None, None);
        assert_eq!(ctx.link.topology, None);
        let status = json!({"link": {"state": "stale", "topology": "powered_hub"}});
        let ctx = src.compose(Some(&status), None, None);
        assert_eq!(ctx.link.topology.as_deref(), Some("powered_hub"));
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
        let ctx = src.compose(Some(&status), None, None);

        assert_eq!(ctx.link.state.as_deref(), Some("connected"));
        assert_eq!(ctx.link.rssi_dbm, Some(-67.0));
        assert_eq!(ctx.link.channel, Some(149));
        // fec_lost falls back to the producer's fec_failed key.
        assert_eq!(ctx.link.fec_lost, Some(3));
        assert_eq!(ctx.link.tx_power_dbm, Some(5));

        assert_eq!(ctx.role.current.as_deref(), Some("receiver"));
        assert!(ctx.role.mesh_capable);

        assert_eq!(ctx.mesh.up, Some(true));
        assert_eq!(ctx.mesh.peer_count, Some(2));
        assert_eq!(ctx.mesh.selected_gateway.as_deref(), Some("gw-2"));
        // No roster in the block: unknown, not an empty list.
        assert!(ctx.mesh.peers.is_none());

        assert_eq!(ctx.network.uplink_type.as_deref(), Some("eth"));
        assert_eq!(ctx.network.uplink_reachable, Some(true));

        assert_eq!(ctx.system.cpu_pct, Some(22.0));
        assert_eq!(ctx.system.agent_version.as_deref(), Some("0.49.41"));

        assert!(ctx.video.recording);
        assert_eq!(ctx.video.camera_count, 1);
        assert_eq!(ctx.video.mediamtx_ready, Some(true));
        // Not reported, so not "inactive".
        assert_eq!(ctx.video.active, None);
    }

    /// A relay whose mesh poll has no current snapshot gets the agent's nulled,
    /// stale-marked block. That is "unknown", never "down" with zero peers.
    #[test]
    fn a_stale_mesh_block_is_unknown_not_down() {
        let mut src = source();
        let status = json!({
            "mesh": {"up": null, "peer_count": null, "selected_gateway": null,
                     "partition": null, "mesh_id": null, "stale": true}
        });
        let ctx = src.compose(Some(&status), None, None);
        assert_eq!(ctx.mesh.up, None);
        assert_eq!(ctx.mesh.peer_count, None);
        assert!(ctx.mesh.stale);
        assert_eq!(ctx.mesh.state(), crate::pages::MeshState::Unknown);

        // A stale block that still says up is unknown too.
        let status = json!({"mesh": {"up": true, "peer_count": 3, "stale": true}});
        let ctx = src.compose(Some(&status), None, None);
        assert_eq!(ctx.mesh.state(), crate::pages::MeshState::Unknown);

        let status = json!({"mesh": {"up": false, "peer_count": 0, "stale": false}});
        let ctx = src.compose(Some(&status), None, None);
        assert_eq!(ctx.mesh.state(), crate::pages::MeshState::Down);
    }

    /// A stale radio snapshot is no data: the dead producer's last RSSI,
    /// bitrate, FEC, channel and TX power never reach a page, and the RSSI
    /// trend breaks instead of carrying the frozen value forward.
    #[test]
    fn a_stale_link_block_carries_no_readings() {
        let mut src = source();
        let status = json!({
            "link": {
                "state": "stale",
                "rssi_dbm": -58.0,
                "bitrate_kbps": 20000,
                "bitrate_mbps": 20.0,
                "fec_recovered": 12,
                "fec_failed": 1,
                "channel": 149,
                "tx_power_dbm": 10
            }
        });
        let ctx = src.compose(Some(&status), None, None);
        assert!(ctx.link.is_stale());
        assert_eq!(ctx.link.rssi_dbm, None);
        assert_eq!(ctx.link.bitrate_mbps, None);
        assert_eq!(ctx.link.bitrate_kbps, None);
        assert_eq!(ctx.link.fec_recovered, None);
        assert_eq!(ctx.link.fec_lost, None);
        assert_eq!(ctx.link.channel, None);
        assert_eq!(ctx.link.tx_power_dbm, None);
        assert_eq!(ctx.link.rssi_history.last(), Some(&None));
    }

    /// No `armed` in the FC block is an unknown arm state, not DISARMED.
    #[test]
    fn an_absent_arm_report_stays_unknown() {
        let mut src = source();
        let ctx = src.compose(Some(&json!({"fc": {"mode": "LOITER"}})), None, None);
        assert_eq!(ctx.fc.armed, None);
        assert_eq!(ctx.drone.armed, None);
        let ctx = src.compose(Some(&json!({"fc": {"armed": false}})), None, None);
        assert_eq!(ctx.fc.armed, Some(false));
    }

    #[test]
    fn paired_drone_and_link_topology_map() {
        let mut src = source();
        let status = json!({
            "paired_drone": {"device_id": "drone-aabbcc", "key_fingerprint": "deadbeef"},
            "link": {"topology": "external_5v"}
        });
        let ctx = src.compose(Some(&status), None, None);
        assert_eq!(ctx.paired_drone.device_id.as_deref(), Some("drone-aabbcc"));
        assert_eq!(
            ctx.paired_drone.key_fingerprint.as_deref(),
            Some("deadbeef")
        );
        // The drone tile falls back to the paired_drone block when no live
        // `drone` block is present.
        assert_eq!(ctx.drone.device_id.as_deref(), Some("drone-aabbcc"));
        assert_eq!(ctx.link.topology.as_deref(), Some("external_5v"));
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
        let ctx = src.compose(Some(&status), None, Some(&hop));

        assert!(ctx.hopping.present);
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

    /// A hop-supervisor sidecar older than its refresh window describes no
    /// running supervisor, so the page is told there is no data rather than
    /// being handed the last history as current.
    #[test]
    fn a_stale_hop_sidecar_is_not_present() {
        let dir = tempfile::tempdir().unwrap();
        let hop_path = dir.path().join("hop-supervisor.json");
        std::fs::write(&hop_path, r#"{"band":"u-nii-3","history":[]}"#).unwrap();
        let mut src = StateSource::with_paths(
            "http://127.0.0.1:1",
            Path::new("/nonexistent/pairing.json"),
            hop_path.clone(),
        );
        assert!(src.build_context().hopping.present);

        let old = std::time::SystemTime::now() - HOP_SIDECAR_FRESH - Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&hop_path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let ctx = src.build_context();
        assert!(!ctx.hopping.present);
        assert!(ctx.hopping.band.is_none());
    }

    /// The one system-metrics source: the status `system` block, disk included.
    #[test]
    fn the_system_block_carries_every_host_metric() {
        let mut src = source();
        let status = json!({"system": {
            "cpu_pct": 31.5, "ram_used_mb": 1024.0, "ram_total_mb": 4096.0,
            "temp_c": 52.3, "disk_pct": 12.0
        }});
        let ctx = src.compose(Some(&status), None, None);
        assert_eq!(ctx.system.cpu_pct, Some(31.5));
        assert_eq!(ctx.system.ram_pct(), Some(25.0));
        assert_eq!(ctx.system.disk_pct, Some(12.0));
        assert_eq!(ctx.system.temp_c, Some(52.3));
    }

    #[test]
    fn setup_snapshot_drives_completion_and_pair_code() {
        let mut src = source();
        let setup = json!({
            "finalized": false,
            "completion_percent": 70.0,
            "next_action": "pair with Mission Control",
            "pairing_code": "7YTFC7",
            "device_name": "gs-example",
            "version": "0.49.41"
        });
        let ctx = src.compose(None, Some(&setup), None);
        assert!(!ctx.setup_finalized);
        assert_eq!(ctx.completion_percent, Some(70.0));
        assert_eq!(
            ctx.next_action.as_deref(),
            Some("pair with Mission Control")
        );
        assert_eq!(ctx.pairing.code.as_deref(), Some("7YTFC7"));
        assert_eq!(ctx.cloud.pair_code.as_deref(), Some("7YTFC7"));
        assert_eq!(ctx.device.device_name.as_deref(), Some("gs-example"));
        assert_eq!(ctx.device.version.as_deref(), Some("0.49.41"));
    }

    /// The setup status carries the LAN host, the advertised Mission Control
    /// URL and the hardware checklist (`hardware_check.items`, with the
    /// producer's ids and `required` flag). All three feed the early-life tiles.
    #[test]
    fn setup_snapshot_maps_reach_names_and_the_hardware_checklist() {
        let mut src = source();
        let setup = json!({
            "lan_host": "ados-9f2c1a.local",
            "access_urls": [
                {"kind": "setup", "url": "http://192.168.1.50:8080/setup"},
                {"kind": "mission_control", "url": "https://mc.example.com"}
            ],
            "hardware_check": {
                "profile": "ground_station",
                "items": [
                    {"id": "board", "label": "Companion compute", "required": true, "state": "ok"},
                    {"id": "radio_wfb", "label": "WFB radio adapter", "required": true,
                     "state": "missing", "fix_hint": "Plug in an RTL8812EU/AU USB adapter."}
                ]
            }
        });
        let ctx = src.compose(None, Some(&setup), None);
        assert_eq!(ctx.lan_host.as_deref(), Some("ados-9f2c1a.local"));
        assert_eq!(
            ctx.mission_control_url.as_deref(),
            Some("https://mc.example.com")
        );
        let radio = ctx
            .hardware_check
            .iter()
            .find(|it| it.id.as_deref() == Some("radio_wfb"))
            .expect("the radio row maps");
        assert!(radio.required);
        assert_eq!(radio.state.as_deref(), Some("missing"));
        assert_eq!(ctx.hardware_check.len(), 2);

        // The ground-station status has no hardware_check block; a stray one
        // there is not read.
        let status =
            json!({"hardware_check": {"items": [{"id": "radio_wfb", "state": "missing"}]}});
        assert!(src
            .compose(Some(&status), None, None)
            .hardware_check
            .is_empty());
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
        let mut ctx = src.compose(Some(&status), None, None);
        for _ in 0..64 {
            ctx = src.compose(Some(&status), None, None);
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
    fn auto_pair_flag_reads_the_wfb_pair_body() {
        assert_eq!(
            auto_pair_enabled(Some(&json!({"auto_pair_enabled": false}))),
            Some(false)
        );
        assert_eq!(
            auto_pair_enabled(Some(&json!({"auto_pair_enabled": true}))),
            Some(true)
        );
        // No answer, or no flag, is unknown rather than a guessed state.
        assert_eq!(auto_pair_enabled(None), None);
        assert_eq!(auto_pair_enabled(Some(&json!({"paired": false}))), None);
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
