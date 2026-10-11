//! The `radio.crsf` config slice this service reads, plus the profile gate.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::sources::ChannelSourceMode;

/// Default RC frame cadence when the config does not pin one: the standard
/// mid rate — comfortably inside a USB-serial RC module bridge's budget
/// (26 bytes × 150 Hz ≈ 31 kbit/s on a 420 kbaud line), and the full-duplex
/// bridge owns the half-duplex bus turnaround, so the host only has to hold
/// the frame cadence, not a microsecond turnaround budget.
pub const DEFAULT_PACKET_RATE_HZ: u16 = 150;
/// Ceiling on the configured cadence (the protocol's fastest standard rate).
pub const MAX_PACKET_RATE_HZ: u16 = 500;

/// The module's operating band class (`radio.crsf.band`). A target for the
/// module, surfaced on status; the module's own parameter system owns the
/// actual band change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BandMode {
    /// A dual-band (Gemini-class) module running both links.
    #[default]
    Dual,
    /// Sub-GHz only.
    Band900,
    /// 2.4 GHz only.
    Band2p4,
}

impl BandMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "dual" => Some(Self::Dual),
            "900" => Some(Self::Band900),
            "2p4" => Some(Self::Band2p4),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dual => "dual",
            Self::Band900 => "900",
            Self::Band2p4 => "2p4",
        }
    }
}

/// What the attached module carries (`radio.crsf.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaneMode {
    /// The RC channel lane this service transmits (the default).
    #[default]
    CrsfRc,
    /// The module runs its native MAVLink mode: a MAVLink byte carrier (the
    /// module firmware owns the CRSF air protocol internally — no CRSF parsing
    /// host-side). The MAVLink router ingests the carrier as its FC source —
    /// the pinned device at the fixed MAVLink-mode baud when
    /// `mavlink_transport` is `serial`, or a UDP listen on the conventional
    /// MAVLink port for `backpack_wifi` — so this lane never opens the port
    /// and stands by at `ready` with the mode reported. The router reads
    /// telemetry up but keeps the host->FC command-down direction gated closed
    /// by default (`radio.crsf.mavlink_command_enabled`), so the source is
    /// telemetry-only until that marker is set for a bench-validated command
    /// lane.
    Mavlink,
    /// A generic serial data pipe through the module.
    Airport,
}

impl LaneMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "crsf_rc" => Some(Self::CrsfRc),
            "mavlink" => Some(Self::Mavlink),
            "airport" => Some(Self::Airport),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CrsfRc => "crsf_rc",
            Self::Mavlink => "mavlink",
            Self::Airport => "airport",
        }
    }
}

/// The carrier for `mode: mavlink` (`radio.crsf.mavlink_transport`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MavlinkTransport {
    /// The module's USB-serial port (the default).
    #[default]
    Serial,
    /// The module's WiFi-backpack UDP bridge.
    BackpackWifi,
}

impl MavlinkTransport {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "serial" => Some(Self::Serial),
            "backpack_wifi" => Some(Self::BackpackWifi),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Serial => "serial",
            Self::BackpackWifi => "backpack_wifi",
        }
    }
}

/// This node's part in an RC relay chain (`radio.crsf.relay_role`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RelayRole {
    /// Not relaying (the default).
    #[default]
    None,
    /// A pure CRSF repeater.
    Repeater,
    /// An agent relay driving the ELRS last mile.
    AgentLastMile,
}

impl RelayRole {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "none" => Some(Self::None),
            "repeater" => Some(Self::Repeater),
            "agent_last_mile" => Some(Self::AgentLastMile),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Repeater => "repeater",
            Self::AgentLastMile => "agent_last_mile",
        }
    }

    /// The sidecar's `relay_role` field: participation is reported only when
    /// this node actually relays — `none` reads as a JSON null, never a
    /// fabricated role label.
    pub fn sidecar_str(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            other => Some(other.as_str()),
        }
    }
}

/// The `radio.crsf` block of `/etc/ados/config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrsfLaneConfig {
    /// The lane opt-in. Absent/false ⇒ the service idles harmlessly.
    pub enabled: bool,
    /// The serial device the RC transmitter module is pinned to
    /// (e.g. `/dev/ttyUSB0`), normalized from the config's nullable field.
    /// Empty ⇒ no pin, the lane is unconfigured.
    pub device: String,
    /// The module's operating band class target.
    pub band: BandMode,
    /// RC frame cadence, frames per second, clamped to 1..=500.
    pub packet_rate_hz: u16,
    /// Requested conducted TX power for the module. `None` (the default)
    /// leaves the module at its own default — never a fabricated figure; the
    /// measured power comes back on link-statistics telemetry.
    pub tx_power_dbm: Option<i64>,
    /// What the attached module carries. The RC transmitter runs only in
    /// `crsf_rc`; in the other modes this service idles (the port belongs to
    /// the MAVLink router / the data pipe).
    pub mode: LaneMode,
    /// Which source feeds the transmitted channels (`hid` | `inject` |
    /// `hybrid`). Defaults to `hid`: the handset/gamepad path only — the
    /// programmatic lane must be opted into.
    pub channel_source: ChannelSourceMode,
    /// The carrier for `mode: mavlink`.
    pub mavlink_transport: MavlinkTransport,
    /// This node's part in an RC relay chain.
    pub relay_role: RelayRole,
    /// Whether the host->FC command-down direction is opened for a
    /// MAVLink-over-ELRS source (`radio.crsf.mavlink_command_enabled`). Off by
    /// default: a MAVLink-over-ELRS source is telemetry-only until this is set
    /// for a bench-validated command lane. The MAVLink router owns the carrier
    /// and the actual writer gate in `mode: mavlink`; the lane reads the same
    /// flag so it can report the gate honestly on its own status surface (see
    /// [`Self::fc_command_down_gated`]).
    pub mavlink_command_enabled: bool,
}

impl Default for CrsfLaneConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device: String::new(),
            band: BandMode::Dual,
            packet_rate_hz: DEFAULT_PACKET_RATE_HZ,
            tx_power_dbm: None,
            mode: LaneMode::CrsfRc,
            channel_source: ChannelSourceMode::Hid,
            mavlink_transport: MavlinkTransport::Serial,
            relay_role: RelayRole::None,
            mavlink_command_enabled: false,
        }
    }
}

/// The `radio.crsf` slice as it sits in the config. Its `Default` is the
/// schema default, so the slice conformance test holds it to the schema.
#[derive(Debug, Deserialize, Serialize)]
struct CrsfSection {
    #[serde(default)]
    enabled: bool,
    // Nullable on disk (the Python model writes `device: null` for "no pin"),
    // so an Option is load-bearing: a bare String would fail the whole parse
    // on the explicit null.
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    band: Option<String>,
    #[serde(default)]
    packet_rate_hz: Option<u16>,
    #[serde(default)]
    tx_power_dbm: Option<i64>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    channel_source: Option<String>,
    #[serde(default)]
    mavlink_transport: Option<String>,
    #[serde(default)]
    relay_role: Option<String>,
    #[serde(default)]
    mavlink_command_enabled: bool,
}

impl Default for CrsfSection {
    fn default() -> Self {
        Self {
            enabled: false,
            device: None,
            band: Some("dual".to_string()),
            packet_rate_hz: Some(DEFAULT_PACKET_RATE_HZ),
            tx_power_dbm: None,
            mode: Some("crsf_rc".to_string()),
            channel_source: Some("hid".to_string()),
            mavlink_transport: Some("serial".to_string()),
            relay_role: Some("none".to_string()),
            mavlink_command_enabled: false,
        }
    }
}

/// Parse an optional enum-ish string field, warning (once, at load) and
/// falling back to the default on an unknown value — a typo must degrade
/// loudly to a safe default, never crash-loop the lane.
fn parse_or_default<T: Default>(
    field: &'static str,
    raw: Option<&str>,
    parse: impl Fn(&str) -> Option<T>,
) -> T {
    match raw {
        None => T::default(),
        Some(s) => parse(s).unwrap_or_else(|| {
            tracing::warn!(
                field,
                configured = s,
                "crsf config value unknown; using default"
            );
            T::default()
        }),
    }
}

impl CrsfLaneConfig {
    /// Load from the agent config file. A missing file is the fresh-node case
    /// (the schema defaults); a malformed file or slice is reported loudly
    /// through the shared slice loader and publishes to the config-status
    /// sidecar, then degrades to defaults rather than crash-looping.
    pub fn load_from(path: &Path) -> Self {
        let (crsf, cfg_err) = match ados_config::schema::load_slice(path, "/radio/crsf") {
            Ok(crsf) => (crsf, None),
            Err(e) => (CrsfSection::default(), Some(e)),
        };
        ados_config::write_config_status("crsf", cfg_err.as_deref());
        Self::from_section(crsf)
    }

    fn from_section(crsf: CrsfSection) -> Self {
        let packet_rate_hz = match crsf.packet_rate_hz {
            None => DEFAULT_PACKET_RATE_HZ,
            Some(r) if (1..=MAX_PACKET_RATE_HZ).contains(&r) => r,
            Some(r) => {
                let clamped = r.clamp(1, MAX_PACKET_RATE_HZ);
                tracing::warn!(
                    configured = r,
                    clamped,
                    "crsf packet rate out of range; clamped"
                );
                clamped
            }
        };
        let channel_source = match crsf.channel_source.as_deref() {
            None => ChannelSourceMode::Hid,
            Some(s) => ChannelSourceMode::parse(s).unwrap_or_else(|| {
                tracing::warn!(
                    configured = s,
                    "crsf channel_source unknown; defaulting to hid"
                );
                ChannelSourceMode::Hid
            }),
        };
        Self {
            enabled: crsf.enabled,
            device: crsf.device.as_deref().unwrap_or("").trim().to_string(),
            band: parse_or_default("band", crsf.band.as_deref(), BandMode::parse),
            packet_rate_hz,
            tx_power_dbm: crsf.tx_power_dbm,
            mode: parse_or_default("mode", crsf.mode.as_deref(), LaneMode::parse),
            channel_source,
            mavlink_transport: parse_or_default(
                "mavlink_transport",
                crsf.mavlink_transport.as_deref(),
                MavlinkTransport::parse,
            ),
            relay_role: parse_or_default(
                "relay_role",
                crsf.relay_role.as_deref(),
                RelayRole::parse,
            ),
            mavlink_command_enabled: crsf.mavlink_command_enabled,
        }
    }

    /// Whether a MAVLink-over-ELRS command source exists on this lane and, if so,
    /// whether its host->FC command-down direction is gated closed. Matches the
    /// MAVLink router's `command_down_gated` predicate (both read the same
    /// `radio.crsf` block), so a consumer reading ONLY this lane's status can see
    /// the ELRS command path's gate without the top-level FC status surface.
    ///
    /// Tri-state, honest: `None` when NO MAVLink-over-ELRS source exists (any
    /// non-`mavlink` mode, the lane disabled, or `mode: mavlink` with no
    /// resolvable carrier) — there is no command path to gate; `Some(true)` when
    /// a source exists and the gate is CLOSED (telemetry-only, the default);
    /// `Some(false)` only when a source exists and the gate is OPEN
    /// (`mavlink_command_enabled`, a bidirectional command lane).
    pub fn fc_command_down_gated(&self) -> Option<bool> {
        if !self.enabled || self.mode != LaneMode::Mavlink {
            return None;
        }
        // Resolve the carrier exactly as the router's crsf_mavlink_source does:
        // the backpack UDP listen always resolves; the serial carrier only with a
        // pinned device. No carrier => no source => nothing to gate.
        let source_resolves =
            self.mavlink_transport == MavlinkTransport::BackpackWifi || !self.device.is_empty();
        source_resolves.then_some(!self.mavlink_command_enabled)
    }
}

/// True when the agent profile resolves to `drone` — the CRSF transmitter
/// lane is ground-side (the ground node drives the RC module), so on a drone
/// this binary idles. Reads `agent.profile` from the config file, falling
/// back to the profile marker file. Defensive: the unit is already
/// profile-gated at install time.
pub fn profile_is_drone(config_path: &Path, profile_conf: &Path) -> bool {
    #[derive(Debug, Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        agent: AgentSection,
    }
    #[derive(Debug, Default, Deserialize)]
    struct AgentSection {
        #[serde(default)]
        profile: Option<String>,
    }
    let cfg_profile = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_norway::from_str::<Raw>(&t).ok())
        .and_then(|r| r.agent.profile);
    match cfg_profile.as_deref() {
        Some("drone") => return true,
        Some("ground_station") | Some("ground-station") => return false,
        _ => {} // empty/auto/missing → consult the profile marker
    }
    if let Ok(text) = std::fs::read_to_string(profile_conf) {
        for line in text.lines() {
            let s = line.trim();
            if let Some(v) = s
                .strip_prefix("profile:")
                .or_else(|| s.strip_prefix("profile="))
            {
                let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                return v == "drone";
            }
        }
    }
    false
}

#[cfg(test)]
mod tests;
