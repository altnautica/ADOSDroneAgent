use super::*;

/// A tempdir wired with a config + role sentinel + the resolved seam paths, so
/// the gating helpers read fixtures via the path-injectable cores. No process
/// env is mutated, so the test cannot race a sibling test.
struct Env {
    _dir: tempfile::TempDir,
    config: PathBuf,
    profile_conf: PathBuf,
    role_path: PathBuf,
}

fn with_env(role: Option<&str>, config_body: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    std::fs::write(&config, config_body).unwrap();
    // The profile resolver reads profile.conf only when the config field is
    // "auto"/empty; an explicit profile in config_body wins, so point the
    // sentinel at an absent path to keep the resolver deterministic.
    let profile_conf = dir.path().join("absent.conf");
    let role_path = dir.path().join("role");
    if let Some(r) = role {
        std::fs::write(&role_path, format!("{r}\n")).unwrap();
    }
    Env {
        _dir: dir,
        config,
        profile_conf,
        role_path,
    }
}

#[test]
fn drone_profile_does_not_pass_the_gate() {
    let env = with_env(None, "agent:\n  profile: drone\n");
    assert!(!is_ground_station_at(
        &env.config,
        &env.profile_conf,
        &env.role_path
    ));
}

#[test]
fn ground_station_profile_passes_the_gate() {
    let env = with_env(Some("direct"), "agent:\n  profile: ground_station\n");
    assert!(is_ground_station_at(
        &env.config,
        &env.profile_conf,
        &env.role_path
    ));
}

/// The golden role body the GCS reads on a relay-role and a direct-role
/// ground station: the direct role owns the single-node receive plane.
#[test]
fn role_body_is_the_golden_shape() {
    let all = json!([
        "ados-wfb-rx.service",
        "ados-batman.service",
        "ados-wfb-relay.service",
        "ados-wfb-receiver.service",
    ]);
    assert_eq!(
        role_body("relay", "relay"),
        json!({
            "role": "relay",
            "configured": "relay",
            "supported": ["direct", "relay", "receiver"],
            "units": ["ados-batman.service", "ados-wfb-relay.service"],
            "all_mesh_units": all,
        })
    );
    assert_eq!(
        role_body("direct", "direct")["units"],
        json!(["ados-wfb-rx.service"])
    );
}

#[test]
fn role_config_default_is_direct_when_section_absent() {
    let env = with_env(Some("direct"), "agent:\n  profile: ground_station\n");
    let cfg = MeshRouteConfig::load_from(&env.config);
    assert_eq!(cfg.ground_station.role, "direct");
}

#[test]
fn mesh_config_defaults_match_the_model_when_the_section_is_absent() {
    // A config with no ground_station section reads the MeshConfig defaults.
    let env = with_env(Some("direct"), "agent:\n  profile: ground_station\n");
    let mesh = MeshRouteConfig::load_from(&env.config).ground_station.mesh;
    let body = json!({
        "mesh_id": mesh.mesh_id,
        "carrier": mesh.carrier,
        "channel": mesh.channel,
        "bat_iface": mesh.bat_iface,
        "interface_override": mesh.interface_override,
    });
    let want = json!({
        "mesh_id": null,
        "carrier": "802.11s",
        "channel": 1,
        "bat_iface": "bat0",
        "interface_override": null,
    });
    assert_eq!(body, want);
}

#[test]
fn mesh_config_reads_the_configured_values() {
    let body = "ground_station:\n  mesh:\n    mesh_id: site-a\n    carrier: ibss\n    channel: 6\n    bat_iface: bat1\n    interface_override: wlan2\n";
    let env = with_env(Some("direct"), body);
    let mesh = MeshRouteConfig::load_from(&env.config).ground_station.mesh;
    let got = json!({
        "mesh_id": mesh.mesh_id,
        "carrier": mesh.carrier,
        "channel": mesh.channel,
        "bat_iface": mesh.bat_iface,
        "interface_override": mesh.interface_override,
    });
    assert_eq!(
        got,
        json!({
            "mesh_id": "site-a",
            "carrier": "ibss",
            "channel": 6,
            "bat_iface": "bat1",
            "interface_override": "wlan2",
        })
    );
}

#[test]
fn slicers_default_to_empty_collections() {
    // An empty snapshot yields the empty-list / null defaults.
    let empty = Map::new();
    assert_eq!(slice_neighbors(&empty), json!({"neighbors": []}));
    assert_eq!(slice_routes(&empty), json!({"routes": []}));
    assert_eq!(
        slice_gateways(&empty),
        json!({"gateways": [], "selected": null})
    );
}

#[test]
fn slicers_project_a_populated_snapshot() {
    // The neighbors list is shared by /mesh/neighbors and /mesh/routes.
    let mut snap = Map::new();
    snap.insert("neighbors".to_string(), json!([{"mac": "aa:bb"}]));
    snap.insert("gateways".to_string(), json!([{"mac": "cc:dd"}]));
    snap.insert("selected_gateway".to_string(), json!("cc:dd"));
    assert_eq!(
        slice_neighbors(&snap),
        json!({"neighbors": [{"mac": "aa:bb"}]})
    );
    assert_eq!(slice_routes(&snap), json!({"routes": [{"mac": "aa:bb"}]}));
    assert_eq!(
        slice_gateways(&snap),
        json!({"gateways": [{"mac": "cc:dd"}], "selected": "cc:dd"})
    );
}

#[test]
fn a_mesh_sidecar_a_dead_poll_loop_left_behind_reads_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mesh-state.json");
    std::fs::write(&path, r#"{"up":true,"neighbors":[{"id":"a"}]}"#).unwrap();
    let written = std::fs::metadata(&path).unwrap().modified().unwrap();
    let live = fresh_mesh_sidecar(&path, written + std::time::Duration::from_secs(1));
    assert_eq!(live["up"], json!(true));
    let dead = fresh_mesh_sidecar(&path, written + mesh_fresh_window() * 2);
    assert_eq!(dead, Map::new());
    assert_eq!(slice_neighbors(&dead), json!({"neighbors": []}));
}

#[test]
fn error_responses_are_404() {
    assert_eq!(profile_mismatch().status(), StatusCode::NOT_FOUND);
    assert_eq!(not_in_mesh().status(), StatusCode::NOT_FOUND);
}
