//! Page composers for the native-resolution LCD render path.
//!
//! Each page lays the [`crate::graphics`] widgets out into a full-panel canvas
//! and returns it for packing and blitting. The ground-station landscape
//! dashboard (top status bar, content tiles, bottom system bar), the pairing /
//! detail screens, and the menu live here.
//!
//! This module is the shared home for the page system. It defines:
//!
//! * [`PageContext`] — every data field any screen reads, gathered into nested
//!   sub-structs ([`LinkCtx`], [`DroneCtx`], [`CloudCtx`], and the rest). The
//!   per-page modules only READ this context; they never extend it.
//! * [`Page`] — the contract a page implements (id, refresh cadence, render,
//!   hit zones).
//! * [`HitZone`] — a rectangular touch target on a page.
//! * The panel geometry consts (top status bar, bottom tab bar, content region)
//!   and [`tile_rects`] for the dashboard's 2x2 grid.
//!
//! The per-page composer modules each own one screen and lay it out from the
//! shared [`PageContext`]. [`blank_panel`] is the starting canvas every page
//! fills before painting its chrome and surfaces.

use crate::graphics::palette::Palette;
use crate::graphics::primitives::Canvas;

pub mod about;
pub mod access_point;
pub mod calibration;
pub mod channel_hops;
pub mod dashboard;
pub mod diagnostics;
pub mod drone;
pub mod link_stats;
pub mod mesh;
pub mod more;
pub mod pair_drone;
pub mod plugin;
pub mod radio_link;
pub mod settings;
pub mod uplink;
pub mod video;

// ── panel geometry ──────────────────────────────────────────────────

/// Full panel width in landscape orientation.
pub const PANEL_W: u32 = 480;
/// Full panel height in landscape orientation.
pub const PANEL_H: u32 = 320;

/// Height of the persistent top status bar.
pub const TOP_BAR_H: u32 = 32;
/// Height of the persistent bottom tab bar.
pub const BOTTOM_BAR_H: u32 = 44;

/// Width of the page content region (full panel width).
pub const CONTENT_W: u32 = PANEL_W;
/// Height of the page content region: panel minus top bar and bottom bar.
pub const CONTENT_H: u32 = PANEL_H - TOP_BAR_H - BOTTOM_BAR_H;
/// Top edge (in panel-global y) of the page content region.
pub const CONTENT_Y: u32 = TOP_BAR_H;

/// Outer margin and inter-tile gap for the dashboard's inset 2x2 grid.
pub const TILE_OUTER_MARGIN: i32 = 8;
/// Gap between adjacent dashboard tiles.
pub const TILE_GAP: i32 = 8;

/// A single tile rectangle in page-local coordinates (origin at the top-left of
/// the content region, not the panel-global origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The four dashboard tile rectangles in page-local content coordinates.
///
/// Order is top-left, top-right, bottom-left, bottom-right — matching the
/// `(top_left, top_right, bottom_left, bottom_right)` router order the
/// dashboard paints in. Tile size derives from the same outer-margin + gap math
/// the inset dashboard uses: `(CONTENT_W - 2*margin - gap) / 2` wide by
/// `(CONTENT_H - 2*margin - gap) / 2` tall.
pub fn tile_rects() -> [TileRect; 4] {
    let tile_w = (CONTENT_W as i32 - TILE_OUTER_MARGIN * 2 - TILE_GAP) / 2;
    let tile_h = (CONTENT_H as i32 - TILE_OUTER_MARGIN * 2 - TILE_GAP) / 2;
    let col_a = TILE_OUTER_MARGIN;
    let col_b = TILE_OUTER_MARGIN + tile_w + TILE_GAP;
    let row_a = TILE_OUTER_MARGIN;
    let row_b = TILE_OUTER_MARGIN + tile_h + TILE_GAP;
    [
        TileRect {
            x: col_a,
            y: row_a,
            w: tile_w,
            h: tile_h,
        },
        TileRect {
            x: col_b,
            y: row_a,
            w: tile_w,
            h: tile_h,
        },
        TileRect {
            x: col_a,
            y: row_b,
            w: tile_w,
            h: tile_h,
        },
        TileRect {
            x: col_b,
            y: row_b,
            w: tile_w,
            h: tile_h,
        },
    ]
}

// ── hit zones ───────────────────────────────────────────────────────

/// The action a hit zone dispatches when tapped. The navigator stage maps these
/// to route changes, modal pushes, or REST calls; the page layer only needs to
/// label its zones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitAction {
    /// Switch to a top-level tab by page id.
    GoTab(&'static str),
    /// Drill into a detail page by page id.
    OpenDetail(&'static str),
    /// Pop the current modal / detail page back to its parent.
    Back,
    /// A page-defined action keyed by a stable string id (button taps, slider
    /// regions, list rows). The owning page interprets the key.
    Custom(String),
}

/// A rectangular touch target on a page, in the page's own coordinate frame:
/// panel-global for a [`Chrome::FullScreen`] page, content-local (origin at the
/// top-left of the 480x244 content region) for a [`Chrome::Tabbed`] one. See
/// [`Chrome::origin_y`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitZone {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub action: HitAction,
}

impl HitZone {
    /// Build a zone with the given rectangle and action.
    pub fn new(x: i32, y: i32, w: i32, h: i32, action: HitAction) -> Self {
        Self { x, y, w, h, action }
    }

    /// Return true if `(px, py)` lies inside this zone (half-open on the far
    /// edges, matching the page hit-test convention).
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

// ── page-context data model ─────────────────────────────────────────

/// WFB radio-link telemetry. Carries the fields of the `link` state block and
/// the `/api/wfb` snapshot that `ados_protocol::wfb_status` derives: signal,
/// throughput, FEC, channel, and the watchdog counters the diagnostics
/// surfaces care about.
#[derive(Debug, Clone, Default)]
pub struct LinkCtx {
    /// Link-layer connection state (`connected`, `connecting`, `stale`, …).
    /// On `stale` every measured field below is `None`: the producer stopped
    /// writing and its last numbers are not a reading.
    pub state: Option<String>,
    pub rssi_dbm: Option<f64>,
    pub snr_db: Option<f64>,
    pub noise_dbm: Option<f64>,
    pub loss_percent: Option<f64>,
    /// Throughput in megabits per second (the tile's display unit).
    pub bitrate_mbps: Option<f64>,
    /// Throughput in kilobits per second (the canonical snapshot key).
    pub bitrate_kbps: Option<f64>,
    pub fec_recovered: Option<i64>,
    pub fec_lost: Option<i64>,
    pub channel: Option<i64>,
    pub frequency_mhz: Option<i64>,
    pub bandwidth_mhz: Option<i64>,
    pub tx_power_dbm: Option<i64>,
    /// The radio's declared power path (`host_vbus`, `powered_hub`,
    /// `external_5v`), which the brownout warning keys on. `None` when the
    /// snapshot does not carry it: the supply path is then unknown, not host VBUS.
    pub topology: Option<String>,
    pub mcs_index: Option<i64>,
    /// Live Reed-Solomon ratio (data shards / total) of the transmit plane.
    pub fec_k: Option<i64>,
    pub fec_n: Option<i64>,
    /// Closed-loop FEC controller armed flag + its current ladder rung name.
    pub adaptive_bitrate_enabled: Option<bool>,
    pub recommended_tier_name: Option<String>,
    pub packets_received: Option<i64>,
    pub packets_lost: Option<i64>,
    /// 60-sample RSSI trend for the sparkline surfaces (`None` marks a gap).
    pub rssi_history: Vec<Option<f64>>,
}

impl LinkCtx {
    /// Whether the agent reports the radio snapshot as stale (the producer has
    /// stopped refreshing it).
    pub fn is_stale(&self) -> bool {
        self.state.as_deref() == Some(LINK_STATE_STALE)
    }
}

/// The `link.state` the agent reports once the radio snapshot has aged past its
/// freshness window.
pub const LINK_STATE_STALE: &str = "stale";

/// What a panel may say about the vehicle's arm state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmState {
    Armed,
    Disarmed,
    /// No telemetry reports it. Never shown as DISARMED: an unknown vehicle may
    /// be armed and flying.
    Unknown,
}

impl ArmState {
    pub fn from_report(armed: Option<bool>) -> Self {
        match armed {
            Some(true) => Self::Armed,
            Some(false) => Self::Disarmed,
            None => Self::Unknown,
        }
    }

    /// The panel label: `ARMED`, `DISARMED`, or `ARM —` when unknown.
    pub fn label(self) -> &'static str {
        match self {
            Self::Armed => "ARMED",
            Self::Disarmed => "DISARMED",
            Self::Unknown => "ARM —",
        }
    }

    /// The label colour: success when armed, secondary when disarmed, tertiary
    /// (no-data) when unknown.
    pub fn color(self, palette: &Palette) -> embedded_graphics::pixelcolor::Rgb888 {
        match self {
            Self::Armed => palette.status_success,
            Self::Disarmed => palette.text_secondary,
            Self::Unknown => palette.text_tertiary,
        }
    }
}

/// Drone telemetry as carried in the `drone` state block (the dashboard tile
/// shape) — identity plus a light FC summary.
#[derive(Debug, Clone, Default)]
pub struct DroneCtx {
    pub device_id: Option<String>,
    pub fc_mode: Option<String>,
    pub battery_pct: Option<f64>,
    pub gps_sats: Option<i64>,
    pub armed: Option<bool>,
    pub key_fingerprint: Option<String>,
}

/// The paired-drone record (`paired_drone` state block) the drone / pair detail
/// surfaces read for the radio-pair identity.
#[derive(Debug, Clone, Default)]
pub struct PairedDroneCtx {
    pub device_id: Option<String>,
    pub key_fingerprint: Option<String>,
    /// Seconds since the pair was established (relative-time display).
    pub paired_at_seconds: Option<f64>,
    /// Unix timestamp of the pair (absolute clock display).
    pub paired_at: Option<f64>,
    /// The WFB auto-pair arm flag from `GET /api/wfb/pair`. `None` when that
    /// read did not answer, so the page cannot claim either state.
    pub auto_pair_enabled: Option<bool>,
}

/// Live flight-controller telemetry from the dashboard snapshot's `fc` block.
#[derive(Debug, Clone, Default)]
pub struct FcCtx {
    pub vehicle: Option<String>,
    pub mode: Option<String>,
    /// `None` when no telemetry reports it; see [`ArmState`].
    pub armed: Option<bool>,
    pub battery_voltage: Option<f64>,
    pub battery_remaining: Option<f64>,
    pub gps_fix_type: Option<i64>,
    pub gps_satellites_visible: Option<i64>,
    /// 60-sample battery-percent trend for the drone detail sparkline.
    pub battery_history: Vec<Option<f64>>,
}

/// Cloud-relay / Mission Control pairing state (`cloud` state block + the
/// snapshot `cloud` block the uplink surfaces read).
#[derive(Debug, Clone, Default)]
pub struct CloudCtx {
    /// Whether Mission Control has claimed this node.
    pub paired: bool,
    /// Six-character pair code (cloud or local source).
    pub pair_code: Option<String>,
    pub latency_ms: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub broadcasting: bool,
    /// MQTT transport state (`connected`, `connecting`, …). `None` = not reported.
    pub mqtt_state: Option<String>,
    /// HTTP heartbeat state (`ok`, `connecting`, …). `None` = not reported.
    pub http_state: Option<String>,
    /// The paired drone id once the cloud relay has bound one.
    pub drone_id: Option<String>,
}

/// The local pairing code + pairing-window state (`pairing` state block).
#[derive(Debug, Clone, Default)]
pub struct PairingCtx {
    pub code: Option<String>,
    /// Whether a local pairing window is currently open.
    pub window_active: bool,
    /// Seconds remaining on the open pairing window.
    pub window_remaining_seconds: Option<f64>,
}

/// The node's mesh role (`role` state block).
#[derive(Debug, Clone, Default)]
pub struct RoleCtx {
    /// `direct`, `relay`, `receiver`, or unset.
    pub current: Option<String>,
    pub configured: Option<String>,
    pub mesh_capable: bool,
}

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

/// The full render context handed to every page. The chrome and the page
/// composers READ this; no page mutates it.
#[derive(Debug, Clone, Default)]
pub struct PageContext {
    /// The node's hostname (top status bar + setup URL).
    pub hostname: String,
    /// The wall-clock string for the top bar (`HH:MM:SS`).
    pub clock: String,
    /// Whether the first-boot setup wizard has been finalized.
    pub setup_finalized: bool,
    /// Setup completion percentage for the wizard tile.
    pub completion_percent: Option<f64>,
    /// The next-step copy for the wizard tile.
    pub next_action: Option<String>,
    /// The node's LAN-routable host (setup status `lan_host`, e.g.
    /// `ados-9f2c1a.local`), the reach name the setup and pairing links use.
    pub lan_host: Option<String>,
    /// The Mission Control base URL the agent advertises (setup status
    /// `access_urls` entry of kind `mission_control`).
    pub mission_control_url: Option<String>,
    pub link: LinkCtx,
    pub drone: DroneCtx,
    pub paired_drone: PairedDroneCtx,
    pub fc: FcCtx,
    pub cloud: CloudCtx,
    pub pairing: PairingCtx,
    pub role: RoleCtx,
    pub mesh: MeshCtx,
    pub network: NetworkCtx,
    pub uplink: UplinkCtx,
    pub system: SystemCtx,
    pub hardware_check: Vec<HardwareItem>,
    pub hopping: HoppingCtx,
    pub video: VideoCtx,
    pub device: DeviceCtx,
    pub diagnostics: DiagnosticsCtx,
}

/// The hosted Mission Control the agent advertises when the operator has not
/// configured another (the setup service's `mission_control_url` default).
pub const DEFAULT_MISSION_CONTROL_URL: &str = "https://command.altnautica.com";

impl PageContext {
    /// The Mission Control pairing deep link for `code`:
    /// `<mission control>/pair?code=<CODE>`, plus `&host=<lan host>` when the
    /// node's LAN name is known. Mission Control's `/pair` route reads exactly
    /// these two parameters and opens its pairing dialog pre-filled, so a phone
    /// that scans the panel QR lands on the claim, not on a page that drops the
    /// code.
    pub fn pair_deep_link(&self, code: &str) -> String {
        let base = self
            .mission_control_url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or(DEFAULT_MISSION_CONTROL_URL)
            .trim()
            .trim_end_matches('/');
        let code: String = code
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let mut url = format!("{base}/pair?code={code}");
        if let Some(host) = self.lan_host.as_deref().map(str::trim) {
            let host: String = host
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
                .collect();
            if !host.is_empty() {
                url.push_str("&host=");
                url.push_str(&host);
            }
        }
        url
    }

    /// The setup wizard URL on this node: its LAN host when the setup service
    /// reports one, else the access point gateway while the AP runs. `None` when
    /// no reach name is known; a guessed hostname would send the operator to a
    /// name that does not resolve.
    pub fn setup_url(&self) -> Option<String> {
        self.lan_host
            .as_deref()
            .or(self.network.ap_ip.as_deref())
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| format!("http://{h}:8080"))
    }
}

// ── page trait ──────────────────────────────────────────────────────

/// Which chrome a page paints, and so which coordinate frame its hit zones use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chrome {
    /// Top status bar + bottom tab bar. Zones are content-local: y = 0 is the
    /// first row under the top bar, and taps in the tab band switch tabs.
    Tabbed,
    /// The whole 480x320 panel is the page (detail pages, the overflow menu, the
    /// plugin page). Zones are panel-global and there is no tab band.
    FullScreen,
}

impl Chrome {
    /// Panel-global y of the page's zone frame origin.
    pub fn origin_y(self) -> i32 {
        match self {
            Self::Tabbed => CONTENT_Y as i32,
            Self::FullScreen => 0,
        }
    }

    /// Whether the page paints the bottom tab bar (and so owns the tab band).
    pub fn has_tab_bar(self) -> bool {
        matches!(self, Self::Tabbed)
    }
}

/// An agent write a panel control performs over the local API.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRequest {
    /// `POST`, `PUT` or `DELETE`.
    pub method: &'static str,
    pub path: &'static str,
    /// The JSON body, when the route takes one.
    pub body: Option<serde_json::Value>,
    /// What the operator asked for, for the acknowledgement line.
    pub label: String,
}

/// What a page does with one of its own custom hit-zone keys.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelAction {
    /// Handled inside the page (an overlay opened or closed); repaint now.
    Repaint,
    /// Perform this agent write and show its outcome.
    Agent(AgentRequest),
}

/// The contract every LCD page implements.
///
/// The render loop reads [`Page::refresh_hz`] to pace the page, calls
/// [`Page::render`] each tick to paint a full panel canvas, and queries
/// [`Page::hit_zones`] to route taps. A page is a stateless composer over the
/// shared [`PageContext`]; per-page transient state (drag, scroll) lives on the
/// concrete page struct.
pub trait Page {
    /// Stable route id the navigator and persistence key on.
    fn id(&self) -> &'static str;

    /// The chrome the page paints, which fixes the frame of its hit zones.
    fn chrome(&self) -> Chrome;

    /// Preferred redraw cadence in hertz.
    fn refresh_hz(&self) -> f32;

    /// Paint the full 480x320 panel for this page (chrome included).
    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas;

    /// Return the page's active hit zones, in the frame [`Page::chrome`] names.
    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone>;

    /// Resolve one of this page's [`HitAction::Custom`] keys. `None` when the
    /// key does nothing right now (for example a stepper whose current value is
    /// unknown). Pages without custom zones keep the default.
    fn on_custom(&self, _key: &str, _ctx: &PageContext) -> Option<PanelAction> {
        None
    }
}

/// Allocate a blank full-panel canvas filled with the palette background.
///
/// Every page starts from this and paints its chrome and surfaces over it, so
/// the background is always cleared before a page composes its frame.
pub fn blank_panel(palette: &Palette) -> Canvas {
    Canvas::new(PANEL_W, PANEL_H, palette.bg_primary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    #[test]
    fn content_region_math_adds_up() {
        assert_eq!(TOP_BAR_H + CONTENT_H + BOTTOM_BAR_H, PANEL_H);
        assert_eq!(CONTENT_Y, TOP_BAR_H);
    }

    #[test]
    fn tile_rects_form_a_2x2_grid() {
        let tiles = tile_rects();
        // Top row shares a y; bottom row shares a y.
        assert_eq!(tiles[0].y, tiles[1].y);
        assert_eq!(tiles[2].y, tiles[3].y);
        // Left column shares an x; right column shares an x.
        assert_eq!(tiles[0].x, tiles[2].x);
        assert_eq!(tiles[1].x, tiles[3].x);
        // All four tiles are the same size.
        for t in &tiles[1..] {
            assert_eq!(t.w, tiles[0].w);
            assert_eq!(t.h, tiles[0].h);
        }
        // The expected inset tile size: (480-16-8)/2 = 228, (244-16-8)/2 = 110.
        assert_eq!(tiles[0].w, 228);
        assert_eq!(tiles[0].h, 110);
    }

    #[test]
    fn hit_zone_contains_is_half_open() {
        let z = HitZone::new(10, 10, 20, 20, HitAction::Back);
        assert!(z.contains(10, 10));
        assert!(z.contains(29, 29));
        assert!(!z.contains(30, 30));
        assert!(!z.contains(9, 9));
    }

    #[test]
    fn blank_panel_is_full_size() {
        let c = blank_panel(&DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(c.height(), PANEL_H);
    }

    /// The pairing QR opens Mission Control's `/pair` route with the code in
    /// `code=` (the parameter that route reads) and the node's LAN name.
    #[test]
    fn the_pair_deep_link_targets_the_pair_route() {
        let mut ctx = PageContext::default();
        assert_eq!(
            ctx.pair_deep_link("7yt-fc7"),
            "https://command.altnautica.com/pair?code=7YTFC7"
        );
        ctx.mission_control_url = Some("https://mc.example.com/".into());
        ctx.lan_host = Some("ados-9f2c1a.local".into());
        assert_eq!(
            ctx.pair_deep_link("7YTFC7"),
            "https://mc.example.com/pair?code=7YTFC7&host=ados-9f2c1a.local"
        );
    }

    /// The setup URL uses a reach name the agent reported, never a literal
    /// hostname; with none known there is no URL.
    #[test]
    fn the_setup_url_uses_only_a_reported_reach_name() {
        let mut ctx = PageContext::default();
        assert_eq!(ctx.setup_url(), None);
        ctx.network.ap_ip = Some("192.168.4.1".into());
        assert_eq!(ctx.setup_url().as_deref(), Some("http://192.168.4.1:8080"));
        ctx.lan_host = Some("ados-9f2c1a.local".into());
        assert_eq!(
            ctx.setup_url().as_deref(),
            Some("http://ados-9f2c1a.local:8080")
        );
    }
}
