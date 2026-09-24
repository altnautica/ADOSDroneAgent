//! Agent profile / role / video resolution.
//!
//! Mirrors the Python wire-contract helpers (`core/profile.py` +
//! `ground_station/role_manager.get_current_role`): the agent stores the
//! profile in underscore form on disk and exposes the hyphenated wire form,
//! and the active ground-station role comes from an on-disk sentinel so it
//! survives a stale in-memory config during a transition.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";
pub const MESH_ROLE_PATH: &str = "/etc/ados/mesh/role";
pub const RUN_DIR: &str = "/run/ados";

/// The ground-station mesh roles, in advertised order. The one definition every
/// role consumer reads (the transition, the boot apply, the REST role routes).
pub const VALID_ROLES: [&str; 3] = ["direct", "relay", "receiver"];

/// Only the fields the supervisor reads. serde ignores everything else in
/// `config.yaml`, so the large operator config stays untouched here.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    agent: AgentSection,
    #[serde(default)]
    video: VideoSection,
    #[serde(default)]
    vision: VisionSection,
    #[serde(default)]
    ground_station: GroundStationSection,
    #[serde(default)]
    server: ServerSection,
}

#[derive(Debug, Default, Deserialize)]
struct AgentSection {
    #[serde(default)]
    profile: Option<String>,
    /// `agent.headless`: when true, the supervisor boots only the lean KEEP set
    /// (the Rust core), blocking FastAPI / cloud / health / GS units. Absent or
    /// false → the full agent. The zero-Python flight profile.
    #[serde(default)]
    headless: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct VideoSection {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    camera: CameraSection,
}

#[derive(Debug, Default, Deserialize)]
struct CameraSection {
    /// `video.camera.source`: a device hint (`csi`/`usb`/`ip`) or an explicit
    /// `rtsp://…` / `http://…` network URL. The supervisor only reads it to
    /// decide whether to start the video service without a local camera node.
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct VisionSection {
    /// `vision.enabled`: opt-in for the onboard vision engine. The engine reads
    /// the top-level `vision:` block (not `video:`); the supervisor mirrors that
    /// key so it can decide whether to start `ados-vision` at boot instead of
    /// leaving a configured engine silently dark.
    #[serde(default)]
    enabled: bool,
}

#[derive(Debug, Default, Deserialize)]
struct GroundStationSection {
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ServerSection {
    /// `server.mode`: `local` (default) reaches the agent only over the LAN;
    /// `cloud` / `self_hosted` enable the cloud relay.
    #[serde(default)]
    mode: Option<String>,
}

/// The resolved agent identity the orchestrator gates on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// Wire-contract profile: `"drone"` or `"ground-station"`.
    pub profile_wire: String,
    /// Active ground-station role at boot (`direct`/`relay`/`receiver`), or
    /// `None` on a drone. This is the snapshot taken at config load. The
    /// service gate does NOT read this — it re-reads the on-disk sentinel
    /// every check so a runtime role switch is honored without restarting
    /// the supervisor (see `live_role`). Kept for display / boot reporting.
    pub role: Option<String>,
    /// `video.mode` is set and not `"disabled"`.
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

        let profile_wire = ados_config::resolve_profile(raw.agent.profile.as_deref(), profile_conf);

        let role = if profile_wire == "ground-station" {
            Some(read_current_role(mesh_role))
        } else {
            None
        };

        let video_enabled = raw
            .video
            .mode
            .as_deref()
            .map(|m| m != "disabled")
            .unwrap_or(false);

        let video_network_source = raw
            .video
            .camera
            .source
            .as_deref()
            .map(str::trim)
            .filter(|s| s.starts_with("rtsp://") || s.starts_with("http://"))
            .map(str::to_string);

        let vision_enabled = raw.vision.enabled;

        let cloud_relay_enabled = matches!(
            raw.server.mode.as_deref(),
            Some("cloud") | Some("self_hosted")
        );

        let configured_gs_role = raw
            .ground_station
            .role
            .filter(|r| VALID_ROLES.contains(&r.as_str()))
            .unwrap_or_else(|| "direct".to_string());

        let headless_mode = raw.agent.headless.unwrap_or(false);

        AgentConfig {
            profile_wire,
            role,
            video_enabled,
            video_network_source,
            vision_enabled,
            cloud_relay_enabled,
            configured_gs_role,
            headless_mode,
            mesh_role_path: mesh_role.to_path_buf(),
            run_dir: run_dir.to_path_buf(),
            log_store_enabled: log_store_wanted(config_yaml),
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
}

/// Read + parse the raw agent config, returning the parse-error message
/// alongside the config so the startup path can surface it on a config-status
/// sidecar. `None` on success or a missing/unreadable file (a fresh node is not a
/// fault); `Some(msg)` on a present-but-malformed file.
fn read_raw_config(path: &Path) -> (RawConfig, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (RawConfig::default(), None);
    };
    ados_config::yaml_reporting(&text, "supervisor")
}

/// The name of the marker that pins the durable store off regardless of the
/// config key. Written next to `config.yaml`, so it follows an `ADOS_CONFIG`
/// override and stays with the config it belongs to.
pub const LOGD_PIN_OFF_MARKER: &str = "logd-python-fallback";

/// Whether the logging store is wanted on this node. The config key is read
/// through the shared `ados_config::log_store` gate — the same one the
/// installer's enable/mask reconcile and the daemon's own self-gate read — so
/// the three cannot drift, and the pin marker is honoured because the installer
/// MASKS the unit when it is present (a start against a masked unit only
/// produces failures).
pub fn log_store_wanted(config_yaml: &Path) -> bool {
    let pinned_off = config_yaml
        .parent()
        .map(|dir| dir.join(LOGD_PIN_OFF_MARKER).exists())
        .unwrap_or(false);
    !pinned_off && ados_config::log_store::read_gate(config_yaml).enabled
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
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn explicit_config_profile_wins() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(
            &cfg,
            "agent:\n  profile: ground_station\nvideo:\n  mode: auto\n",
        );
        let pc = dir.path().join("profile.conf"); // not read when config is explicit
        let role = dir.path().join("mesh/role");
        write(&role, "relay\n");
        let ac = AgentConfig::load_from(&cfg, &pc, &role);
        assert_eq!(ac.profile_wire, "ground-station");
        assert_eq!(ac.role.as_deref(), Some("relay"));
        assert!(ac.video_enabled);
        assert_eq!(ac.profile_gate(), "ground_station");
        assert!(ac.is_ground_station());
    }

    #[test]
    fn cloud_relay_enabled_follows_server_mode() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("profile.conf");
        let role = dir.path().join("mesh/role");
        let load = |body: &str| {
            let cfg = dir.path().join("config.yaml");
            write(&cfg, body);
            AgentConfig::load_from(&cfg, &pc, &role).cloud_relay_enabled
        };
        // Local-first default (absent or explicit `local`) keeps the relay off.
        assert!(!load("agent:\n  profile: drone\n"));
        assert!(!load("server:\n  mode: local\n"));
        // Cloud / self-hosted postures enable it.
        assert!(load("server:\n  mode: cloud\n"));
        assert!(load("server:\n  mode: self_hosted\n"));
        // An unknown mode stays off (local-first).
        assert!(!load("server:\n  mode: weird\n"));
    }

    #[test]
    fn video_network_source_parsed_only_for_url() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("profile.conf");
        let role = dir.path().join("mesh/role");
        let load = |body: &str| {
            let cfg = dir.path().join("config.yaml");
            write(&cfg, body);
            AgentConfig::load_from(&cfg, &pc, &role).video_network_source
        };
        // A local-camera hint or device path is not a network source.
        assert_eq!(load("video:\n  camera:\n    source: csi\n"), None);
        assert_eq!(load("video:\n  camera:\n    source: /dev/video0\n"), None);
        assert_eq!(load("agent:\n  profile: drone\n"), None);
        // An explicit rtsp/http URL is picked up (trimmed).
        assert_eq!(
            load("video:\n  camera:\n    source: rtsp://host:8554/scene\n"),
            Some("rtsp://host:8554/scene".to_string())
        );
        assert_eq!(
            load("video:\n  camera:\n    source: \"  http://cam/stream  \"\n"),
            Some("http://cam/stream".to_string())
        );
    }

    #[test]
    fn vision_enabled_follows_the_vision_block() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("profile.conf");
        let role = dir.path().join("mesh/role");
        let load = |body: &str| {
            let cfg = dir.path().join("config.yaml");
            write(&cfg, body);
            AgentConfig::load_from(&cfg, &pc, &role).vision_enabled
        };
        // No vision block → disabled (the safe default on a rig without vision).
        assert!(!load("agent:\n  profile: drone\n"));
        // Explicit false → disabled.
        assert!(!load("vision:\n  enabled: false\n"));
        // Explicit true → enabled, matching the key the engine itself reads.
        assert!(load("vision:\n  enabled: true\n"));
    }

    #[test]
    fn auto_falls_back_to_profile_conf() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        write(&cfg, "agent:\n  profile: auto\nvideo:\n  mode: disabled\n");
        let pc = dir.path().join("profile.conf");
        write(
            &pc,
            "# header\nprofile: ground-station\nmesh_capable: true\n",
        );
        let role = dir.path().join("mesh/role");
        let ac = AgentConfig::load_from(&cfg, &pc, &role);
        assert_eq!(ac.profile_wire, "ground-station");
        // role sentinel missing → direct
        assert_eq!(ac.role.as_deref(), Some("direct"));
        // video.mode disabled → not enabled
        assert!(!ac.video_enabled);
        // The boot role-apply and receive-plane start key off the RESOLVED
        // profile, so an `auto` config on a ground station boots its role.
        assert!(ac.is_ground_station());
    }

    #[test]
    fn missing_everything_defaults_to_drone() {
        let dir = tempfile::tempdir().unwrap();
        let ac = AgentConfig::load_from(
            &dir.path().join("nope.yaml"),
            &dir.path().join("nope.conf"),
            &dir.path().join("nope.role"),
        );
        assert_eq!(ac.profile_wire, "drone");
        assert_eq!(ac.role, None);
        assert!(!ac.video_enabled);
        assert_eq!(ac.configured_gs_role, "direct");
        // The full agent is the default: headless must be opt-in.
        assert!(!ac.headless_mode);
    }

    #[test]
    fn headless_mode_follows_agent_headless() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("profile.conf");
        let role = dir.path().join("mesh/role");
        let load = |body: &str| {
            let cfg = dir.path().join("config.yaml");
            write(&cfg, body);
            AgentConfig::load_from(&cfg, &pc, &role).headless_mode
        };
        // Absent → full agent.
        assert!(!load("agent:\n  profile: drone\n"));
        // Explicit false → full agent.
        assert!(!load("agent:\n  profile: drone\n  headless: false\n"));
        // Explicit true → lean headless.
        assert!(load("agent:\n  profile: drone\n  headless: true\n"));
    }

    #[test]
    fn workstation_and_compute_profiles_resolve_and_never_collapse_to_drone() {
        // Both the operator's workstation and a lean engine-only compute worker must
        // survive profile resolution (a `compute` value used to fall through to
        // `drone`, which never started the compute engine). The wire form is the
        // same token for both, so `profile_gate()` matches the `workstation|compute`
        // engine gate and the reconstruction pipeline runs on either node.
        let dir = tempfile::tempdir().unwrap();
        let role = dir.path().join("mesh/role");
        let no_conf = dir.path().join("nope.conf");
        let load = |profile: &str| {
            let cfg = dir.path().join("config.yaml");
            write(&cfg, &format!("agent:\n  profile: {profile}\n"));
            AgentConfig::load_from(&cfg, &no_conf, &role)
        };
        let ws = load("workstation");
        assert_eq!(ws.profile_wire, "workstation");
        assert_eq!(ws.profile_gate(), "workstation");
        let compute = load("compute");
        assert_eq!(compute.profile_wire, "compute");
        assert_eq!(compute.profile_gate(), "compute");
    }

    #[test]
    fn unknown_role_sentinel_falls_back_to_direct() {
        let dir = tempfile::tempdir().unwrap();
        let role = dir.path().join("role");
        write(&role, "garbage\n");
        assert_eq!(read_current_role(&role), "direct");
    }
}
