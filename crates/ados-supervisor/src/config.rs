//! Agent profile / role / video resolution.
//!
//! Mirrors the Python wire-contract helpers (`core/profile.py` +
//! `ground_station/role_manager.get_current_role`): the agent stores the
//! profile in underscore form on disk and exposes the hyphenated wire form,
//! and the active ground-station role comes from an on-disk sentinel so it
//! survives a stale in-memory config during a transition.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";
pub const MESH_ROLE_PATH: &str = "/etc/ados/mesh/role";
pub const RUN_DIR: &str = "/run/ados";

/// The ground-station mesh roles, in advertised order. The one definition every
/// role consumer reads (the transition, the boot apply, the REST role routes).
pub const VALID_ROLES: [&str; 3] = ["direct", "relay", "receiver"];

/// Only the fields the supervisor reads, as a slice of the effective config
/// (schema defaults overlaid with `config.yaml`). serde ignores everything else,
/// so the large operator config stays untouched here. Every default equals the
/// schema default (`config_slice_conforms`).
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct RawConfig {
    agent: AgentSection,
    video: VideoSection,
    vision: VisionSection,
    ground_station: GroundStationSection,
    server: ServerSection,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
struct AgentSection {
    /// `agent.profile`: `auto` (default) resolves from `profile.conf`.
    profile: String,
    /// `agent.headless`: when true, the supervisor boots only the lean KEEP set
    /// (the Rust core), blocking FastAPI / cloud / health / GS units. Absent or
    /// false → the full agent. The zero-Python flight profile.
    headless: bool,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            profile: "auto".to_string(),
            headless: false,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
struct VideoSection {
    /// `video.mode`: any value but `disabled` enables video (default `wfb`).
    mode: String,
    camera: CameraSection,
}

impl Default for VideoSection {
    fn default() -> Self {
        Self {
            mode: "wfb".to_string(),
            camera: CameraSection::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
struct CameraSection {
    /// `video.camera.source`: a device hint (`csi`/`usb`/`ip`) or an explicit
    /// `rtsp://…` / `http://…` network URL. The supervisor only reads it to
    /// decide whether to start the video service without a local camera node.
    source: String,
}

impl Default for CameraSection {
    fn default() -> Self {
        Self {
            source: "csi".to_string(),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct VisionSection {
    /// `vision.enabled`: opt-in for the onboard vision engine. The engine reads
    /// the top-level `vision:` block (not `video:`); the supervisor mirrors that
    /// key so it can decide whether to start `ados-vision` at boot instead of
    /// leaving a configured engine silently dark.
    enabled: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
struct GroundStationSection {
    role: String,
}

impl Default for GroundStationSection {
    fn default() -> Self {
        Self {
            role: "direct".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
struct ServerSection {
    /// `server.mode`: `local` (default) reaches the agent only over the LAN;
    /// `cloud` / `self_hosted` enable the cloud relay.
    mode: String,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            mode: "local".to_string(),
        }
    }
}

/// The resolved agent identity the orchestrator gates on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// Wire-contract profile: `"drone"`, `"ground-station"`, `"workstation"`,
    /// `"compute"` or `"visor"`.
    pub profile_wire: String,
    /// Active ground-station role at boot (`direct`/`relay`/`receiver`), or
    /// `None` on a drone. This is the snapshot taken at config load. The
    /// service gate does NOT read this — it re-reads the on-disk sentinel
    /// every check so a runtime role switch is honored without restarting
    /// the supervisor (see `live_role`). Kept for display / boot reporting.
    pub role: Option<String>,
    /// `video.mode` is not `"disabled"` (an absent key reads as the schema default `wfb`).
    pub video_enabled: bool,
    /// An explicit network camera source (`video.camera.source` = `rtsp://…`
    /// / `http://…`), or `None` for the local-camera path. When set, the
    /// hardware-detect gate starts `ados-video` even with no local `/dev/video`
    /// node — the IP-camera drone case, where the feed is a remote URL.
    pub video_network_source: Option<String>,
    /// `vision.enabled` is set. The hardware-detect pass starts `ados-vision`
    /// (drone-gated) when this is true and a camera source exists, so a
    /// configured engine actually comes up instead of staying silently dark —
    /// the engine self-gates on the same flag as a backstop. Without this the
    /// unit is never started (it is not in any install-time enable set), so a
    /// `vision.enabled: true` config would never bring the vision pipeline up.
    pub vision_enabled: bool,
    /// What the top-level `cameras[]` list declares: captured cameras start
    /// `ados-vision` whatever `vision.enabled` says (the engine is their only
    /// owner), and either a captured or a network entry is a video source with
    /// no local camera node.
    pub declared_cameras: ados_config::cameras::DeclaredCameras,
    /// `server.mode` is a cloud posture (`cloud` / `self_hosted`), i.e. the
    /// cloud relay is configured. Default `local` → false. The WFB auto-pair
    /// loop only fails over to the cloud relay when this is true; a local-first
    /// rig keeps retrying the local bind forever instead of giving up.
    pub cloud_relay_enabled: bool,
    /// `ground_station.role` from config (default `direct`); the role to apply on boot.
    pub configured_gs_role: String,
    /// `agent.headless` is true: boot only the lean headless KEEP set. The
    /// service gate blocks every non-KEEP unit when this is set, so a zero-Python
    /// flight node runs just the Rust core (MAVLink / camera / radio / HTTP
    /// front). A boot-time flag — the supervisor reads it once at config load.
    pub headless_mode: bool,
    /// Whether the durable logging + telemetry store is wanted on this node:
    /// `logging.store.enabled` is set AND the Python-fallback pin marker is
    /// absent. Read through `ados_config::log_store` so the supervisor, the
    /// installer and the daemon cannot drift on one toggle.
    ///
    /// The store ships OFF, and when it is off the installer masks the unit, so
    /// the `ados-logd` registry row gates on this: without the gate the
    /// supervisor would start-fail a masked unit on every tick of every node
    /// that has the store turned off.
    pub log_store_enabled: bool,
    /// Where the on-disk role sentinel lives. The role gate re-reads this on
    /// every check so an operator-driven role switch (which flips the sentinel
    /// and stops/starts units without restarting this process) is reflected in
    /// self-healing immediately. Defaults to `MESH_ROLE_PATH`; overridable in
    /// tests.
    pub mesh_role_path: PathBuf,
    /// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`): the control
    /// socket, the per-role state files a role transition clears, and the
    /// mesh-event journal it appends to.
    pub run_dir: PathBuf,
    /// The agent config file this was resolved from. The monitor watches its
    /// modification time: a unit that exited because this node had nothing for
    /// it to run is started again when the config changes, since the change may
    /// be the one that gives it work (a fleet slot, an enabled feature).
    pub config_yaml: PathBuf,
}

impl AgentConfig {
    /// Resolve using the canonical on-disk locations. This is the real startup
    /// entry, so it also publishes the config-status sidecar: a malformed config
    /// surfaces on the remote Health view, not just in the log.
    ///
    /// The three canonical paths honour the same `ADOS_CONFIG` / `ADOS_PROFILE_CONF`
    /// / `ADOS_MESH_ROLE` overrides the sibling daemons (`ados-control`,
    /// `ados-plugin-host`) already read, so a rootless per-user install (macOS
    /// workstation under `$HOME/.ados`) resolves its `workstation` profile from
    /// the same config file the rest of the node reads. Unset (the SBC default) →
    /// the `/etc/ados` FHS paths, unchanged.
    pub fn load() -> Self {
        let config_yaml = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.to_string());
        let profile_conf = ados_config::profile_conf_path();
        let mesh_role =
            std::env::var("ADOS_MESH_ROLE").unwrap_or_else(|_| MESH_ROLE_PATH.to_string());
        let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| RUN_DIR.to_string());
        Self::load_from_inner(
            Path::new(&config_yaml),
            &profile_conf,
            Path::new(&mesh_role),
            Path::new(&run_dir),
            true,
        )
    }

    /// Resolve from explicit paths (testable). Does NOT publish the config-status
    /// sidecar — only the real [`load`](Self::load) startup path does, so tests
    /// never write to the run dir.
    pub fn load_from(config_yaml: &Path, profile_conf: &Path, mesh_role: &Path) -> Self {
        Self::load_from_inner(
            config_yaml,
            profile_conf,
            mesh_role,
            Path::new(RUN_DIR),
            false,
        )
    }

    /// Shared resolver. When `publish` is set (the real startup path) the parse
    /// error (or its absence) is written to the config-status sidecar.
    fn load_from_inner(
        config_yaml: &Path,
        profile_conf: &Path,
        mesh_role: &Path,
        run_dir: &Path,
        publish: bool,
    ) -> Self {
        let (raw, config_error) = read_raw_config(config_yaml);
        if publish {
            ados_config::write_config_status("supervisor", config_error.as_deref());
        }

        let profile_wire = ados_config::resolve_profile(Some(&raw.agent.profile), profile_conf);

        let role = if profile_wire == "ground-station" {
            Some(read_current_role(mesh_role))
        } else {
            None
        };

        let video_enabled = raw.video.mode != "disabled";

        let video_network_source = Some(raw.video.camera.source.trim())
            .filter(|s| s.starts_with("rtsp://") || s.starts_with("http://"))
            .map(str::to_string);

        let vision_enabled = raw.vision.enabled;
        let declared_cameras = std::fs::read_to_string(config_yaml)
            .map(|t| ados_config::cameras::declared_cameras_from(&t))
            .unwrap_or_default();

        let cloud_relay_enabled = matches!(raw.server.mode.as_str(), "cloud" | "self_hosted");

        let configured_gs_role = Some(raw.ground_station.role)
            .filter(|r| VALID_ROLES.contains(&r.as_str()))
            .unwrap_or_else(|| "direct".to_string());

        let headless_mode = raw.agent.headless;

        AgentConfig {
            profile_wire,
            role,
            video_enabled,
            video_network_source,
            vision_enabled,
            declared_cameras,
            cloud_relay_enabled,
            configured_gs_role,
            headless_mode,
            mesh_role_path: mesh_role.to_path_buf(),
            run_dir: run_dir.to_path_buf(),
            log_store_enabled: log_store_wanted(config_yaml),
            config_yaml: config_yaml.to_path_buf(),
        }
    }

    /// The active ground-station role read fresh from the on-disk sentinel. Falls back
    /// to `direct` when the sentinel is missing/unreadable/unknown. This is the source
    /// of truth for role gating: an operator role switch flips the sentinel and
    /// stops/starts units WITHOUT restarting this process, so a cached boot-time role
    /// would leave self-healing acting on the wrong unit set.
    pub fn live_role(&self) -> String {
        read_current_role(&self.mesh_role_path)
    }

    /// The underscore form used to compare against a service's `profile_gate`.
    pub fn profile_gate(&self) -> String {
        self.profile_wire.replace('-', "_")
    }

    /// True when the resolved profile is the ground station. The boot role
    /// apply and the hardware pass key off this, so a config that leaves the
    /// profile to `profile.conf` (`auto`) or uses the hyphen spelling boots its
    /// ground-station role exactly like one that names it literally.
    pub fn is_ground_station(&self) -> bool {
        self.profile_wire == "ground-station"
    }

    /// True when the resolved profile is the handheld capture rig.
    pub fn is_visor(&self) -> bool {
        self.profile_wire == "visor"
    }
}

/// Read the supervisor's slice of the effective config, returning the error
/// message alongside it so the startup path can surface it on a config-status
/// sidecar. `None` on success or a missing file (a fresh node is not a fault,
/// and reads as the schema defaults); `Some(msg)` on a present-but-malformed
/// file, which falls back to the defaults.
fn read_raw_config(path: &Path) -> (RawConfig, Option<String>) {
    match ados_config::schema::load_slice::<RawConfig>(path, "") {
        Ok(raw) => (raw, None),
        Err(e) => (RawConfig::default(), Some(e)),
    }
}

/// Whether the logging store is wanted on this node. The config key is read
/// through the shared `ados_config::log_store` gate — the same one the
/// installer's enable/mask reconcile and the daemon's own self-gate read — so
/// the three cannot drift.
pub fn log_store_wanted(config_yaml: &Path) -> bool {
    ados_config::log_store::read_gate(config_yaml).enabled
}

/// Read the on-disk role sentinel. Falls back to `direct` if missing,
/// unreadable, or holding an unknown value.
pub fn read_current_role(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let v = text.trim();
            if VALID_ROLES.contains(&v) {
                v.to_string()
            } else {
                "direct".to_string()
            }
        }
        Err(_) => "direct".to_string(),
    }
}

#[cfg(test)]
mod tests;
