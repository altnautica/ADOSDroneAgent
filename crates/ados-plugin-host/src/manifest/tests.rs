use super::*;

#[test]
fn supports_board_matches_stem_soc_and_star() {
    let compat = |boards: &[&str]| Compatibility {
        ados_version: ">=0.1.0".into(),
        gcs_version: None,
        supported_boards: boards.iter().map(|s| s.to_string()).collect(),
        min_tier: None,
    };
    let board = BoardId {
        profile: "rock-5c-lite".into(),
        soc: "RK3582".into(),
    };
    assert!(compat(&[]).supports_board(&board));
    assert!(compat(&["*"]).supports_board(&board));
    assert!(compat(&["cm4", "ROCK-5C-LITE"]).supports_board(&board));
    assert!(compat(&["rk3582"]).supports_board(&board));
    // The display name is not a board token.
    assert!(!compat(&["Radxa ROCK 5C Lite (RK3582)"]).supports_board(&board));
    assert!(!compat(&["cm4"]).supports_board(&board));
    // An unknown SoC never matches an empty entry.
    let no_soc = BoardId {
        profile: "cm4".into(),
        soc: String::new(),
    };
    assert!(!compat(&[""]).supports_board(&no_soc));
    assert_eq!(board.to_string(), "rock-5c-lite");
}

const MINIMAL: &str = r#"
id: com.example.thermal
version: 1.0.0
name: Thermal
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
agent:
  entrypoint: agent/py/thermal.py
  permissions:
    - hardware.spi
    - id: mission.write
      required: false
"#;

#[test]
fn parses_minimal_and_reads_lifecycle_fields() {
    let m = PluginManifest::from_yaml_text(MINIMAL).unwrap();
    assert_eq!(m.id, "com.example.thermal");
    assert_eq!(m.version, "1.0.0");
    assert_eq!(m.risk, "medium");
    let agent = m.agent.as_ref().unwrap();
    assert_eq!(agent.isolation, AgentIsolation::Subprocess);
    assert_eq!(agent.resources.max_ram_mb, 96);
    assert!(m.is_subprocess_agent());
    assert_eq!(
        m.declared_permissions(),
        ["hardware.spi", "mission.write"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
}

#[test]
fn bare_string_and_object_permissions_both_parse() {
    let m = PluginManifest::from_yaml_text(MINIMAL).unwrap();
    let perms = &m.agent.as_ref().unwrap().permissions;
    assert_eq!(perms[0].id, "hardware.spi");
    assert!(perms[0].required);
    assert_eq!(perms[1].id, "mission.write");
    assert!(!perms[1].required);
}

#[test]
fn subprocess_spawn_allowlist_parses_and_defaults_empty() {
    // Default: absent → empty allowlist.
    let m = PluginManifest::from_yaml_text(MINIMAL).unwrap();
    assert!(m.agent.as_ref().unwrap().subprocess_spawn.is_empty());

    // Explicit: a list of basenames the plugin may process.spawn.
    let yaml = r#"
id: com.example.spawner
version: 1.0.0
compatibility:
  ados_version: ">=0.1.0"
agent:
  entrypoint: agent/py/x.py
  permissions:
    - process.spawn
  subprocess_spawn:
    - ffmpeg
    - v4l2-ctl
"#;
    let m = PluginManifest::from_yaml_text(yaml).unwrap();
    assert_eq!(
        m.agent.as_ref().unwrap().subprocess_spawn,
        vec!["ffmpeg".to_string(), "v4l2-ctl".to_string()]
    );
}

#[test]
fn manifests_the_sdk_refuses_are_refused_here_too() {
    let base =
        "id: com.example.rules\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n";
    let agent = "agent:\n  entrypoint: agent/py/x.py\n";
    let refused = [
        (base.to_string(), "neither agent nor gcs"),
        (format!("{base}risk: extreme\n{agent}"), "risk"),
        (
            format!("{base}{agent}").replace(
                "ados_version: \">=0.1.0\"\n",
                "ados_version: \">=0.1.0\"\n  min_tier: 9\n",
            ),
            "min_tier",
        ),
        (
            format!("{base}{agent}  subprocess_spawn:\n    - ffmpeg\n"),
            "process.spawn",
        ),
        (
            format!("{base}agent:\n  entrypoint: bin/x\n  runtime: rust\n  isolation: inprocess\n"),
            "in-process",
        ),
        (
            format!("{base}{agent}  contains_vendor_binary: true\n"),
            "vendor_attribution is empty",
        ),
        (
            format!("{base}{agent}  vendor_attribution:\n    - name: lib\n      license: MIT\n"),
            "contains_vendor_binary is false",
        ),
    ];
    for (yaml, needle) in refused {
        let err = PluginManifest::from_yaml_text(&yaml).unwrap_err().0;
        assert!(err.contains(needle), "{needle}: {err}");
    }
    let tiered = format!("{base}{agent}").replace(
        "ados_version: \">=0.1.0\"\n",
        "ados_version: \">=0.1.0\"\n  min_tier: 4\n",
    );
    assert!(PluginManifest::from_yaml_text(&tiered).is_ok());
    let paired = format!(
            "{base}risk: critical\n{agent}  contains_vendor_binary: true\n  vendor_attribution:\n    - name: lib\n      license: MIT\n"
        );
    assert!(PluginManifest::from_yaml_text(&paired).is_ok());
}

#[test]
fn inprocess_isolation_parses() {
    let yaml = r#"
id: com.altnautica.builtin
version: 0.1.0
compatibility:
  ados_version: ">=0.1.0"
agent:
  entrypoint: pkg.mod:Class
  isolation: inprocess
"#;
    let m = PluginManifest::from_yaml_text(yaml).unwrap();
    assert_eq!(m.agent.unwrap().isolation, AgentIsolation::Inprocess);
    assert!(!PluginManifest::from_yaml_text(yaml)
        .unwrap()
        .is_subprocess_agent());
}

#[test]
fn agent_runtime_defaults_to_python_when_absent() {
    // An old manifest with no runtime: field parses and is python.
    let m = PluginManifest::from_yaml_text(MINIMAL).unwrap();
    assert_eq!(m.agent.as_ref().unwrap().runtime, AgentRuntime::Python);
    assert_eq!(m.agent_runtime(), Some(AgentRuntime::Python));
}

#[test]
fn agent_runtime_rust_parses() {
    let yaml = r#"
id: com.example.rustplug
version: 1.0.0
compatibility:
  ados_version: ">=0.1.0"
agent:
  entrypoint: agent/bin/com.example.rustplug
  runtime: rust
"#;
    let m = PluginManifest::from_yaml_text(yaml).unwrap();
    assert_eq!(m.agent.as_ref().unwrap().runtime, AgentRuntime::Rust);
    assert_eq!(m.agent_runtime(), Some(AgentRuntime::Rust));
}

#[test]
fn gcs_only_plugin_has_no_agent_runtime() {
    let yaml = r#"
id: com.example.panel
version: 0.1.0
compatibility:
  ados_version: ">=0.1.0"
gcs:
  entrypoint: gcs/dist/index.js
"#;
    let m = PluginManifest::from_yaml_text(yaml).unwrap();
    assert_eq!(m.agent_runtime(), None);
}

#[test]
fn install_dialog_fields_parse_and_unknown_fields_are_refused() {
    let yaml = r#"
id: com.example.future
version: 2.0.0
schema_version: 2
author: someone
features: [a, b]
compatibility:
  ados_version: ">=0.1.0"
gcs:
  entrypoint: gcs/dist/index.js
  isolation: inline
"#;
    let m = PluginManifest::from_yaml_text(yaml).unwrap();
    assert_eq!(m.gcs.as_ref().unwrap().isolation, GcsIsolation::Inline);
    assert!(!m.is_subprocess_agent());
    let unknown = format!("{yaml}future_field: 1\n");
    let err = PluginManifest::from_yaml_text(&unknown).unwrap_err();
    assert!(err.0.contains("unknown field"), "{}", err.0);
}

#[test]
fn non_mapping_top_level_is_rejected() {
    let err = PluginManifest::from_yaml_text("- a\n- b").unwrap_err();
    assert!(err.0.contains("top-level must be a mapping"), "{}", err.0);
}

/// A manifest built from YAML with the given id, version and agent
/// entrypoint. Values are emitted as JSON-quoted scalars (valid YAML) so a
/// test can carry a newline or quote through the parser intact.
fn manifest_with(id: &str, version: &str, entrypoint: &str) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap();
    format!(
            "id: {}\nversion: {}\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: {}\n  runtime: rust\n",
            q(id),
            q(version),
            q(entrypoint)
        )
}

#[test]
fn ids_that_escape_the_install_dir_or_the_unit_line_are_rejected() {
    for id in [
        "../../etc",
        "/etc",
        "../../../etc/cron.d",
        "com.example.x\nExecStartPre=+/bin/sh -c id",
        "com.example.x y",
        "com.example/x",
        "Com.Example.x",
        "single",
        "com..example",
        "com.example.",
        ".com.example",
        "",
    ] {
        let err = PluginManifest::from_yaml_text(&manifest_with(id, "1.0.0", "bin/x"))
            .expect_err(&format!("id {id:?} must be rejected"));
        assert!(err.0.contains("plugin id"), "{id:?}: {}", err.0);
    }
    for id in [
        "com.example.thermal-lepton",
        "com.example.x",
        "io.ados.a1.b-2",
    ] {
        PluginManifest::from_yaml_text(&manifest_with(id, "1.0.0", "bin/x"))
            .unwrap_or_else(|e| panic!("id {id:?} must be accepted: {}", e.0));
    }
}

#[test]
fn entrypoints_that_escape_or_inject_are_rejected() {
    for entrypoint in [
        "bin/x\nExecStartPre=+/bin/sh -c 'id>/root/p'",
        "bin/x\rExecStartPre=+/bin/sh",
        "bin/x\tflag",
        "bin/x --flag",
        "../bin/x",
        "bin/../../x",
        "/usr/bin/x",
        "bin//x",
        "bin/x/",
        "",
        "pkg.mod:Class\nExecStartPre=+/bin/sh",
        "pkg.mod:Class:Extra",
        "../pkg:Class",
        ":Class",
        "pkg:",
    ] {
        let err =
            PluginManifest::from_yaml_text(&manifest_with("com.example.x", "1.0.0", entrypoint))
                .expect_err(&format!("entrypoint {entrypoint:?} must be rejected"));
        assert!(
            err.0.contains("agent.entrypoint"),
            "{entrypoint:?}: {}",
            err.0
        );
    }
    for entrypoint in [
        "agent/py/thermal.py",
        "agent/bin/com.example.rustplug",
        "bin/vision-nav",
        "gcs/plugin.bundle.js",
        "pkg.mod:Class",
        "altnautica_thermal_camera.plugin:ThermalUsbPlugin",
    ] {
        PluginManifest::from_yaml_text(&manifest_with("com.example.x", "1.0.0", entrypoint))
            .unwrap_or_else(|e| panic!("entrypoint {entrypoint:?} must be accepted: {}", e.0));
    }
}

#[test]
fn the_gcs_entrypoint_is_held_to_the_same_rules() {
    let yaml = "id: com.example.panel\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: \"../../dist/index.js\"\n";
    let err = PluginManifest::from_yaml_text(yaml).unwrap_err();
    assert!(err.0.contains("gcs.entrypoint"), "{}", err.0);
}

#[test]
fn version_must_be_semver() {
    for version in [
        "1.0", "01.0.0", "1.0.0-", "1.0.0-01", "1.0.0+", "1.0.0 ", "v1.0.0", "1.0.0\n",
    ] {
        let err = PluginManifest::from_yaml_text(&manifest_with("com.example.x", version, "bin/x"))
            .expect_err(&format!("version {version:?} must be rejected"));
        assert!(err.0.contains("semver"), "{version:?}: {}", err.0);
    }
    for version in [
        "0.0.0",
        "1.2.3",
        "1.0.0-rc.1",
        "1.0.0-alpha-2.0a",
        "1.0.0+build.5",
        "10.20.30-0.x+b",
    ] {
        PluginManifest::from_yaml_text(&manifest_with("com.example.x", version, "bin/x"))
            .unwrap_or_else(|e| panic!("version {version:?} must be accepted: {}", e.0));
    }
}

fn agent_manifest(id: &str, agent_extra: &str) -> Result<PluginManifest, ManifestError> {
    PluginManifest::from_yaml_text(&format!(
            "id: {id}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n{agent_extra}"
        ))
}

#[test]
fn target_profiles_default_to_drone_and_normalize_the_underscore_form() {
    let m = agent_manifest("com.example.x", "").unwrap();
    assert_eq!(m.agent.unwrap().target_profiles, vec!["drone".to_string()]);
    let m = agent_manifest(
        "com.example.x",
        "  target_profiles: [ground_station, compute, ground-station]\n",
    )
    .unwrap();
    assert_eq!(
        m.agent.unwrap().target_profiles,
        vec!["ground-station".to_string(), "compute".to_string()]
    );
    for bad in ["  target_profiles: []\n", "  target_profiles: [laptop]\n"] {
        let err = agent_manifest("com.example.x", bad).unwrap_err();
        assert!(err.0.contains("agent.target_profiles"), "{}", err.0);
    }
}

#[test]
fn resource_bounds_follow_the_resource_class() {
    let over = "  resources:\n    max_ram_mb: 8192\n";
    let err = agent_manifest("com.example.x", over).unwrap_err();
    assert!(err.0.contains("max_ram_mb 8192"), "{}", err.0);
    let heavy = "  resources:\n    class: heavy\n    max_ram_mb: 16384\n    max_cpu_percent: 800\n    max_pids: 1024\n";
    let m = agent_manifest("com.example.x", heavy).unwrap();
    assert_eq!(m.agent.unwrap().resources.class, ResourceClass::Heavy);
    let err = agent_manifest(
        "com.example.x",
        "  resources:\n    class: heavy\n    max_cpu_percent: 4000\n",
    )
    .unwrap_err();
    assert!(err.0.contains("max_cpu_percent"), "{}", err.0);
    assert!(agent_manifest("com.example.x", "  resources:\n    max_pids: 0\n").is_err());
}

#[test]
fn a_bin_entrypoint_needs_rust_and_a_binaries_entry() {
    let yaml = |runtime: &str, binaries: &str| {
        format!(
                "id: com.example.x\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: \"bin:x-link\"\n  runtime: {runtime}\n{binaries}"
            )
    };
    let binaries = "  binaries:\n    x-link:\n      aarch64-linux: bin/aarch64-linux/x-link\n";
    let m = PluginManifest::from_yaml_text(&yaml("rust", binaries)).unwrap();
    assert_eq!(
        m.agent.unwrap().binary_path("x-link", "aarch64-linux"),
        Some("bin/aarch64-linux/x-link")
    );
    let err = PluginManifest::from_yaml_text(&yaml("python", binaries)).unwrap_err();
    assert!(err.0.contains("runtime: rust"), "{}", err.0);
    let err = PluginManifest::from_yaml_text(&yaml("rust", "")).unwrap_err();
    assert!(err.0.contains("agent.binaries"), "{}", err.0);
    for bad in [
            "  binaries:\n    x-link:\n      aarch64: bin/x\n",
            "  binaries:\n    x-link:\n      aarch64-linux: ../x\n",
            "  binaries:\n    X:\n      aarch64-linux: bin/x\n    x-link:\n      aarch64-linux: bin/x\n",
        ] {
            assert!(PluginManifest::from_yaml_text(&yaml("rust", bad)).is_err(), "{bad}");
        }
}

#[test]
fn payloads_are_pinned_https_files_inside_the_tree() {
    let payload = |path: &str, source: &str, sha: &str, size: u64| {
        format!(
                "  payloads:\n    - path: {path}\n      source: {source}\n      sha256: {sha}\n      size_bytes: {size}\n      profiles: [ground_station]\n"
            )
    };
    let sha = "ab".repeat(32);
    let good = payload("bin/tool", "https://github.com/o/r/tool", &sha, 10);
    let m = agent_manifest("com.example.x", &good).unwrap();
    let spec = &m.agent.as_ref().unwrap().payloads[0];
    assert_eq!(spec.profiles, Some(vec!["ground-station".to_string()]));
    assert!(spec.applies_to("ground-station", "aarch64-linux"));
    assert!(!spec.applies_to("drone", "aarch64-linux"));
    assert_eq!(m.applicable_payloads("drone", "aarch64-linux").len(), 0);
    for bad in [
        payload("../escape", "https://github.com/o/r/tool", &sha, 10),
        payload("bin/tool", "http://github.com/o/r/tool", &sha, 10),
        payload(
            "bin/tool",
            "https://github.com/o/r/tool",
            &sha.to_uppercase(),
            10,
        ),
        payload("bin/tool", "https://github.com/o/r/tool", "abc", 10),
        payload("bin/tool", "https://github.com/o/r/tool", &sha, 0),
        payload(
            "bin/tool",
            "https://github.com/o/r/tool",
            &sha,
            PAYLOAD_MAX_BYTES + 1,
        ),
    ] {
        assert!(agent_manifest("com.example.x", &bad).is_err(), "{bad}");
    }
}

#[test]
fn declared_capabilities_and_shared_topics_live_under_the_id_leaf() {
    let ok = "  declared_capabilities:\n    - id: plugin.mapper.world.read\n      risk: low\n  contributes:\n    shared_topics:\n      - topic: plugin.mapper.occupancy\n        subscribe_capability: plugin.mapper.world.read\n      - topic: plugin.mapper.pose\n        subscribe_capability: telemetry.read\n";
    let m = agent_manifest("com.example.mapper", ok).unwrap();
    let agent = m.agent.unwrap();
    assert_eq!(agent.shared_topics().len(), 2);
    assert_eq!(agent.declared_capabilities[0].risk, "low");
    // The same block under another id is outside that plugin's namespace.
    let err = agent_manifest("com.example.other", ok).unwrap_err();
    assert!(err.0.contains("plugin.other."), "{}", err.0);
    // A subscribe capability must be known or declared by the owner.
    let unknown = "  contributes:\n    shared_topics:\n      - topic: plugin.mapper.pose\n        subscribe_capability: plugin.mapper.nope\n";
    let err = agent_manifest("com.example.mapper", unknown).unwrap_err();
    assert!(err.0.contains("subscribe_capability"), "{}", err.0);
    let bad_risk =
        "  declared_capabilities:\n    - id: plugin.mapper.world.read\n      risk: extreme\n";
    assert!(agent_manifest("com.example.mapper", bad_risk).is_err());
}

fn gcs_manifest(gcs_extra: &str) -> Result<PluginManifest, ManifestError> {
    PluginManifest::from_yaml_text(&format!(
            "id: com.example.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/index.mjs\n{gcs_extra}"
        ))
}

#[test]
fn gcs_isolation_is_iframe_or_inline() {
    assert_eq!(
        gcs_manifest("").unwrap().gcs.unwrap().isolation,
        GcsIsolation::Iframe
    );
    assert!(gcs_manifest("  isolation: worker\n").is_err());
}

#[test]
fn agent_pages_and_node_surfaces_are_validated() {
    let ok = "  contributes:\n    agent_pages:\n      - {id: world, title: World, section: videoVision, profile: [drone]}\n      - {id: world-setup, title: World setup, setup_for: world}\n    node_surfaces:\n      - {id: overview, title: Compute, profile: [workstation, compute], group: status, order: 10}\n";
    let gcs = gcs_manifest(ok).unwrap().gcs.unwrap();
    assert_eq!(gcs.agent_pages().len(), 2);
    assert_eq!(
        gcs.node_surfaces()[0].profile,
        vec!["workstation", "compute"]
    );
    for bad in [
            // setup_for names a missing page, itself, or another setup page.
            "  contributes:\n    agent_pages:\n      - {id: a, title: A, setup_for: nope}\n",
            "  contributes:\n    agent_pages:\n      - {id: a, title: A, setup_for: a}\n",
            "  contributes:\n    agent_pages:\n      - {id: a, title: A}\n      - {id: b, title: B, setup_for: a}\n      - {id: c, title: C, setup_for: b}\n",
            // Duplicate ids, a bad id, an empty title.
            "  contributes:\n    agent_pages:\n      - {id: a, title: A}\n      - {id: a, title: B}\n",
            "  contributes:\n    agent_pages:\n      - {id: A!, title: A}\n",
            "  contributes:\n    agent_pages:\n      - {id: a, title: ''}\n",
            // A surface needs a profile and a known group.
            "  contributes:\n    node_surfaces:\n      - {id: s, title: S}\n",
            "  contributes:\n    node_surfaces:\n      - {id: s, title: S, profile: []}\n",
            "  contributes:\n    node_surfaces:\n      - {id: s, title: S, profile: [drone], group: sidebar}\n",
        ] {
            assert!(gcs_manifest(bad).is_err(), "{bad}");
        }
}

#[test]
fn the_gcs_block_projection_carries_isolation_and_every_contribution() {
    let m = PluginManifest::from_yaml_text(
            "id: com.altnautica.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/index.mjs\n  isolation: inline\n  locales: [en]\n  contributes:\n    panels: [{id: p}]\n    settings: [{key: a}]\n    agent_pages:\n      - {id: world, title: World, profile: [ground_station]}\n    node_surfaces:\n      - {id: overview, title: Compute, profile: [compute]}\n",
        )
        .unwrap();
    let block = gcs_block_json(&m).unwrap();
    assert_eq!(block["entrypoint"], "gcs/index.mjs");
    assert_eq!(block["isolation"], "inline");
    assert_eq!(block["locales"], serde_json::json!(["en"]));
    let c = &block["contributes"];
    assert_eq!(c["panels"], serde_json::json!([{"id": "p"}]));
    assert_eq!(c["settings"], serde_json::json!([{"key": "a"}]));
    assert_eq!(c["overlays"], serde_json::json!([]));
    assert_eq!(
        c["agent_pages"],
        serde_json::json!([{"id": "world", "title": "World", "profile": ["ground-station"]}])
    );
    assert_eq!(
        c["node_surfaces"],
        serde_json::json!([{"id": "overview", "title": "Compute", "profile": ["compute"]}])
    );
    let agent_only = agent_manifest("com.example.x", "").unwrap();
    assert!(gcs_block_json(&agent_only).is_none());
}

fn gcs_manifest_without_entrypoint(contributes: &str) -> Result<PluginManifest, ManifestError> {
    PluginManifest::from_yaml_text(&format!(
            "id: com.example.params\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  isolation: iframe\n{contributes}"
        ))
}

#[test]
fn a_half_with_nothing_to_render_needs_no_bundle() {
    let m = gcs_manifest_without_entrypoint(
        "  contributes:\n    parameters: [{key: gain, type: number}]\n    skills: [{id: boost}]\n",
    )
    .unwrap();
    assert_eq!(m.gcs.as_ref().unwrap().entrypoint, None);
    let block = gcs_block_json(&m).unwrap();
    assert_eq!(block["entrypoint"], serde_json::Value::Null);
    assert_eq!(
        block["contributes"]["parameters"],
        serde_json::json!([{"key": "gain", "type": "number"}])
    );
    // No contributions at all is equally bundle-free.
    assert!(gcs_manifest_without_entrypoint("").is_ok());
    // Empty bundle lists render nothing either.
    assert!(gcs_manifest_without_entrypoint("  contributes:\n    panels: []\n").is_ok());
}

#[test]
fn a_rendered_contribution_without_a_bundle_is_refused() {
    for (contributes, key) in [
        ("  contributes:\n    panels: [{id: p}]\n", "panels"),
        ("  contributes:\n    tabs: [{id: t}]\n", "tabs"),
        (
            "  contributes:\n    agent_pages:\n      - {id: world, title: World}\n",
            "agent_pages",
        ),
        (
            "  contributes:\n    node_surfaces:\n      - {id: s, title: S, profile: [drone]}\n",
            "node_surfaces",
        ),
    ] {
        let err = gcs_manifest_without_entrypoint(contributes).unwrap_err();
        assert!(
            err.0.contains(key) && err.0.contains("gcs.entrypoint"),
            "{key}: {}",
            err.0
        );
    }
}

#[test]
fn map_overlays_mission_templates_settings_and_models_are_projected() {
    let m = gcs_manifest_without_entrypoint(
            "  contributes:\n    map_overlays: [{id: heat}]\n    mission_templates: [{id: grid}]\n    settings: [{key: a}]\n    models: [{id: yolo}]\n",
        )
        .unwrap();
    let c = gcs_block_json(&m).unwrap()["contributes"].clone();
    assert_eq!(c["map_overlays"], serde_json::json!([{"id": "heat"}]));
    assert_eq!(c["mission_templates"], serde_json::json!([{"id": "grid"}]));
    assert_eq!(c["settings"], serde_json::json!([{"key": "a"}]));
    assert_eq!(c["models"], serde_json::json!([{"id": "yolo"}]));
}
