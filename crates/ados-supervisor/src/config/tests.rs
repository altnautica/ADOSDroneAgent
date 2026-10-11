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
    // Absent `video.mode` reads as the schema default `wfb`: video enabled.
    assert!(ac.video_enabled);
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

#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<RawConfig>("");
}
