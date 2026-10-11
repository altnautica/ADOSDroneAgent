//! Loading the `mavlink:` slice and resolving the agent profile.

use super::*;

/// Resolve the agent profile as `drone` / `ground-station` / the raw value.
///
/// Order: the config's `agent.profile`, then the on-disk profile marker, then
/// `drone`. An absent, empty, `auto` or unrecognised value collapses to `drone`,
/// which is the same resolution the process supervisor applies when it decides
/// which units to start at all — so a rig cannot end up with this service
/// running under one notion of its profile and gating features on another.
///
/// Mirrors the sibling bearer services' profile gate rather than sharing it:
/// each service resolves its own, and converging the several copies is a
/// codebase-wide cleanup of its own.
pub fn resolve_profile(config_profile: Option<&str>, profile_conf: &Path) -> String {
    let from_config = config_profile.map(str::trim).filter(|v| !v.is_empty());
    if let Some(v) = from_config {
        match v {
            "drone" => return "drone".to_string(),
            "ground_station" | "ground-station" => return "ground-station".to_string(),
            // Anything else (including `auto`) falls through to the marker.
            _ => {}
        }
    }
    if let Ok(text) = std::fs::read_to_string(profile_conf) {
        for line in text.lines() {
            let stripped = line.trim();
            if stripped.is_empty() || stripped.starts_with('#') {
                continue;
            }
            let Some(value) = stripped
                .strip_prefix("profile:")
                .or_else(|| stripped.strip_prefix("profile="))
            else {
                continue;
            };
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            return match value {
                "ground_station" | "ground-station" => "ground-station".to_string(),
                "workstation" => "workstation".to_string(),
                "compute" => "compute".to_string(),
                _ => "drone".to_string(),
            };
        }
    }
    default_profile()
}

impl MavlinkConfig {
    /// Load from the canonical path (or `ADOS_CONFIG` when set). A missing or
    /// unreadable file yields the defaults, matching the Python loader. This is the
    /// real startup entry, so it also publishes the config-status sidecar: a
    /// malformed config surfaces on the remote Health view, not just in the log.
    pub fn load() -> Self {
        let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.to_string());
        let (config, error) = Self::load_reporting(Path::new(&path));
        ados_config::write_config_status("mavlink", error.as_deref());
        config
    }

    /// Load from an explicit path (testable). Does NOT publish the config-status
    /// sidecar — only the real [`load`](Self::load) startup path does, so tests
    /// never write to the run dir.
    pub fn load_from(path: &Path) -> Self {
        Self::load_reporting(path).0
    }

    /// Load from an explicit path, also returning the parse-error message so the
    /// startup path can publish it. `None` on success or a missing/unreadable
    /// file; `Some(msg)` on a present-but-malformed file.
    ///
    /// Reads the `mavlink:` section plus the foreign `radio.crsf` fields the
    /// router gates on: the pinned CRSF/ELRS serial device (excluded from FC
    /// port candidacy in `crsf_rc` mode) and the lane's enable/mode/transport
    /// (which resolve the MAVLink-over-ELRS ingest source in `mavlink` mode).
    pub(super) fn load_reporting(path: &Path) -> (Self, Option<String>) {
        #[derive(Debug, Default, Deserialize)]
        struct AgentSection {
            // Nullable on disk like the CRSF strings below, and commonly left
            // on `auto`, which resolves from the on-disk profile marker.
            #[serde(default)]
            profile: Option<String>,
            #[serde(default)]
            device_id: Option<String>,
        }
        #[derive(Debug, Default, Deserialize)]
        struct CrsfSection {
            #[serde(default)]
            enabled: bool,
            // Nullable on disk (the config model writes `device: null` for
            // "no pin"); a bare String would fail the parse on the explicit
            // null and take the whole mavlink section down with it. The other
            // string fields are Options for the same reason.
            #[serde(default)]
            device: Option<String>,
            #[serde(default)]
            mode: Option<String>,
            #[serde(default)]
            mavlink_transport: Option<String>,
            #[serde(default)]
            mavlink_command_enabled: bool,
        }
        // Each slice loads on its own off the schema-merged config, so a
        // malformed foreign block costs only its own fields; the first error is
        // the one surfaced.
        let mut error: Option<String> = None;
        let mut cfg = match ados_config::schema::load_slice::<MavlinkConfig>(path, "/mavlink") {
            Ok(c) => c,
            Err(e) => {
                error = Some(e);
                MavlinkConfig::default()
            }
        };
        let crsf = match ados_config::schema::load_slice::<CrsfSection>(path, "/radio/crsf") {
            Ok(c) => c,
            Err(e) => {
                error.get_or_insert(e);
                CrsfSection::default()
            }
        };
        let agent = match ados_config::schema::load_slice::<AgentSection>(path, "/agent") {
            Ok(a) => a,
            Err(e) => {
                error.get_or_insert(e);
                AgentSection::default()
            }
        };
        cfg.crsf_device = crsf.device.as_deref().unwrap_or("").trim().to_string();
        cfg.crsf_enabled = crsf.enabled;
        // Absent enum-ish fields read their model defaults, matching the lane
        // service's own parse of the same block.
        cfg.crsf_mode = crsf.mode.as_deref().unwrap_or("crsf_rc").trim().to_string();
        cfg.crsf_mavlink_transport = crsf
            .mavlink_transport
            .as_deref()
            .unwrap_or("serial")
            .trim()
            .to_string();
        cfg.crsf_mavlink_command_enabled = crsf.mavlink_command_enabled;
        let profile_conf =
            std::env::var("ADOS_PROFILE_CONF").unwrap_or_else(|_| PROFILE_CONF.to_string());
        cfg.profile = resolve_profile(agent.profile.as_deref(), Path::new(&profile_conf));
        cfg.device_id = ados_protocol::identity::device_id(agent.device_id.as_deref());
        (cfg, error)
    }

    /// Whether this node is a drone, and so runs the drone-only paths.
    pub fn is_drone(&self) -> bool {
        self.profile == "drone"
    }

    /// Whether this node is a ground station, and so runs the ground-only
    /// paths. Note this is not the negation of [`Self::is_drone`]: the other
    /// profiles are neither, and must not pick up either side's paths.
    pub fn is_ground_station(&self) -> bool {
        self.profile == "ground-station"
    }

    /// Whether the PIC injector gate runs on this node: the flag is on AND the
    /// profile is a ground station, the only profile that runs the arbiter.
    pub fn injector_arbitration_active(&self) -> bool {
        self.injector_arbitration
            && ados_config::normalize_profile(Some(self.profile.as_str())) == "ground-station"
    }

    /// The `injector_arbitration` state extra: `active` (gate built),
    /// `inactive_no_local_arbiter` (flag on, but this profile runs no arbiter),
    /// or `off` (flag off).
    pub fn injector_arbitration_state(&self) -> &'static str {
        if !self.injector_arbitration {
            "off"
        } else if self.injector_arbitration_active() {
            "active"
        } else {
            "inactive_no_local_arbiter"
        }
    }

    /// Resolve the MAVLink-over-ELRS ingest source from the `radio.crsf`
    /// block: `Some` only when the lane is opted in, its mode is `mavlink`,
    /// AND the carrier resolves — the backpack carrier always does (the
    /// listen port is fixed), the serial carrier only with a pinned device.
    /// Matches the lane service's parse posture on an unknown
    /// `mavlink_transport` value: it degrades to the serial carrier.
    pub fn crsf_mavlink_source(&self) -> Option<CrsfMavlinkSource<'_>> {
        if !self.crsf_enabled || self.crsf_mode != "mavlink" {
            return None;
        }
        if self.crsf_mavlink_transport == "backpack_wifi" {
            return Some(CrsfMavlinkSource::BackpackUdp {
                port: BACKPACK_UDP_PORT,
            });
        }
        let device = self.crsf_device.trim();
        if device.is_empty() {
            None
        } else {
            Some(CrsfMavlinkSource::Serial { device })
        }
    }

    /// The first enabled WebSocket endpoint, if any (the proxy's bind host and
    /// port).
    pub fn websocket_endpoint(&self) -> Option<&EndpointConfig> {
        self.endpoints
            .iter()
            .find(|e| e.enabled && e.kind == "websocket")
    }

    /// The first enabled WebSocket endpoint port, if any (the proxy bind port).
    pub fn websocket_port(&self) -> Option<u16> {
        self.websocket_endpoint().map(|e| e.port)
    }
}
