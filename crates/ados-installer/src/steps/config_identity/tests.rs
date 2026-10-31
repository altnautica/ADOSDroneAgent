use super::provision::default_agent_name;
use super::*;
use crate::env::CONVEX_URL;

#[test]
fn slugify_lowercases_and_collapses() {
    assert_eq!(slugify_hostname("My Drone 01"), "my-drone-01");
    assert_eq!(slugify_hostname("ALPHA__beta"), "alpha-beta");
    assert_eq!(slugify_hostname("--lead-and-trail--"), "lead-and-trail");
    assert_eq!(slugify_hostname("skynode"), "skynode");
}

#[test]
fn slugify_caps_at_63_and_trims_dash() {
    let long = "a".repeat(70);
    let slug = slugify_hostname(&long);
    assert_eq!(slug.len(), 63);
    // A name whose 63rd char region is a dash must not leave a trailing '-'.
    let dashy = format!("{}-x", "b".repeat(62));
    let s = slugify_hostname(&dashy);
    assert!(!s.ends_with('-'), "slug must not end with '-': {s}");
    assert_eq!(s.len(), 62);
}

#[test]
fn slugify_all_disallowed_is_empty() {
    assert_eq!(slugify_hostname("@@@"), "");
    assert_eq!(slugify_hostname(""), "");
}

/// The generated config carries the identity file's id whole, so config and
/// file can never name the node differently. An 8-char prefix here once
/// made a drone refuse every relayed request addressed by its full id.
#[test]
fn the_generated_config_names_the_node_by_the_identity_file() {
    let id = "4e7a083410f5";
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("device-id");
    std::fs::write(&file, format!("{id}\n")).unwrap();

    let cfg = default_config_yaml(id, &default_agent_name(id), "drone", "", "local", None, "");
    let parsed: serde_norway::Value = serde_norway::from_str(&cfg).unwrap();
    let configured = parsed["agent"]["device_id"].as_str().unwrap();
    assert_eq!(configured, std::fs::read_to_string(&file).unwrap().trim());
    assert_eq!(
        ados_protocol::identity::resolve(&file, None, Some(configured)),
        configured
    );
    // The name stays a short label.
    assert_eq!(parsed["agent"]["name"].as_str(), Some("ados-4e7a0834"));
}

#[test]
fn default_config_carries_identity_and_blocks() {
    let cfg = default_config_yaml(
        "abcd1234",
        "skynode",
        "drone",
        "/dev/ttyACM0",
        "local",
        None,
        "",
    );
    assert!(cfg.contains("device_id: \"abcd1234\""));
    assert!(cfg.contains("name: \"skynode\""));
    assert!(cfg.contains("profile: \"drone\""));
    assert!(cfg.contains("serial_port: \"/dev/ttyACM0\""));
    // No dead blocks: the REST port lives in the API's own config, not a
    // retired scripting block, and recording is not a video key.
    assert!(!cfg.contains("scripting:"));
    assert!(!cfg.contains("rest_api:"));
    assert!(!cfg.contains("record:"));
    // The pairing convex_url must be the constant, not blank.
    assert!(cfg.contains(&format!("convex_url: \"{CONVEX_URL}\"")));
    // Video defaults present.
    assert!(cfg.contains("bitrate_kbps: 4000"));
    // Install-honesty default: the aux lane enabled, so a fresh install is
    // not inert by default. A drone carries no injector arbitration.
    assert!(cfg.contains("aux_enable: true"));
    // Local-first server mode + the unrestricted region default.
    assert!(cfg.contains("mode: \"local\""));
    assert!(cfg.contains("mode: \"unrestricted\""));
    assert!(cfg.contains("region: null"));
}

#[test]
fn default_config_empty_fc_port_is_blank_string() {
    let cfg = default_config_yaml(
        "abcd1234",
        "ados-abcd1234",
        "ground_station",
        "",
        "cloud",
        Some("US"),
        "2026-07-04T00:00:00Z",
    );
    assert!(cfg.contains("serial_port: \"\""));
    assert!(cfg.contains("profile: \"ground_station\""));
    // Cloud (reach-from-anywhere) server mode + a pinned region.
    assert!(cfg.contains("mode: \"cloud\""));
    assert!(cfg.contains("mode: \"region\""));
    assert!(cfg.contains("region: \"US\""));
    assert!(cfg.contains("ack_at: \"2026-07-04T00:00:00Z\""));
}

#[test]
fn default_config_links_the_configuration_page() {
    let cfg = default_config_yaml("abcd1234", "n", "drone", "", "local", None, "");
    assert!(cfg.contains("# Docs: https://docs.altnautica.com/drone-agent/configuration\n"));
}

#[test]
fn no_dead_mqtt_keys_in_template() {
    for profile in ["drone", "ground_station", "visor"] {
        let cfg = default_config_yaml("abcd1234", "n", profile, "", "local", None, "");
        assert!(!cfg.contains("mqtt_username"), "{profile}");
        assert!(!cfg.contains("mqtt_password"), "{profile}");
        assert!(!cfg.contains("mqtt_plaintext_dev"), "{profile}");
        // The one MQTT key the relay reads stays.
        assert!(cfg.contains("mqtt_transport: \"websockets\""), "{profile}");
    }
}

#[test]
fn drone_template_has_no_injector_arbitration() {
    let drone = default_config_yaml("abcd1234", "n", "drone", "", "local", None, "");
    assert!(!drone.contains("injector_arbitration"));
    let visor = default_config_yaml("abcd1234", "n", "visor", "", "local", None, "");
    assert!(!visor.contains("injector_arbitration"));
    assert!(visor.contains("profile: \"visor\""));
    // A panel profile auto-detects its display; everything else gets none.
    assert_eq!(default_display_arg("visor"), "auto");
    assert_eq!(default_display_arg("ground_station"), "auto");
    assert_eq!(default_display_arg("drone"), "none");
    let gs = default_config_yaml("abcd1234", "n", "ground_station", "", "local", None, "");
    let parsed: serde_norway::Value = serde_norway::from_str(&gs).unwrap();
    assert_eq!(
        parsed["mavlink"]["injector_arbitration"].as_bool(),
        Some(true)
    );
    assert_eq!(parsed["mavlink"]["component_id"].as_u64(), Some(191));
    let parsed: serde_norway::Value = serde_norway::from_str(&drone).unwrap();
    assert_eq!(parsed["mavlink"]["component_id"].as_u64(), Some(191));
}

#[test]
fn regulatory_block_defaults_to_unrestricted_and_pins_on_demand() {
    let free = regulatory_block(None, "");
    assert!(free.contains("mode: \"unrestricted\""));
    assert!(free.contains("region: null"));
    // The default note is honest: it never claims compliance.
    assert!(free.to_lowercase().contains("responsible"));
    assert!(!free.to_lowercase().contains("compliant"));

    let pinned = regulatory_block(Some("IN"), "2026-07-04T10:00:00Z");
    assert!(pinned.contains("mode: \"region\""));
    assert!(pinned.contains("region: \"IN\""));
    assert!(pinned.contains("ack_operator: \"installer\""));
    assert!(pinned.contains("ack_at: \"2026-07-04T10:00:00Z\""));
}

#[test]
fn pairing_json_uppercases_and_stamps() {
    let body = pairing_json("abcd-1234", 1700000000, None);
    assert!(body.contains("\"pairing_code\": \"ABCD-1234\""));
    assert!(body.contains("\"code_created_at\": 1700000000"));
    assert!(!body.contains("pending_api_key"));
    let keyed = pairing_json("abcd-1234", 1700000000, Some("ados_k"));
    let doc: serde_json::Value = serde_json::from_str(&keyed).unwrap();
    assert_eq!(doc["pending_api_key"], "ados_k");
    let key = mint_pending_api_key().unwrap();
    assert!(key.starts_with("ados_") && key.len() == 48, "key {key}");
}

#[test]
fn cutover_note_is_native_only_for_every_plane() {
    let body = cutover_note_body();
    // Every plane is native-only now: no fallback flag of any kind may
    // appear in the operator note (the mesh relay/receiver fallback was
    // the last one, and it is gone).
    assert!(!body.contains("/etc/ados/wfb-python-fallback"));
    assert!(!body.contains("/etc/ados/groundlink-python-fallback"));
    assert!(!body.contains("python-fallback"));
    assert!(!body.contains("ados-wfb-rx"));
    // It must state the services run native binaries.
    assert!(body.to_lowercase().contains("native"));
    // No stale rust-enabled opt-in flag should leak into the operator note.
    assert!(!body.contains("rust-enabled"));
}

#[test]
fn profile_conf_body_is_yaml_line() {
    assert_eq!(
        profile_conf_body("ground_station", "edge", None),
        "profile: ground_station\nchannel: edge\n"
    );
}

#[test]
fn hosts_rewrite_replaces_existing_127_0_1_1() {
    let hosts = "127.0.0.1\tlocalhost\n127.0.1.1\told-name\n::1\tlocalhost\n";
    let out = rewrite_hosts_body(hosts, "skynode");
    assert!(out.contains("127.0.1.1\tskynode"));
    assert!(!out.contains("old-name"));
    // The other lines survive.
    assert!(out.contains("127.0.0.1\tlocalhost"));
    assert!(out.contains("::1\tlocalhost"));
}

#[test]
fn hosts_rewrite_appends_when_absent() {
    let hosts = "127.0.0.1\tlocalhost\n";
    let out = rewrite_hosts_body(hosts, "gs-example");
    assert!(out.contains("127.0.1.1\tgs-example"));
    assert!(out.ends_with('\n'));
}
