//! The page render context: [`PageContext`] and the per-block structs it
//! nests for link, drone, flight-controller, cloud, pairing and role state.

use super::status_ctx::{
    DeviceCtx, DiagnosticsCtx, HardwareItem, HoppingCtx, MeshCtx, NetworkCtx, SystemCtx, UplinkCtx,
    VideoCtx,
};
use super::{gs_ctx, reading, sensing_ctx};
use crate::graphics::palette::Palette;

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

    /// Throughput in megabits per second, the tiles' display unit.
    pub fn bitrate_mbps(&self) -> Option<f64> {
        self.bitrate_kbps.map(|kbps| kbps / 1000.0)
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
    pub sensing: sensing_ctx::SensingCtx,
    /// `GET /api/v1/gnss`; absent when no GNSS service is fitted.
    pub gnss: reading::Reading<gs_ctx::GnssCtx>,
    /// `GET /api/v1/lora` + beacons; absent when no LoRa service is fitted.
    pub lora: reading::Reading<gs_ctx::LoraCtx>,
    /// `GET /api/v1/radio/adapters`; absent when no receive plane reports.
    pub radio_adapters: reading::Reading<gs_ctx::RadioAdaptersCtx>,
}

/// The hosted Mission Control the agent advertises when the operator has not
/// configured another (the setup service's `mission_control_url` default).
pub const DEFAULT_MISSION_CONTROL_URL: &str = "https://command.altnautica.com";

impl PageContext {
    /// The pair code every pairing surface shows: the local pairing window's
    /// code, else the cloud one. `None` when neither is known.
    pub fn pair_code(&self) -> Option<&str> {
        self.pairing
            .code
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.cloud.pair_code.as_deref().filter(|s| !s.is_empty()))
    }

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
