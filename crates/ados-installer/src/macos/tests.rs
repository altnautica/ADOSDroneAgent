use super::health::parse_daemon_state;
use super::identity::{mint_device_id, workstation_config_yaml};
use std::path::Path;

use super::*;

#[test]
fn parse_daemon_state_ignores_nested_endpoint_state_lines() {
    // Real `launchctl print` shape: the job's own `state = running` + `pid`
    // come first, then nested sub-dictionaries carry their own `state =`
    // lines. Reading every `state =` line let a trailing nested
    // `state = active` clobber the job flag to false and read a healthy
    // daemon as down. The parser must take only the first of each.
    let out = "\
gui/501/co.ados.control = {
\tactive count = 1
\tstate = running
\tprogram = /Users/x/.ados/bin/ados-control
\tpid = 82787
\tendpoints = {
\t\t\"co.ados.control.socket\" = {
\t\t\tport = 8080
\t\t\tstate = active
\t\t}
\t}
\tsockets = {
\t\t\"listener\" = {
\t\t\tstate = active
\t\t}
\t}
}";
    assert_eq!(parse_daemon_state(out), Some(82787));

    // A genuinely stopped job (state not running) reads as None even when a
    // stale pid line is present.
    let stopped = "\
gui/501/co.ados.logd = {
\tstate = not running
\tpid = 999
\tendpoints = {
\t\tx = { state = active }
\t}
}";
    assert_eq!(parse_daemon_state(stopped), None);

    // No state line at all (job not loaded) reads as None.
    assert_eq!(parse_daemon_state("gui/501/co.ados.x = {\n}"), None);
}

#[test]
fn profile_defaults_to_workstation_and_rejects_sbc_profiles() {
    let ws = Args::default();
    assert_eq!(resolve_profile(&ws).unwrap(), "workstation");

    let compute = Args {
        profile: Some("compute".to_string()),
        ..Args::default()
    };
    assert_eq!(resolve_profile(&compute).unwrap(), "compute");

    let explicit_ws = Args {
        profile: Some("workstation".to_string()),
        ..Args::default()
    };
    assert_eq!(resolve_profile(&explicit_ws).unwrap(), "workstation");

    // A drone/ground-station profile on a Mac is an error, not a silent default.
    let drone = Args {
        profile: Some("drone".to_string()),
        ..Args::default()
    };
    assert!(resolve_profile(&drone).is_err());
    let gs = Args {
        profile: Some("ground_station".to_string()),
        ..Args::default()
    };
    assert!(resolve_profile(&gs).is_err());
}

#[test]
fn workstation_config_carries_identity() {
    let cfg = workstation_config_yaml("abcd1234", "my-mac", "workstation");
    assert!(cfg.contains("device_id: \"abcd1234\""));
    assert!(cfg.contains("name: \"my-mac\""));
    assert!(cfg.contains("profile: \"workstation\""));
    assert!(cfg.contains("mode: \"local\""));
}

#[test]
fn label_maps_service_to_reverse_dns() {
    assert_eq!(label_for("ados-control"), "co.ados.control");
    assert_eq!(label_for("ados-supervisor"), "co.ados.supervisor");
    // The reverse-DNS prefix is what the plist filename + service target use.
    assert!(label_for("ados-cloud").starts_with("co.ados."));
}

#[test]
fn env_pins_the_control_port_and_home_rooted_paths() {
    let home = PathBuf::from("/Users/tester");
    let paths = Paths {
        bin: home.join(".ados/bin"),
        run: home.join(".ados/run"),
        log: home.join(".ados/log"),
        config: home.join(".ados/config.yaml"),
        profile_conf: home.join(".ados/profile.conf"),
        device_id_file: home.join(".ados/device-id"),
        pairing: home.join(".ados/pairing.json"),
        launch_agents: home.join("Library/LaunchAgents"),
        ados_home: home.join(".ados"),
        home,
    };
    let env = build_env(
        &paths,
        "0011aabbccdd",
        "1.2.3",
        Path::new("/Users/tester/.local/bin/ados-plugin-runner"),
    );
    let get = |k: &str| env.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(get("ADOS_CONTROL_PORT").as_deref(), Some("8080"));
    assert_eq!(
        get("ADOS_CONFIG").as_deref(),
        Some("/Users/tester/.ados/config.yaml")
    );
    assert_eq!(
        get("ADOS_RUN_DIR").as_deref(),
        Some("/Users/tester/.ados/run")
    );
    assert_eq!(get("ADOS_DEVICE_ID").as_deref(), Some("0011aabbccdd"));
    assert_eq!(
        get("ADOS_LOGD_DB").as_deref(),
        Some("/Users/tester/.ados/logd/logs.db")
    );
    // The config path also rides the alternate env var name some daemons read.
    assert_eq!(
        get("ADOS_CONFIG_YAML").as_deref(),
        Some("/Users/tester/.ados/config.yaml")
    );
    // The version the control surface reports for a Rust-only Mac node.
    assert_eq!(get("ADOS_AGENT_VERSION").as_deref(), Some("1.2.3"));
    // The etc/data dirs the control surface reads, all home-rooted so no route
    // falls back to a root-owned Linux path.
    assert_eq!(get("ADOS_ETC_DIR").as_deref(), Some("/Users/tester/.ados"));
    assert_eq!(
        get("ADOS_MODELS_DIR").as_deref(),
        Some("/Users/tester/.ados/models")
    );
    assert_eq!(
        get("ADOS_RECORDINGS_DIR").as_deref(),
        Some("/Users/tester/.ados/recordings")
    );
    assert_eq!(
        get("ADOS_WFB_KEY_DIR").as_deref(),
        Some("/Users/tester/.ados/wfb-keys")
    );
    assert_eq!(
        get("ADOS_INSTALL_RESULT").as_deref(),
        Some("/Users/tester/.ados/install-result.json")
    );
    assert_eq!(
        get("ADOS_SETUP_TOKEN_PATH").as_deref(),
        Some("/Users/tester/.ados/secrets/setup-token")
    );
    // The launchd PATH must carry the Homebrew prefixes so a daemon finds
    // ffmpeg/git.
    assert!(get("PATH").unwrap().contains("/opt/homebrew/bin"));
    // The plugin lifecycle is home-rooted too; its unit dir is the per-user
    // LaunchAgents dir, so a plugin job lands beside the core ones.
    assert_eq!(
        get("ADOS_PLUGIN_UNIT_DIR").as_deref(),
        Some("/Users/tester/Library/LaunchAgents")
    );
    assert_eq!(
        get("ADOS_PLUGIN_RUNNER").as_deref(),
        Some("/Users/tester/.local/bin/ados-plugin-runner")
    );
    for (key, _) in env.iter().filter(|(k, _)| k.starts_with("ADOS_PLUGIN_")) {
        let value = get(key).unwrap();
        assert!(
            value.starts_with("/Users/tester/"),
            "{key}={value} escapes the per-user home"
        );
    }
}

/// The daemons resolve their plugin layout from the plist environment
/// (`Paths::from_env`); the installer's own World Engine step drives
/// `plugin_paths` directly. A misspelled variable would leave a daemon on a
/// root-owned Linux default while the installer wrote the per-user tree.
#[test]
fn the_plist_environment_resolves_to_the_layout_the_installer_drives() {
    let home = PathBuf::from("/Users/tester");
    let paths = Paths {
        bin: home.join(".ados/bin"),
        run: home.join(".ados/run"),
        log: home.join(".ados/log"),
        config: home.join(".ados/config.yaml"),
        profile_conf: home.join(".ados/profile.conf"),
        device_id_file: home.join(".ados/device-id"),
        pairing: home.join(".ados/pairing.json"),
        launch_agents: home.join("Library/LaunchAgents"),
        ados_home: home.join(".ados"),
        home,
    };
    let runner = Path::new("/Users/tester/.local/bin/ados-plugin-runner");
    let env = build_env(&paths, "0011aabbccdd", "1.2.3", runner);
    let keys: Vec<&(String, String)> = env
        .iter()
        .filter(|(k, _)| {
            k.starts_with("ADOS_PLUGIN_") || k == "ADOS_RUN_DIR" || k == "ADOS_DEVICE_ID_PATH"
        })
        .collect();
    // No other test in this crate reads these variables.
    let saved: Vec<(String, Option<std::ffi::OsString>)> = keys
        .iter()
        .map(|(k, _)| (k.clone(), std::env::var_os(k)))
        .collect();
    for (k, v) in &keys {
        std::env::set_var(k, v);
    }
    let resolved = ados_plugin_host::Paths::from_env();
    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(&k, v),
            None => std::env::remove_var(&k),
        }
    }
    assert_eq!(
        format!("{resolved:?}"),
        format!("{:?}", paths.plugin_paths(runner))
    );
}

#[test]
fn plugin_agents_are_found_by_label_and_never_confused_with_the_host() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "co.ados.plugin.com-altnautica-world-engine.plist",
        "co.ados.plugin.com-example-x.plist",
        // The core plugin host daemon, not a plugin.
        "co.ados.plugin-host.plist",
        "co.ados.control.plist",
        "com.other.agent.plist",
    ] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    assert_eq!(
        plugin_agent_labels(dir.path()),
        vec![
            "co.ados.plugin.com-altnautica-world-engine".to_string(),
            "co.ados.plugin.com-example-x".to_string(),
        ]
    );
    assert!(plugin_agent_labels(&dir.path().join("absent")).is_empty());
}

#[test]
fn mint_device_id_is_12_hex() {
    let id = mint_device_id();
    assert_eq!(id.len(), 12);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn agent_version_parses_dunder_version() {
    let dir = tempfile::tempdir().unwrap();
    let init = dir.path().join("src/ados/__init__.py");
    std::fs::create_dir_all(init.parent().unwrap()).unwrap();
    std::fs::write(
        &init,
        "\"\"\"doc\"\"\"\n\n__version__ = \"0.99.86\"\n\nfoo = 1\n",
    )
    .unwrap();
    assert_eq!(agent_version_from_source(dir.path()), "0.99.86");

    // A tree with no package version reads as unknown, never a panic.
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(agent_version_from_source(empty.path()), "unknown");
}

#[test]
fn install_result_records_health_and_lands_at_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let paths = Paths {
        bin: home.join(".ados/bin"),
        run: home.join(".ados/run"),
        log: home.join(".ados/log"),
        config: home.join(".ados/config.yaml"),
        profile_conf: home.join(".ados/profile.conf"),
        device_id_file: home.join(".ados/device-id"),
        pairing: home.join(".ados/pairing.json"),
        launch_agents: home.join("Library/LaunchAgents"),
        ados_home: home.join(".ados"),
        home,
    };
    std::fs::create_dir_all(&paths.ados_home).unwrap();

    // A clean install records status ok with no failed steps.
    let ok = HealthReport {
        up: true,
        failures: vec![],
    };
    write_macos_install_result(&paths, "workstation", "1.2.3", &ok);
    let body = std::fs::read_to_string(paths.install_result()).unwrap();
    assert!(body.contains("\"status\": \"ok\""));
    assert!(body.contains("\"version\": \"1.2.3\""));
    assert!(body.contains("\"profile\": \"workstation\""));
    assert!(body.contains("\"failedSteps\": []"));

    // A failed health gate records status failed and carries the reasons as
    // both failed + required steps (so the recorded status is `failed`).
    let bad = HealthReport {
        up: false,
        failures: vec!["ados-logd: restarting".to_string()],
    };
    write_macos_install_result(&paths, "workstation", "1.2.3", &bad);
    let body = std::fs::read_to_string(paths.install_result()).unwrap();
    assert!(body.contains("\"status\": \"failed\""));
    assert!(body.contains("ados-logd: restarting"));
}

#[test]
fn status_profile_prefers_flag_then_conf_then_default() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let paths = Paths {
        bin: home.join(".ados/bin"),
        run: home.join(".ados/run"),
        log: home.join(".ados/log"),
        config: home.join(".ados/config.yaml"),
        profile_conf: home.join(".ados/profile.conf"),
        device_id_file: home.join(".ados/device-id"),
        pairing: home.join(".ados/pairing.json"),
        launch_agents: home.join("Library/LaunchAgents"),
        ados_home: home.join(".ados"),
        home,
    };
    std::fs::create_dir_all(&paths.ados_home).unwrap();

    // No flag, no conf → the default.
    let none = Args::default();
    assert_eq!(status_profile(&none, &paths), "workstation");

    // profile.conf wins when no flag.
    std::fs::write(&paths.profile_conf, "profile: compute\n").unwrap();
    assert_eq!(status_profile(&none, &paths), "compute");

    // An explicit flag wins over the conf.
    let flagged = Args {
        profile: Some("workstation".to_string()),
        ..Args::default()
    };
    assert_eq!(status_profile(&flagged, &paths), "workstation");
}
