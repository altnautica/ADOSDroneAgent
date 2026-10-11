use super::*;
use std::io::Write;

fn write_file(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path
}

#[test]
fn missing_file_reads_inert_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = TunnelChannelConfig::load_from(&dir.path().join("nope.yaml"));
    assert_eq!(cfg, TunnelChannelConfig::default());
    assert!(!cfg.enabled);
    assert!(!cfg.command_enabled);
    assert_eq!(cfg.rx_port, DEFAULT_RX_PORT);
}

#[test]
fn parses_the_enable_gates_and_the_ingress_port() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        &dir,
        "config.yaml",
        "radio:\n  tunnel:\n    enabled: true\n    command_enabled: true\n    rx_port: 6001\n",
    );
    let cfg = TunnelChannelConfig::load_from(&path);
    assert!(cfg.enabled && cfg.command_enabled);
    assert_eq!(cfg.rx_port, 6001);
}

#[test]
fn the_ingress_port_agrees_with_what_the_plane_consumers_forward_to() {
    // Two independent readers resolve this port: this service, which binds
    // it, and each rig's aux plane consumer, which forwards to it from a
    // crate that cannot depend on this one. If they ever disagree the lane
    // goes silent with no error anywhere — frames arrive over the air,
    // decode fine, and land on a port nobody is bound to.
    let text = "radio:\n  tunnel:\n    enabled: true\n    rx_port: 6001\n";
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "config.yaml", text);
    assert_eq!(
        TunnelChannelConfig::load_from(&path).rx_port,
        ados_protocol::config_tunnel_ingest::ingress_port_from_yaml(text)
    );
    assert_eq!(
        TunnelChannelConfig::default().rx_port,
        ados_protocol::config_tunnel_ingest::DEFAULT_INGRESS_PORT
    );
}

#[test]
fn a_zero_port_falls_back_to_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        &dir,
        "config.yaml",
        "radio:\n  tunnel:\n    enabled: true\n    rx_port: 0\n",
    );
    let cfg = TunnelChannelConfig::load_from(&path);
    assert!(cfg.enabled);
    assert!(!cfg.command_enabled); // not implied by enabling the channel
    assert_eq!(cfg.rx_port, DEFAULT_RX_PORT);
}

#[test]
fn resolves_profile_from_config_then_marker() {
    let dir = tempfile::tempdir().unwrap();
    let drone = write_file(&dir, "drone.yaml", "agent:\n  profile: drone\n");
    assert_eq!(resolve_profile(&drone, &dir.path().join("none")), "drone");
    let gs = write_file(&dir, "gs.yaml", "agent:\n  profile: ground_station\n");
    assert_eq!(
        resolve_profile(&gs, &dir.path().join("none")),
        "ground-station"
    );
    // No config profile → consult the marker.
    let auto = write_file(&dir, "auto.yaml", "agent:\n  profile: auto\n");
    let marker = write_file(&dir, "profile.conf", "profile: drone\n");
    assert_eq!(resolve_profile(&auto, &marker), "drone");
}

#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<TunnelSection>("/radio/tunnel");
}
