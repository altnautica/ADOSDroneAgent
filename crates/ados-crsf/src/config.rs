//! The `radio.crsf` config slice this service reads, plus the profile gate.

use std::path::Path;

use serde::Deserialize;

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

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    radio: RadioSection,
}

#[derive(Debug, Default, Deserialize)]
struct RadioSection {
    #[serde(default)]
    crsf: CrsfSection,
}

#[derive(Debug, Default, Deserialize)]
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
    /// (quiet defaults); a malformed one is reported loudly through the
    /// shared config loader and publishes to the config-status sidecar, then
    /// degrades to defaults rather than crash-looping.
    pub fn load_from(path: &Path) -> Self {
        let (raw, cfg_err) = ados_config::load_yaml_reporting::<RawConfig>(path, "crsf");
        ados_config::write_config_status("crsf", cfg_err.as_deref());
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Self {
        let crsf = raw.radio.crsf;
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
    /// whether its host->FC command-down direction is gated closed. Mirrors the
    /// MAVLink router's `command_down_gated` predicate — both read the same
    /// `radio.crsf` block — so a consumer reading ONLY this lane's status can see
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
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn missing_file_reads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = CrsfLaneConfig::load_from(&dir.path().join("nope.yaml"));
        assert_eq!(cfg, CrsfLaneConfig::default());
        assert!(!cfg.enabled);
        assert_eq!(cfg.packet_rate_hz, DEFAULT_PACKET_RATE_HZ);
        assert_eq!(cfg.band, BandMode::Dual);
        assert_eq!(cfg.tx_power_dbm, None);
        assert_eq!(cfg.mode, LaneMode::CrsfRc);
        assert_eq!(cfg.channel_source, ChannelSourceMode::Hid);
        assert_eq!(cfg.mavlink_transport, MavlinkTransport::Serial);
        assert_eq!(cfg.relay_role, RelayRole::None);
        assert!(!cfg.mavlink_command_enabled);
    }

    #[test]
    fn full_block_reads_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "radio:\n  crsf:\n    enabled: true\n    device: \" /dev/ttyUSB0 \"\n    band: \"900\"\n    packet_rate_hz: 250\n    tx_power_dbm: 20\n    mode: mavlink\n    channel_source: hybrid\n    mavlink_transport: backpack_wifi\n    relay_role: repeater\n    mavlink_command_enabled: true\n",
        );
        let cfg = CrsfLaneConfig::load_from(&path);
        assert!(cfg.enabled);
        assert_eq!(cfg.device, "/dev/ttyUSB0");
        assert_eq!(cfg.band, BandMode::Band900);
        assert_eq!(cfg.packet_rate_hz, 250);
        assert_eq!(cfg.tx_power_dbm, Some(20));
        assert_eq!(cfg.mode, LaneMode::Mavlink);
        assert_eq!(cfg.channel_source, ChannelSourceMode::Hybrid);
        assert_eq!(cfg.mavlink_transport, MavlinkTransport::BackpackWifi);
        assert_eq!(cfg.relay_role, RelayRole::Repeater);
        assert!(cfg.mavlink_command_enabled);
    }

    #[test]
    fn fc_command_down_gated_is_tri_state_and_mirrors_the_router() {
        // No MAVLink-over-ELRS source -> None (nothing to gate). The RC channel
        // lane, an unconfigured lane, and a disabled lane all read absent.
        let rc = CrsfLaneConfig {
            enabled: true,
            device: "/dev/ttyUSB0".to_string(),
            mode: LaneMode::CrsfRc,
            ..Default::default()
        };
        assert_eq!(rc.fc_command_down_gated(), None);
        let disabled = CrsfLaneConfig {
            enabled: false,
            device: "/dev/ttyUSB0".to_string(),
            mode: LaneMode::Mavlink,
            ..Default::default()
        };
        assert_eq!(disabled.fc_command_down_gated(), None);
        // mode: mavlink with a serial carrier but no pinned device does not
        // resolve a source (matching the router) -> None.
        let no_device = CrsfLaneConfig {
            enabled: true,
            device: String::new(),
            mode: LaneMode::Mavlink,
            mavlink_transport: MavlinkTransport::Serial,
            ..Default::default()
        };
        assert_eq!(no_device.fc_command_down_gated(), None);

        // A resolved serial source, gate closed by default -> Some(true).
        let serial_gated = CrsfLaneConfig {
            enabled: true,
            device: "/dev/ttyUSB0".to_string(),
            mode: LaneMode::Mavlink,
            mavlink_transport: MavlinkTransport::Serial,
            mavlink_command_enabled: false,
            ..Default::default()
        };
        assert_eq!(serial_gated.fc_command_down_gated(), Some(true));
        // Same source with the command marker set -> the gate is open, Some(false).
        let serial_open = CrsfLaneConfig {
            mavlink_command_enabled: true,
            ..serial_gated.clone()
        };
        assert_eq!(serial_open.fc_command_down_gated(), Some(false));
        // The backpack carrier always resolves, even with no pinned device.
        let backpack = CrsfLaneConfig {
            enabled: true,
            device: String::new(),
            mode: LaneMode::Mavlink,
            mavlink_transport: MavlinkTransport::BackpackWifi,
            ..Default::default()
        };
        assert_eq!(backpack.fc_command_down_gated(), Some(true));
    }

    #[test]
    fn null_device_and_power_read_as_unset() {
        // The Python model writes explicit nulls for the unset nullable
        // fields; the parse must read them as absent, never fail.
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "radio:\n  crsf:\n    enabled: true\n    device: null\n    tx_power_dbm: null\n",
        );
        let cfg = CrsfLaneConfig::load_from(&path);
        assert!(cfg.enabled);
        assert!(cfg.device.is_empty());
        assert_eq!(cfg.tx_power_dbm, None);
    }

    #[test]
    fn out_of_range_rate_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "radio:\n  crsf:\n    enabled: true\n    packet_rate_hz: 2000\n",
        );
        assert_eq!(
            CrsfLaneConfig::load_from(&path).packet_rate_hz,
            MAX_PACKET_RATE_HZ
        );
        let path0 = write_file(
            &dir,
            "config0.yaml",
            "radio:\n  crsf:\n    packet_rate_hz: 0\n",
        );
        assert_eq!(CrsfLaneConfig::load_from(&path0).packet_rate_hz, 1);
    }

    #[test]
    fn channel_source_parses_with_a_hid_default() {
        let dir = tempfile::tempdir().unwrap();
        for (yaml, expected) in [
            (
                "radio:\n  crsf:\n    enabled: true\n",
                ChannelSourceMode::Hid,
            ),
            (
                "radio:\n  crsf:\n    channel_source: hid\n",
                ChannelSourceMode::Hid,
            ),
            (
                "radio:\n  crsf:\n    channel_source: inject\n",
                ChannelSourceMode::Inject,
            ),
            (
                "radio:\n  crsf:\n    channel_source: hybrid\n",
                ChannelSourceMode::Hybrid,
            ),
            (
                "radio:\n  crsf:\n    channel_source: bogus\n",
                ChannelSourceMode::Hid,
            ),
        ] {
            let path = write_file(&dir, "config.yaml", yaml);
            assert_eq!(
                CrsfLaneConfig::load_from(&path).channel_source,
                expected,
                "yaml: {yaml:?}"
            );
        }
    }

    #[test]
    fn unknown_enum_values_degrade_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "radio:\n  crsf:\n    band: 5ghz\n    mode: teleport\n    mavlink_transport: carrier_pigeon\n    relay_role: chief\n",
        );
        let cfg = CrsfLaneConfig::load_from(&path);
        assert_eq!(cfg.band, BandMode::Dual);
        assert_eq!(cfg.mode, LaneMode::CrsfRc);
        assert_eq!(cfg.mavlink_transport, MavlinkTransport::Serial);
        assert_eq!(cfg.relay_role, RelayRole::None);
    }

    #[test]
    fn enum_round_trips_cover_every_variant() {
        for band in [BandMode::Dual, BandMode::Band900, BandMode::Band2p4] {
            assert_eq!(BandMode::parse(band.as_str()), Some(band));
        }
        for mode in [LaneMode::CrsfRc, LaneMode::Mavlink, LaneMode::Airport] {
            assert_eq!(LaneMode::parse(mode.as_str()), Some(mode));
        }
        for t in [MavlinkTransport::Serial, MavlinkTransport::BackpackWifi] {
            assert_eq!(MavlinkTransport::parse(t.as_str()), Some(t));
        }
        for r in [
            RelayRole::None,
            RelayRole::Repeater,
            RelayRole::AgentLastMile,
        ] {
            assert_eq!(RelayRole::parse(r.as_str()), Some(r));
        }
        // The sidecar projection reports participation only: `none` is null.
        assert_eq!(RelayRole::None.sidecar_str(), None);
        assert_eq!(RelayRole::Repeater.sidecar_str(), Some("repeater"));
        assert_eq!(
            RelayRole::AgentLastMile.sidecar_str(),
            Some("agent_last_mile")
        );
    }

    #[test]
    fn absent_crsf_block_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "config.yaml", "video:\n  enabled: true\n");
        let cfg = CrsfLaneConfig::load_from(&path);
        assert!(!cfg.enabled);
        assert!(cfg.device.is_empty());
    }

    #[test]
    fn profile_gate_reads_config_then_marker() {
        let dir = tempfile::tempdir().unwrap();
        let drone = write_file(&dir, "drone.yaml", "agent:\n  profile: drone\n");
        let gs = write_file(&dir, "gs.yaml", "agent:\n  profile: ground_station\n");
        let unset = write_file(&dir, "unset.yaml", "agent: {}\n");
        let marker_drone = write_file(&dir, "profile-drone.conf", "profile=drone\n");
        let marker_gs = write_file(&dir, "profile-gs.conf", "profile: ground_station\n");
        let missing = dir.path().join("missing");

        assert!(profile_is_drone(&drone, &missing));
        assert!(!profile_is_drone(&gs, &marker_drone), "config wins");
        assert!(profile_is_drone(&unset, &marker_drone));
        assert!(!profile_is_drone(&unset, &marker_gs));
        assert!(!profile_is_drone(&missing, &missing), "unknown never gates");
    }
}
