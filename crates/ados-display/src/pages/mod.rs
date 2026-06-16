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

/// A rectangular touch target on a page, in page-local content coordinates
/// (origin at the top-left of the 480x244 content region).
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

/// WFB radio-link telemetry. Mirrors the `link` state block plus the `/api/wfb`
/// snapshot the radio-link surfaces read: signal, throughput, FEC, channel,
/// and the watchdog counters the diagnostics surfaces care about.
#[derive(Debug, Clone, Default)]
pub struct LinkCtx {
    /// Link-layer connection state (`connected`, `connecting`, `unpaired`, …).
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

/// Radio topology — the power-supply path that drives the brownout badge.
#[derive(Debug, Clone, Default)]
pub struct RadioCtx {
    /// `host_vbus`, `powered_hub`, or `external_5v`.
    pub topology: Option<String>,
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
}

/// Live flight-controller telemetry from the dashboard snapshot's `fc` block.
#[derive(Debug, Clone, Default)]
pub struct FcCtx {
    pub vehicle: Option<String>,
    pub mode: Option<String>,
    pub armed: bool,
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
    pub paired: bool,
    /// Six-character pair code (cloud or local source).
    pub pair_code: Option<String>,
    /// The cloud-relay alias for the pair code.
    pub pairing_code: Option<String>,
    pub latency_ms: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub broadcasting: bool,
    pub pair_url: Option<String>,
    /// MQTT transport state (`connected`, `connecting`, …).
    pub mqtt_state: Option<String>,
    /// HTTP heartbeat state (`ok`, `connecting`, …).
    pub http_state: Option<String>,
    /// The paired drone id once the cloud relay has bound one.
    pub drone_id: Option<String>,
}

/// The local pairing code + pairing-window state (`pairing` state block).
#[derive(Debug, Clone, Default)]
pub struct PairingCtx {
    pub code: Option<String>,
    pub pair_url: Option<String>,
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
    pub up: bool,
    pub partition: bool,
    pub peer_count: i64,
    pub selected_gateway: Option<String>,
    pub mesh_id: Option<String>,
    pub peers: Vec<MeshPeer>,
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
    pub ap_ip: Option<String>,
    pub usb_ip: Option<String>,
    /// `eth`, `wifi`, `cellular`, or `none`.
    pub uplink_type: Option<String>,
    pub uplink_reachable: bool,
    /// mDNS hostname used to build the setup-wizard URL.
    pub mdns_host: Option<String>,
    pub hotspot_ssid: Option<String>,
    pub hotspot_enabled: bool,
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

/// System metrics (`system` state block) for the top bar + footer + diagnostics.
#[derive(Debug, Clone, Default)]
pub struct SystemCtx {
    pub cpu_pct: Option<f64>,
    pub ram_used_mb: Option<f64>,
    pub ram_total_mb: Option<f64>,
    pub temp_c: Option<f64>,
    pub uptime_seconds: Option<f64>,
    pub agent_version: Option<String>,
    /// 60-sample CPU trend for the footer sparkline.
    pub cpu_history: Vec<Option<f64>>,
    /// 60-sample temperature trend for the footer sparkline.
    pub temp_history: Vec<Option<f64>>,
}

/// One hardware-check row (`hardware_check.items`) for the early-life checklist.
#[derive(Debug, Clone, Default)]
pub struct HardwareItem {
    pub id: Option<String>,
    pub label: Option<String>,
    /// `ok`, `warning`, `missing`, or `unknown`.
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
    pub active: bool,
    pub recording: bool,
    pub fps: Option<f64>,
    pub latency_ms: Option<f64>,
    pub bitrate_kbps: Option<f64>,
    /// Whether mediamtx reports the path ready.
    pub mediamtx_ready: bool,
    /// mediamtx inbound throughput in kilobits per second.
    pub mediamtx_inbound_kbps: Option<f64>,
    pub camera_label: Option<String>,
    pub camera_count: i64,
}

/// The system-health sidecar (`/run/ados/health.json`) for the link-stats
/// system band.
#[derive(Debug, Clone, Default)]
pub struct HealthCtx {
    pub cpu_percent: Option<f64>,
    pub memory_percent: Option<f64>,
    pub disk_percent: Option<f64>,
    pub temperature: Option<f64>,
}

/// Device identity (`/api/v1/setup/status` + HAL board detect) for the about
/// and diagnostics surfaces.
#[derive(Debug, Clone, Default)]
pub struct DeviceCtx {
    pub device_id: Option<String>,
    pub device_name: Option<String>,
    pub version: Option<String>,
    pub board_name: Option<String>,
    pub mac_eth0: Option<String>,
    pub mac_wlan0: Option<String>,
    pub primary_ip: Option<String>,
    pub primary_mac: Option<String>,
    /// Release-time build stamp (`/etc/ados/build.txt`).
    pub build_stamp: Option<String>,
}

/// One settings-list row's resolved label + current value for the settings
/// surface.
#[derive(Debug, Clone, Default)]
pub struct SettingsRow {
    pub id: String,
    pub label: String,
    /// `default`, `toggle`, or `action`.
    pub variant: String,
    /// The right-column value text (for default rows).
    pub value: Option<String>,
    /// The on/off state (for toggle rows).
    pub toggle_on: Option<bool>,
}

/// Display / radio / network / logging / theme settings snapshot for the
/// settings surface.
#[derive(Debug, Clone, Default)]
pub struct SettingsCtx {
    pub rows: Vec<SettingsRow>,
    /// Count of changes pending a reboot (drives the reboot banner).
    pub pending_reboot_count: i64,
    pub theme: Option<String>,
    pub logging_level: Option<String>,
    pub display_rotation_degrees: Option<i64>,
    pub server_mode: Option<String>,
}

/// The diagnostics agent-log buffer (last journal lines) for the diagnostics
/// surface.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsCtx {
    /// Recent agent log lines, oldest first.
    pub agent_logs: Vec<String>,
    /// Scroll offset into the log buffer, in pixels.
    pub log_scroll_offset: i64,
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
    pub link: LinkCtx,
    pub radio: RadioCtx,
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
    pub health: HealthCtx,
    pub device: DeviceCtx,
    pub settings: SettingsCtx,
    pub diagnostics: DiagnosticsCtx,
}

// ── page trait ──────────────────────────────────────────────────────

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

    /// Preferred redraw cadence in hertz.
    fn refresh_hz(&self) -> f32;

    /// Paint the full 480x320 panel for this page (chrome included).
    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas;

    /// Return the page's active hit zones in page-local content coordinates.
    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone>;
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
}
