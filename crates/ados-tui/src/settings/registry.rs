//! The settings page registry: every page in sidebar order, its group, the
//! profiles it shows on, and what its body is. Plugin pages are appended at
//! runtime by the Settings tab.

use serde_json::Value;

use super::page::BespokeId;

/// A sidebar group heading, in sidebar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    ThisTerminal,
    Identity,
    LinkNetwork,
    VideoVision,
    CloudRemote,
    AccessSecurity,
    SystemSafety,
    Extensions,
    AllSettings,
}

impl Group {
    pub const ALL: [Group; 9] = [
        Group::ThisTerminal,
        Group::Identity,
        Group::LinkNetwork,
        Group::VideoVision,
        Group::CloudRemote,
        Group::AccessSecurity,
        Group::SystemSafety,
        Group::Extensions,
        Group::AllSettings,
    ];

    /// The uppercase heading text.
    pub fn label(self) -> &'static str {
        match self {
            Group::ThisTerminal => "THIS TERMINAL",
            Group::Identity => "IDENTITY",
            Group::LinkNetwork => "LINK & NETWORK",
            Group::VideoVision => "VIDEO & VISION",
            Group::CloudRemote => "CLOUD & REMOTE",
            Group::AccessSecurity => "ACCESS & SECURITY",
            Group::SystemSafety => "SYSTEM & SAFETY",
            Group::Extensions => "EXTENSIONS",
            Group::AllSettings => "ALL SETTINGS",
        }
    }
}

/// The two pages of local terminal preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefsPage {
    Appearance,
    Behaviour,
}

/// What a page body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageBody {
    /// Rows of agent config keys, written with `PUT /api/config`.
    Config(&'static [Row]),
    /// A page with its own layout and actions.
    Bespoke(BespokeId),
    /// Local terminal preferences (no agent call).
    Prefs(PrefsPage),
    /// Every leaf of the config schema.
    AllSettings,
    /// One installed extension's config parameters.
    Plugin,
}

/// One config row of a `Config` page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// A sub-section header drawn above the row when it differs from the
    /// previous row's.
    pub section: &'static str,
    pub key: &'static str,
    /// Empty: the schema title, else the key's last segment title-cased.
    pub label: &'static str,
    /// Empty: the schema description.
    pub help: &'static str,
    /// A change asks for confirmation first.
    pub confirm: bool,
    /// Integer choices that override the schema kind (cycled like an enum).
    pub choices: &'static [i64],
}

const fn row(section: &'static str, key: &'static str, label: &'static str, help: &'static str) -> Row {
    Row {
        section,
        key,
        label,
        help,
        confirm: false,
        choices: &[],
    }
}

const fn confirmed(section: &'static str, key: &'static str, label: &'static str, help: &'static str) -> Row {
    Row {
        section,
        key,
        label,
        help,
        confirm: true,
        choices: &[],
    }
}

/// One settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// Route id, palette id and flash-key prefix. Bespoke pages use
    /// [`BespokeId::id`].
    pub id: &'static str,
    pub group: Group,
    pub title: &'static str,
    /// Profiles the page shows on; empty = every profile.
    pub profiles: &'static [&'static str],
    pub body: PageBody,
}

const ALL: &[&str] = &[];
const DRONE_VISOR: &[&str] = &["drone", "visor"];
const DRONE_GS: &[&str] = &["drone", "ground_station"];
const DRONE: &[&str] = &["drone"];
const VISOR: &[&str] = &["visor"];
const GS: &[&str] = &["ground_station"];

const fn page(id: &'static str, group: Group, title: &'static str, profiles: &'static [&'static str], body: PageBody) -> Page {
    Page {
        id,
        group,
        title,
        profiles,
        body,
    }
}

const fn bespoke(b: BespokeId, id: &'static str, group: Group, title: &'static str, profiles: &'static [&'static str]) -> Page {
    page(id, group, title, profiles, PageBody::Bespoke(b))
}

const DEVICE_ROWS: &[Row] = &[row("", "agent.name", "", "")];

const TRANSPORT: &str = "Flight-controller transport";
const IDENTITY: &str = "Agent identity";
const IDENTITY_HINT: &str = "The system and component IDs the agent presents on the MAVLink bus. Leave these at the defaults unless a routing collision forces a change.";

const MAVLINK_ROWS: &[Row] = &[
    confirmed(TRANSPORT, "mavlink.source", "Flight-controller transport", "How the agent reaches the flight controller. Auto probes candidate serial ports; the explicit modes use the port and baud below."),
    confirmed(TRANSPORT, "mavlink.serial_port", "Serial port", "The serial device path (e.g. /dev/ttyACM0). Only used when the transport is Serial."),
    confirmed(TRANSPORT, "mavlink.baud_rate", "Baud rate", "Serial line speed for the Serial transport (e.g. 57600, 115200, 921600)."),
    confirmed(IDENTITY, "mavlink.system_id", "System ID", IDENTITY_HINT),
    row(IDENTITY, "mavlink.component_id", "Component ID", IDENTITY_HINT),
];

/// The serial-port row's label and help when the transport is UDP or TCP.
pub const NETWORK_TARGET: (&str, &str) = (
    "Network target",
    "A udp:host:port or tcp:host:port target. Only used when the transport is UDP or TCP.",
);

const CAMERA_ROWS: &[Row] = &[
    Row {
        choices: &[0, 90, 180, 270],
        ..row("", "video.camera.rotation", "Rotation", "Clockwise image rotation applied by the encoder before the frame is compressed and sent to the live feed. The pipeline restarts to apply a change (the stretch is applied per-leg from the same config key).")
    },
    row("", "video.camera.hflip", "Horizontal flip", "Mirror the image left-to-right. Useful for a camera mounted facing backwards."),
    row("", "video.camera.vflip", "Vertical flip", "Mirror the image top-to-bottom. Combine with rotation to correct upside-down mounts."),
];

const VISION_ROWS: &[Row] = &[
    row("", "vision.enabled", "On-board detection engine", "Run the detection engine on this node so it can publish detections locally. Off by default; a fresh node runs no vision until this is on."),
    row("", "vision.backend", "Inference backend", "Which accelerator the engine runs on. Auto picks the best the board advertises; the explicit choices force a specific one."),
    row("", "vision.confidence_threshold", "Confidence threshold", "Minimum detection confidence to report, from 0 to 1. Higher drops weak detections."),
    row("", "vision.models_cache_max_mb", "Model cache cap (MB)", "How much disk the downloaded-model cache may use before older models are pruned."),
];

const SWARM_ROWS: &[Row] = &[
    row("", "swarm.enabled", "Swarm participation", "Persist this node's intent to join a swarm. No runtime consumer acts on it yet."),
    row("", "swarm.role", "Role", "The node's intended swarm role (e.g. auto)."),
    row("", "swarm.default_formation", "Default formation", "The formation preset the node starts with."),
    row("", "swarm.default_spacing", "Default spacing (metres)", "The default inter-node spacing a formation should target."),
];

const MONITORING: &str = "Monitoring";
const THRESHOLDS: &str = "Thresholds";

const BATTERY_ROWS: &[Row] = &[
    row(MONITORING, "battery.enabled", "Battery health monitoring", "Evaluate cell, sag, temperature and time-to-reserve rules on every pack and log each alert."),
    row(THRESHOLDS, "battery.low_cell_mv", "Low cell (mV)", "A cell below this raises a warning. Must stay above the critical level."),
    row(THRESHOLDS, "battery.critical_cell_mv", "Critical cell (mV)", "A cell below this raises a critical alert. Must stay below the low level."),
    row(THRESHOLDS, "battery.cell_divergence_mv", "Cell divergence (mV)", "Spread between the highest and lowest cell that flags an unbalanced pack."),
    row(THRESHOLDS, "battery.voltage_drop_mv_per_s", "Voltage sag (mV/s)", "Pack voltage falling faster than this between samples flags a sag."),
    row(THRESHOLDS, "battery.temp_spike_dc_per_s", "Temperature spike (0.1 °C/s)", "Temperature rising faster than this flags a spike. 50 means 5.0 °C/s."),
    row(THRESHOLDS, "battery.predictive_window_s", "Prediction window (s)", "How much recent history the time-to-reserve estimate averages over."),
    row(THRESHOLDS, "battery.reserve_percent", "Reserve (%)", "The remaining charge the time-to-reserve estimate counts down to."),
];

const WIFI_HEAL: &str = "Onboard Wi-Fi self-heal";
const USB_HEAL: &str = "Camera USB recovery";

const SELF_HEAL_ROWS: &[Row] = &[
    row(WIFI_HEAL, "network.wifi_selfheal.enabled", "Re-associate a dead onboard Wi-Fi link", "On by default. Leave it on unless you are debugging the Wi-Fi stack by hand."),
    row(WIFI_HEAL, "network.wifi_selfheal.fail_threshold", "Fail threshold (checks)", "Consecutive failing checks before a re-association fires. A single failing check can be a momentarily-busy gateway."),
    row(WIFI_HEAL, "network.wifi_selfheal.cooldown_s", "Cooldown (seconds)", "Quiet period after a heal, per connection, so a re-association in progress is never re-fired on (anti-flap)."),
    row(USB_HEAL, "video.usb_recovery.enabled", "Recover a wedged camera", "Rebind or re-enable a camera that did not enumerate at boot. Bounded by an attempt budget and cooldown; never restarts the video service."),
    row(USB_HEAL, "video.usb_recovery.allow_ppps", "Per-port power re-enable", "Allow a clean per-port power cycle on an external hub that exposes per-port power switching."),
];

use Group as G;

/// Every static page, in sidebar order.
pub const PAGES: &[Page] = &[
    // THIS TERMINAL
    page("appearance", G::ThisTerminal, "Appearance", ALL, PageBody::Prefs(PrefsPage::Appearance)),
    page("behaviour", G::ThisTerminal, "Behaviour", ALL, PageBody::Prefs(PrefsPage::Behaviour)),
    bespoke(BespokeId::Targets, "agents", G::ThisTerminal, "Agents", ALL),
    // IDENTITY
    bespoke(BespokeId::Profile, "profile", G::Identity, "Profile", ALL),
    page("device", G::Identity, "Device", ALL, PageBody::Config(DEVICE_ROWS)),
    // LINK & NETWORK
    bespoke(BespokeId::Wifi, "wifi", G::LinkNetwork, "Wi-Fi", ALL),
    bespoke(BespokeId::Hotspot, "hotspot", G::LinkNetwork, "Hotspot", ALL),
    bespoke(BespokeId::Cellular, "cellular", G::LinkNetwork, "Cellular", ALL),
    bespoke(BespokeId::MacPin, "mac_pin", G::LinkNetwork, "MAC pinning", ALL),
    bespoke(BespokeId::Discovery, "discovery", G::LinkNetwork, "Discovery", ALL),
    page("mavlink", G::LinkNetwork, "MAVLink", DRONE_VISOR, PageBody::Config(MAVLINK_ROWS)),
    bespoke(BespokeId::Radio, "radio", G::LinkNetwork, "Radio", DRONE_GS),
    // VIDEO & VISION
    page("camera", G::VideoVision, "Camera", DRONE_VISOR, PageBody::Config(CAMERA_ROWS)),
    bespoke(BespokeId::VideoLink, "video_link", G::VideoVision, "Video link", DRONE),
    page("vision", G::VideoVision, "Vision", DRONE_VISOR, PageBody::Config(VISION_ROWS)),
    bespoke(BespokeId::Sensors, "sensors", G::VideoVision, "Sensors", DRONE_VISOR),
    bespoke(BespokeId::Capture, "capture", G::VideoVision, "Capture", VISOR),
    page("swarm", G::VideoVision, "Swarm", DRONE, PageBody::Config(SWARM_ROWS)),
    // CLOUD & REMOTE
    bespoke(BespokeId::Cloud, "cloud", G::CloudRemote, "Cloud", ALL),
    bespoke(BespokeId::RemoteAccess, "remote_access", G::CloudRemote, "Remote access", ALL),
    // ACCESS & SECURITY
    bespoke(BespokeId::Pairing, "pairing", G::AccessSecurity, "Pairing", ALL),
    bespoke(BespokeId::Security, "security", G::AccessSecurity, "Security", ALL),
    bespoke(BespokeId::Mcp, "mcp", G::AccessSecurity, "MCP", ALL),
    // SYSTEM & SAFETY
    bespoke(BespokeId::Region, "region", G::SystemSafety, "Region", DRONE_GS),
    page("battery", G::SystemSafety, "Battery", DRONE_VISOR, PageBody::Config(BATTERY_ROWS)),
    page("self_heal", G::SystemSafety, "Self-heal", ALL, PageBody::Config(SELF_HEAL_ROWS)),
    bespoke(BespokeId::Display, "display", G::SystemSafety, "Display", GS),
    bespoke(BespokeId::Advanced, "advanced", G::SystemSafety, "Advanced", ALL),
    // ALL SETTINGS
    page("all_settings", G::AllSettings, "All settings", ALL, PageBody::AllSettings),
];

/// The page id prefix of a plugin page (`plugin:<plugin id>`).
pub const PLUGIN_PREFIX: &str = "plugin:";

/// Whether a page shows on `profile`. An unreported or undecided profile
/// shows every page, matching the web dashboard; gated pages are hidden, not
/// greyed.
pub fn visible(page: &Page, profile: &str) -> bool {
    page.profiles.is_empty()
        || !matches!(profile, "drone" | "ground_station" | "visor")
        || page.profiles.contains(&profile)
}

/// The bespoke page whose route id is `id`.
pub fn bespoke_by_id(id: &str) -> Option<BespokeId> {
    PAGES.iter().find_map(|p| match p.body {
        PageBody::Bespoke(b) if b.id() == id => Some(b),
        _ => None,
    })
}

/// The curated row for a config key, on any page.
pub fn row_for(key: &str) -> Option<&'static Row> {
    PAGES.iter().find_map(|p| match p.body {
        PageBody::Config(rows) => rows.iter().find(|r| r.key == key),
        _ => None,
    })
}

/// Whether writing `value` to `key` asks for confirmation first. The MAVLink
/// WebSocket auth enforcement only confirms when it is being turned on.
pub fn needs_confirm(key: &str, value: &Value) -> bool {
    if key == "mavlink.ws_proxy_enforce_auth" {
        return value.as_bool() == Some(true);
    }
    row_for(key).is_some_and(|r| r.confirm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn by_id(id: &str) -> Option<&'static Page> {
        PAGES.iter().find(|p| p.id == id)
    }

    #[test]
    fn pages_are_grouped_in_sidebar_order() {
        let mut last = 0;
        for p in PAGES {
            let g = Group::ALL.iter().position(|g| *g == p.group).unwrap();
            assert!(g >= last, "{} is out of group order", p.id);
            last = g;
        }
    }

    #[test]
    fn bespoke_page_ids_match_their_route_ids() {
        for p in PAGES {
            if let PageBody::Bespoke(b) = p.body {
                assert_eq!(p.id, b.id());
                assert_eq!(bespoke_by_id(p.id), Some(b));
            }
        }
        let mut ids: Vec<&str> = PAGES.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), PAGES.len(), "page ids are unique");
    }

    #[test]
    fn profile_gates_hide_pages() {
        let radio = by_id("radio").unwrap();
        assert!(visible(radio, "drone") && visible(radio, "ground_station"));
        assert!(!visible(radio, "visor"));
        assert!(visible(radio, "auto") && visible(radio, ""));
        let display = by_id("display").unwrap();
        assert!(!visible(display, "drone") && visible(display, "ground_station"));
    }

    #[test]
    fn confirm_rows() {
        assert!(needs_confirm("mavlink.baud_rate", &json!(115200)));
        assert!(needs_confirm("mavlink.system_id", &json!(2)));
        assert!(!needs_confirm("mavlink.component_id", &json!(191)));
        assert!(needs_confirm("mavlink.ws_proxy_enforce_auth", &json!(true)));
        assert!(!needs_confirm("mavlink.ws_proxy_enforce_auth", &json!(false)));
        assert!(!needs_confirm("discovery.mdns_enabled", &json!(false)));
    }
}
