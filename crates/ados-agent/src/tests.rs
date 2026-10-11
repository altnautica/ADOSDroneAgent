use super::*;

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

/// Binaries that ship standalone by design and are never applets.
fn is_standalone(name: &str) -> bool {
    name == "ados-installer" || name == "ados-vision" || name.starts_with("ados-vision-onnx")
}

#[test]
fn dispatch_by_argv0() {
    assert_eq!(
        resolve(&args(&["/opt/ados/bin/ados-video"])),
        Dispatch::Run("ados-video")
    );
    assert_eq!(
        resolve(&args(&["ados-groundlink", "--role", "relay"])).clone_name(),
        resolve(&args(&["/x/ados-groundlink"])).clone_name()
    );
    assert_eq!(
        resolve(&args(&["/opt/ados/bin/ados-agent", "video"])),
        Dispatch::Shifted("ados-video")
    );
    assert_eq!(
        resolve(&args(&["ados-agent", "ados-tunnel-config"])),
        Dispatch::Shifted("ados-tunnel-config")
    );
}

#[test]
fn unknown_applet_exits_64() {
    for argv in [
        &["ados-agent", "no-such-daemon"][..],
        &["ados-agent"][..],
        &["/opt/ados/bin/ados-bogus"][..],
    ] {
        assert!(matches!(resolve(&args(argv)), Dispatch::Unknown(_)));
        assert_eq!(dispatch(&args(argv)), ExitCode::from(64));
    }
}

#[test]
fn applet_names_are_unique() {
    let mut names: Vec<&str> = APPLETS.iter().map(|(n, _)| *n).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len());
}

#[test]
fn every_unit_execstart_has_an_applet() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("data/systemd") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("service") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("unit file");
        for line in text
            .lines()
            .filter(|l| l.trim_start().starts_with("ExecStart="))
        {
            for (i, _) in line.match_indices("/opt/ados/bin/") {
                let tail = &line[i + "/opt/ados/bin/".len()..];
                let name: String = tail
                    .chars()
                    .take_while(|c| {
                        c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.'
                    })
                    .collect();
                // Shell helpers and non-Rust entries are not daemons.
                if !name.starts_with("ados-") || name.contains('.') || is_standalone(&name) {
                    continue;
                }
                assert!(
                    APPLETS.iter().any(|(n, _)| *n == name),
                    "{} execs {name}, which has no applet",
                    path.display()
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 0,
        "no Rust daemon ExecStart found under {}",
        dir.display()
    );
}

impl Dispatch {
    fn clone_name(&self) -> String {
        match self {
            Dispatch::Run(n) | Dispatch::Shifted(n) => (*n).to_owned(),
            Dispatch::Unknown(s) => s.clone(),
        }
    }
}
