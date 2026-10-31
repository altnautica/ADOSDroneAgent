use super::launchd::LAUNCHD_ENTRY_SCRIPT;
use super::*;

fn spec() -> UnitSpec {
    UnitSpec {
        description: "ADOS plugin com.example.x".to_string(),
        argv: vec![
            "/var/ados/plugins/com.example.x/bin/x".to_string(),
            "com.example.x".to_string(),
        ],
        working_dir: None,
        env: vec![(
            "ADOS_PLUGIN_SOCKET".to_string(),
            "/run/ados/plugins/com.example.x/host.sock".to_string(),
        )],
        user: "ados-plg-0a1b2c3d".to_string(),
        credential: Some(PathBuf::from("/run/ados/plugins/com.example.x.token")),
        bind_read_only: vec![PathBuf::from("/run/ados/plugins/com.example.x")],
        bind_read_write: Vec::new(),
        exec_paths: vec![
            "/var/ados/plugins/com.example.x".to_string(),
            "-/usr/lib".to_string(),
        ],
        guard_loader: Some(PathBuf::from("/opt/ados/bin/ados-plugin-host")),
        log_path: PathBuf::from("/var/log/ados/plugins/com-example-x.log"),
        restart: "on-failure".to_string(),
        resources: ResourceLimits::default(),
        sandbox_directives: vec!["PrivateDevices=yes".to_string()],
    }
}

#[test]
fn a_unit_runs_as_its_own_user_after_the_guard_with_a_credential_and_no_exec_elsewhere() {
    let unit = render_systemd(&spec());
    assert!(unit.contains("\nUser=ados-plg-0a1b2c3d\nGroup=ados-plugins\n"));
    assert!(
        unit.contains("\nLoadCredential=ados-plugin-token:/run/ados/plugins/com.example.x.token\n")
    );
    assert!(!unit.contains("EnvironmentFile="));
    assert!(unit.contains("\nRequires=ados-plugin-host.service\n"));
    assert!(unit.contains("\nAfter=ados-supervisor.service ados-plugin-host.service\n"));
    let pre = unit
        .find("\nExecStartPre=+/opt/ados/bin/ados-plugin-host guard-load\n")
        .expect("guard-load pre-start");
    assert!(pre < unit.find("\nExecStart=").unwrap());
    assert!(unit.contains("\nNoExecPaths=/\nExecPaths=/var/ados/plugins/com.example.x -/usr/lib\n"));
    let probe = systemd_run_argv(&ProbeSpec {
        argv: vec!["bin/check".to_string()],
        working_dir: PathBuf::from("/var/ados/plugins/com.example.x"),
        user: "ados-plg-0a1b2c3d".to_string(),
        resources: ResourceLimits::default(),
        exec_paths: spec().exec_paths,
        sandbox_directives: Vec::new(),
    });
    assert!(probe.contains(&"--uid=ados-plg-0a1b2c3d".to_string()));
    assert!(probe.contains(&"--gid=ados-plugins".to_string()));
    assert!(probe.contains(&"--property=NoExecPaths=/".to_string()));
}

type Calls = Arc<Mutex<Vec<String>>>;

fn recorder() -> (Calls, CommandRunner) {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let sink = calls.clone();
    let run: CommandRunner = Arc::new(move |program, args| {
        sink.lock().push(format!("{program} {}", args.join(" ")));
        Ok(())
    });
    (calls, run)
}

#[test]
fn systemd_writes_the_slice_and_unit_and_reloads_only_on_change() {
    let dir = tempfile::tempdir().unwrap();
    let (calls, run) = recorder();
    let backend = SystemdBackend::new(dir.path()).with_runner(run);
    let name = "ados-plugin-com-example-x.service";

    assert!(backend.install(name, &spec()).unwrap());
    assert!(dir.path().join(PLUGIN_SLICE_NAME).exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join(name)).unwrap(),
        render_systemd(&spec())
    );
    // Same spec again: nothing written, no reload.
    calls.lock().clear();
    assert!(!backend.install(name, &spec()).unwrap());
    assert!(calls.lock().is_empty());

    backend.enable_start(name).unwrap();
    backend.stop_disable(name).unwrap();
    backend.uninstall(name).unwrap();
    assert!(!dir.path().join(name).exists());
    assert_eq!(
        *calls.lock(),
        vec![
            format!("systemctl enable {name}"),
            format!("systemctl start {name}"),
            format!("systemctl stop {name}"),
            format!("systemctl disable {name}"),
            "systemctl daemon-reload".to_string(),
        ]
    );
}

#[test]
fn launchd_renders_a_plist_that_reads_the_token_file_at_each_start() {
    let dir = tempfile::tempdir().unwrap();
    let (calls, run) = recorder();
    let backend = LaunchdBackend::new(dir.path()).with_runner(run);
    let name = "ados-plugin-com-example-x.service";
    assert_eq!(
        LaunchdBackend::label_for(name),
        "co.ados.plugin.com-example-x"
    );
    assert!(backend.install(name, &spec()).unwrap());
    let plist =
        std::fs::read_to_string(dir.path().join("co.ados.plugin.com-example-x.plist")).unwrap();
    assert!(plist.contains("<string>co.ados.plugin.com-example-x</string>"));
    assert!(plist.contains("<string>/bin/sh</string>"));
    assert!(plist.contains("<string>/run/ados/plugins/com.example.x.token</string>"));
    assert!(plist.contains("<key>ADOS_PLUGIN_SOCKET</key>"));
    assert!(plist
        .contains("<key>StandardOutPath</key>\n\t<string>/var/log/ados/plugins/com-example-x.log"));
    assert!(!backend.enforces_sandbox());

    backend.enable_start(name).unwrap();
    let recorded = calls.lock().clone();
    let target = format!(
        "gui/{}/co.ados.plugin.com-example-x",
        ados_protocol::launchd::current_uid()
    );
    assert_eq!(recorded[0], format!("launchctl enable {target}"));
    assert!(recorded[1].starts_with("launchctl bootstrap gui/"));
    assert!(recorded[1].ends_with("co.ados.plugin.com-example-x.plist"));
    assert_eq!(recorded[2], format!("launchctl kickstart {target}"));
}

#[test]
fn the_launchd_entry_script_exports_the_token_file_verbatim() {
    // The token is `a|b=c` style text: nothing in it may be evaluated.
    let dir = tempfile::tempdir().unwrap();
    let envf = dir.path().join("x.token");
    std::fs::write(&envf, "ADOS_PLUGIN_TOKEN=a|b=c;$(false)\nADOS_X=1").unwrap();
    let out = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            LAUNCHD_ENTRY_SCRIPT,
            "ados-plugin",
            &dir.path().display().to_string(),
            &envf.display().to_string(),
            "/bin/sh",
            "-c",
            "printf '%s|%s|%s' \"$ADOS_PLUGIN_TOKEN\" \"$ADOS_X\" \"$(pwd -P)\"",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let real = dir.path().canonicalize().unwrap();
    assert_eq!(text, format!("a|b=c;$(false)|1|{}", real.display()));
}
