use super::*;

fn agent_manifest(agent_extra: &str) -> Result<PluginManifest, ManifestError> {
    PluginManifest::from_yaml_text(&format!(
        "id: com.example.x\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n{agent_extra}"
    ))
}

const BY_PROFILE: &str = "  resources:\n    class: heavy\n    max_ram_mb: 16384\n    max_cpu_percent: 800\n    max_pids: 1024\n  resources_by_profile:\n    drone:\n      class: heavy\n      max_ram_mb: 768\n      max_cpu_percent: 200\n      max_pids: 128\n    ground_station:\n      max_ram_mb: 512\n";

#[test]
fn a_listed_profile_gets_its_own_envelope_and_others_the_default() {
    let agent = agent_manifest(BY_PROFILE).unwrap().agent.unwrap();
    let drone = agent.resources_for("drone");
    assert_eq!(
        (drone.max_ram_mb, drone.max_cpu_percent, drone.max_pids),
        (768, 200, 128)
    );
    // The underscore key is read as the hyphen form, and either spelling of
    // the node profile finds it.
    assert_eq!(agent.resources_for("ground-station").max_ram_mb, 512);
    assert_eq!(agent.resources_for("ground_station").max_ram_mb, 512);
    assert_eq!(
        agent.resources_for("ground-station").class,
        ResourceClass::Standard
    );
    // An unlisted profile falls back to `resources`.
    assert_eq!(agent.resources_for("workstation").max_ram_mb, 16384);
    assert_eq!(agent.resources_for("visor").max_ram_mb, 16384);
    assert!(agent.any_heavy());
}

#[test]
fn no_per_profile_block_is_the_default_envelope_everywhere() {
    let agent = agent_manifest("").unwrap().agent.unwrap();
    assert!(agent.resources_by_profile.is_empty());
    assert_eq!(agent.resources_for("drone").max_ram_mb, 96);
    assert!(!agent.any_heavy());
}

#[test]
fn a_per_profile_envelope_is_bounded_by_its_own_class() {
    let err = agent_manifest("  resources_by_profile:\n    drone:\n      max_ram_mb: 8192\n")
        .unwrap_err();
    assert!(
        err.0
            .contains("agent.resources_by_profile.drone.max_ram_mb 8192"),
        "{}",
        err.0
    );
    let heavy = "  resources_by_profile:\n    drone:\n      class: heavy\n      max_ram_mb: 8192\n";
    let agent = agent_manifest(heavy).unwrap().agent.unwrap();
    assert_eq!(agent.resources_for("drone").max_ram_mb, 8192);
    assert!(
        agent.any_heavy(),
        "a heavy per-profile entry counts as heavy"
    );
}

#[test]
fn a_per_profile_block_refuses_unknown_profiles_keys_and_duplicates() {
    let unknown =
        agent_manifest("  resources_by_profile:\n    laptop:\n      max_ram_mb: 64\n").unwrap_err();
    assert!(
        unknown.0.contains("agent.resources_by_profile"),
        "{}",
        unknown.0
    );
    let bad_key =
        agent_manifest("  resources_by_profile:\n    drone:\n      max_swap_mb: 64\n").unwrap_err();
    assert!(bad_key.0.contains("max_swap_mb"), "{}", bad_key.0);
    let twice = "  resources_by_profile:\n    ground_station:\n      max_ram_mb: 64\n    ground-station:\n      max_ram_mb: 64\n";
    assert!(agent_manifest(twice).is_err());
    assert!(agent_manifest("  resources_by_profile: [drone]\n").is_err());
}
