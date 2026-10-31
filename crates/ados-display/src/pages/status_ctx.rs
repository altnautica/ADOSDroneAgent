//! Node status blocks the page context carries: mesh, network, uplink,
//! system, hardware check, channel hops, video, device and diagnostics.

/// One mesh peer row for the mesh detail list.
#[derive(Debug, Clone, Default)]
pub struct MeshPeer {
    pub device_id: Option<String>,
    pub role: Option<String>,
    pub last_seen_seconds_ago: Option<f64>,
}

/// Mesh-network state (`mesh` state block) plus the peer roster.
#[derive(Debug, Clone, Default)]
pub struct MeshCtx {
    /// `None` when the agent has no current mesh snapshot: "down" is a
    /// measurement the node cannot make then, and must not be shown.
    pub up: Option<bool>,
    /// The agent marked the block stale (its producer stopped refreshing).
    pub stale: bool,
    pub partition: bool,
    pub peer_count: Option<i64>,
    pub selected_gateway: Option<String>,
    pub mesh_id: Option<String>,
    /// The peer roster, `None` when the snapshot does not carry one (the count
    /// may still be known).
    pub peers: Option<Vec<MeshPeer>>,
}

/// What a panel may say about the mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshState {
    Up,
    Down,
    /// No current snapshot, or a stale one.
    Unknown,
}

impl MeshCtx {
    /// The mesh state the panel may show. A stale block is unknown whatever its
    /// last `up` value was.
    pub fn state(&self) -> MeshState {
        match (self.stale, self.up) {
            (false, Some(true)) => MeshState::Up,
            (false, Some(false)) => MeshState::Down,
            _ => MeshState::Unknown,
        }
    }
}

/// WiFi-client uplink details for the uplink detail fallback.
#[derive(Debug, Clone, Default)]
pub struct WifiClientCtx {
    pub connected: bool,
    pub ssid: Option<String>,
    pub signal_dbm: Option<f64>,
}

/// Network state (`network` state block): hotspot, USB tether, uplink kind, and
/// the WiFi-client fallback the uplink surface reads.
#[derive(Debug, Clone, Default)]
pub struct NetworkCtx {
    pub ap_ssid: Option<String>,
    /// The AP gateway address; the agent reports it only while the AP runs, so
    /// its presence is the "broadcasting" signal.
    pub ap_ip: Option<String>,
    pub usb_ip: Option<String>,
    /// `eth`, `wifi`, `cellular`, `usb`, or `none`; `None` when unknown.
    pub uplink_type: Option<String>,
    /// `None` when unknown.
    pub uplink_reachable: Option<bool>,
    pub hotspot_ssid: Option<String>,
    /// The access point's passphrase, read from its 0600 file ON THE BOX.
    ///
    /// Deliberately NOT sourced from the agent's REST API: the display polls
    /// `127.0.0.1:8080`, but those routes are reachable from the LAN too, so
    /// putting the passphrase in a response would publish it to anyone who can
    /// reach the ground station. The panel is a physical surface — you have to
    /// be standing in front of it — which is the one place showing it is
    /// appropriate.
    ///
    /// `None` when the file is absent or unreadable, which the page renders as
    /// an honest "unavailable" rather than a blank the operator reads as empty.
    pub ap_passphrase: Option<String>,
    pub wifi_client: WifiClientCtx,
}

/// Cellular-modem status for the uplink detail's cellular band.
#[derive(Debug, Clone, Default)]
pub struct UplinkCtx {
    pub modem_present: bool,
    pub rsrp_dbm: Option<f64>,
    pub rsrq_db: Option<f64>,
    pub sinr_db: Option<f64>,
    pub band: Option<String>,
    pub ip: Option<String>,
    /// Radio access technology (`LTE`, `5G`, …).
    pub tech: Option<String>,
    /// Reason string when the modem reports not-present.
    pub reason: Option<String>,
}

/// System metrics (`system` state block) for the top bar, the link-stats SYSTEM
/// band and diagnostics: the one host-metrics source every page reads.
#[derive(Debug, Clone, Default)]
pub struct SystemCtx {
    pub cpu_pct: Option<f64>,
    pub ram_used_mb: Option<f64>,
    pub ram_total_mb: Option<f64>,
    pub temp_c: Option<f64>,
    /// Root filesystem in use, in percent.
    pub disk_pct: Option<f64>,
    pub uptime_seconds: Option<f64>,
    pub agent_version: Option<String>,
    /// 60-sample CPU trend for the footer sparkline.
    pub cpu_history: Vec<Option<f64>>,
    /// 60-sample temperature trend for the footer sparkline.
    pub temp_history: Vec<Option<f64>>,
}

impl SystemCtx {
    /// RAM in use as a percentage of total, when both are reported.
    pub fn ram_pct(&self) -> Option<f64> {
        match (self.ram_used_mb, self.ram_total_mb) {
            (Some(used), Some(total)) if total > 0.0 => Some(used / total * 100.0),
            _ => None,
        }
    }
}

/// One hardware-check row (setup status `hardware_check.items`) for the
/// early-life checklist.
#[derive(Debug, Clone, Default)]
pub struct HardwareItem {
    pub id: Option<String>,
    pub label: Option<String>,
    /// Whether the node needs this component for its profile.
    pub required: bool,
    /// `ok`, `warning`, `missing`, `checking`, or `unknown`.
    pub state: Option<String>,
    pub fix_hint: Option<String>,
}

/// One channel-hop record for the channel-history surface.
#[derive(Debug, Clone, Default)]
pub struct HopEntry {
    /// Unix timestamp of the hop.
    pub at: f64,
    pub from_channel: i64,
    pub to_channel: i64,
    pub ok: bool,
    /// `periodic` or `reactive`.
    pub trigger: Option<String>,
}

/// The hop-supervisor snapshot for the channel-history surface.
#[derive(Debug, Clone, Default)]
pub struct HoppingCtx {
    /// Whether a fresh hop-supervisor sidecar was read this tick. Without one the
    /// page cannot say anything about the supervisor.
    pub present: bool,
    pub band: Option<String>,
    pub history: Vec<HopEntry>,
    /// The live radio channel for the reference line.
    pub radio_channel: Option<i64>,
}

/// Live video metrics for the video preview + link-stats surfaces.
#[derive(Debug, Clone, Default)]
pub struct VideoCtx {
    /// Decoder kind reported by the local tap (`h264 v4l2m2m`, …).
    pub decoder: Option<String>,
    /// Whether the local decode tap has a live frame; `None` when not reported.
    pub active: Option<bool>,
    pub recording: bool,
    pub fps: Option<f64>,
    pub latency_ms: Option<f64>,
    pub bitrate_kbps: Option<f64>,
    /// Whether mediamtx reports the path ready; `None` when not reported.
    pub mediamtx_ready: Option<bool>,
    /// mediamtx inbound throughput in kilobits per second.
    pub mediamtx_inbound_kbps: Option<f64>,
    pub camera_label: Option<String>,
    pub camera_count: i64,
}

/// Device identity for the about and diagnostics surfaces: id, name and version
/// from `/api/v1/setup/status`, the board from the HAL board sidecar, and the
/// NIC addresses from sysfs.
#[derive(Debug, Clone, Default)]
pub struct DeviceCtx {
    pub device_id: Option<String>,
    pub device_name: Option<String>,
    pub version: Option<String>,
    pub board_name: Option<String>,
    /// MAC of the first wired NIC, whatever it is named.
    pub mac_wired: Option<String>,
    /// MAC of the first wireless NIC in managed mode.
    pub mac_wireless: Option<String>,
    /// The source address the default route uses.
    pub primary_ip: Option<String>,
    /// MAC of the default-route interface.
    pub primary_mac: Option<String>,
}

/// The diagnostics agent-log tail for the diagnostics surface.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsCtx {
    /// Recent agent log lines from the logging store, oldest first.
    pub agent_logs: Vec<String>,
}
