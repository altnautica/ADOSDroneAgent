//! Lints over the shipped unit files in `data/systemd`.

use super::*;

#[test]
fn every_unit_supplementary_group_is_provisioned_or_base() {
    // Guard against drift: every group named in a unit's SupplementaryGroups=
    // must be one the installer provisions (ADOS_HARDWARE_GROUPS) or a group
    // present on every Debian/Armbian image (base-passwd, or created by
    // systemd-udev/bluez). A new unit referencing an unprovisioned, non-base
    // group would fail 216/GROUP on a minimal image.
    const BASE_GROUPS: &[&str] = &[
        "dialout",
        "video",
        "audio",
        "plugdev",
        "input",
        "bluetooth",
        "tty",
        "disk",
        "cdrom",
    ];
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).expect("data/systemd is readable") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("service") {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap();
        for line in body.lines() {
            if let Some(rest) = line.trim().strip_prefix("SupplementaryGroups=") {
                for grp in rest.split_whitespace() {
                    checked += 1;
                    assert!(
                        ADOS_HARDWARE_GROUPS.contains(&grp) || BASE_GROUPS.contains(&grp),
                        "unit {:?} references group {:?} that is neither provisioned by the \
                         installer (ADOS_HARDWARE_GROUPS) nor a base-passwd group; it will \
                         fail 216/GROUP on a minimal image",
                        path.file_name().unwrap(),
                        grp,
                    );
                }
            }
        }
    }
    assert!(
        checked > 0,
        "expected SupplementaryGroups= in at least one unit"
    );
}

#[test]
fn plugin_host_unit_runs_the_native_binary_directly() {
    // The shipped unit must exec the native binary with no marker branch --
    // there is no packaged host to select any more -- and must carry an
    // [Install] section so the supervisor pulls it up.
    let unit =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd/ados-plugin-host.service");
    // Read unconditionally. Swallowing a read failure here means a unit that
    // moved or was deleted passes this test in silence, which is the state
    // it exists to prevent.
    let body = std::fs::read_to_string(&unit)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", unit.display()));
    assert!(
        body.contains("ExecStart=/opt/ados/bin/ados-plugin-host"),
        "{body}"
    );
    // Placement is checked by `every_unit_directive_sits_in_a_section_that_reads_it`;
    // a bare `contains` here would pass with the directive in [Service],
    // where systemd ignores it.
    assert!(
        body.contains("ConditionPathExists=/opt/ados/bin/ados-plugin-host"),
        "{body}"
    );
    assert!(body.contains("WantedBy=ados-supervisor.service"), "{body}");
    // Neither retired marker may appear: the unit selects nothing now.
    assert!(!body.contains("plugin-host-rust-enabled"), "{body}");
    assert!(!body.contains("plugin-host-python-fallback"), "{body}");
}

/// Every shipped unit's directives must sit in a section that reads them.
///
/// systemd does not reject a misplaced directive; it logs "Unknown key name
/// … ignoring" at load and carries on. So a `ConditionPathExists` written
/// under `[Service]` is not a guard at all, and the unit behaves as though
/// it were never written — which is indistinguishable, from every surface,
/// from a unit that simply has no condition.
///
/// This is a denylist of the directives that are section-specific AND
/// silently ignored when misplaced, rather than an allowlist of every valid
/// key: an allowlist would reject a legitimately new directive and would
/// need updating on every systemd release, whereas this only has to know
/// about keys whose misplacement is undetectable at runtime.
#[test]
fn every_unit_directive_sits_in_a_section_that_reads_it() {
    // (directive, the only section that reads it)
    const UNIT_ONLY: &[&str] = &[
        "After",
        "Before",
        "Requires",
        "Requisite",
        "Wants",
        "BindsTo",
        "PartOf",
        "Conflicts",
        "Description",
        "Documentation",
        "DefaultDependencies",
        "StartLimitBurst",
        "StartLimitInterval",
        "StartLimitIntervalSec",
        "OnFailure",
        "RefuseManualStart",
        "RefuseManualStop",
    ];
    const INSTALL_ONLY: &[&str] = &["WantedBy", "RequiredBy", "Also", "Alias", "DefaultInstance"];

    // Directives read ONLY by `[Service]`, in any unit type. A `.socket` has
    // no `ExecStart=`; a `.timer` has no `Restart=`.
    const SERVICE_ONLY: &[&str] = &[
        "ExecStart",
        "ExecStop",
        "ExecReload",
        "Restart",
        "RestartSec",
        "RemainAfterExit",
        "WatchdogSec",
    ];

    // Exec-context directives, read by `[Service]`, `[Socket]`, `[Mount]` and
    // `[Swap]` -- but by none of `[Unit]`, `[Install]`, `[Timer]`, `[Slice]`
    // or a `.target`. `Type=` rides here because it is valid in both
    // `[Service]` and `[Mount]` (where it names the filesystem type).
    const EXEC_CONTEXT: &[&str] = &[
        "ExecStartPre",
        "ExecStartPost",
        "ExecStopPost",
        "TimeoutStartSec",
        "TimeoutStopSec",
        "Type",
        "User",
        "Group",
        "EnvironmentFile",
    ];

    /// The sections that actually read `key`, or `None` when the key is not
    /// one whose misplacement systemd swallows.
    ///
    /// Modelled per-directive rather than per-unit-type. Mapping a unit type
    /// to "its own section" reads plausibly and is wrong in both directions:
    /// a `[Timer]`, `[Slice]` or `.target` reads NONE of these, so an
    /// `ExecStart=` copied into one is ignored by systemd and would have gone
    /// unflagged, and `[Socket]` reads the exec-context keys but not
    /// `ExecStart=`/`Restart=`. Both are the same silent-pass this test exists
    /// to close, moved one unit type over.
    fn sections_reading(key: &str) -> Option<&'static [&'static str]> {
        if key.starts_with("Condition") || key.starts_with("Assert") {
            return Some(&["[Unit]"]);
        }
        if UNIT_ONLY.contains(&key) {
            return Some(&["[Unit]"]);
        }
        if INSTALL_ONLY.contains(&key) {
            return Some(&["[Install]"]);
        }
        if SERVICE_ONLY.contains(&key) {
            return Some(&["[Service]"]);
        }
        if EXEC_CONTEXT.contains(&key) {
            return Some(&["[Service]", "[Socket]", "[Mount]", "[Swap]"]);
        }
        None
    }

    /// Flag every directive in `body` sitting in a section that does not read
    /// it. Pure over the text so the corpus scan and the synthetic-unit test
    /// below run the same code -- the shipped corpus is 44 `.service` files
    /// and one `.slice`, so without this seam the socket / timer / target
    /// paths would have no committed coverage at all.
    fn misplaced_directives(name: &str, body: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut section = String::new();
        for line in body.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                section = line.to_string();
                continue;
            }
            if line.starts_with('#') || line.starts_with(';') || !line.contains('=') {
                continue;
            }
            let key = line.split('=').next().unwrap_or("").trim();
            let Some(allowed) = sections_reading(key) else {
                continue;
            };
            if !allowed.contains(&section.as_str()) {
                let want = allowed.join(" or ");
                let where_ = if section.is_empty() {
                    "before any section header".to_string()
                } else {
                    section.clone()
                };
                out.push(format!(
                    "  {name}: {key}= is in {where_} but only {want} reads it"
                ));
            }
        }
        out
    }

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));

    let mut checked = 0usize;
    let mut problems: Vec<String> = Vec::new();

    for entry in entries {
        let path = entry.expect("dir entry").path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(
            ext,
            "service" | "socket" | "timer" | "slice" | "target" | "mount"
        ) {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        checked += 1;

        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        problems.extend(misplaced_directives(name, &body));
    }

    // The corpus is all .service today, so exercise the other unit types on
    // synthetic bodies. Without these the socket / timer / target arms have
    // no committed coverage and the by-hand proof that motivated this shape
    // lives only in a commit message.
    let cases: &[(&str, &str, bool)] = &[
        // A .socket reads the exec-context keys: not a misplacement.
        (
            "ok.socket",
            "[Unit]\nConditionPathExists=/x\n\n[Socket]\nListenStream=/run/x.sock\n\
             User=ados\nGroup=ados\nExecStartPost=/bin/true\nTimeoutStartSec=30\n\
             \n[Install]\nWantedBy=sockets.target\n",
            false,
        ),
        // ...but it does NOT read ExecStart=/Restart=; systemd ignores both.
        (
            "bad.socket",
            "[Unit]\nDescription=x\n\n[Socket]\nListenStream=/run/x.sock\n\
             ExecStart=/bin/true\nRestart=always\n",
            true,
        ),
        // A .target reads none of them, so a copied service template is dead.
        (
            "bad.target",
            "[Unit]\nDescription=x\n\n[Install]\nExecStart=/bin/true\nUser=ados\n",
            true,
        ),
        // A [Timer] reads none of them either.
        (
            "bad.timer",
            "[Unit]\nDescription=x\n\n[Timer]\nOnCalendar=daily\nExecStart=/bin/true\n",
            true,
        ),
        // The original shipped defect: a [Unit] condition written under
        // [Service] is not a guard at all.
        (
            "bad.service",
            "[Unit]\nDescription=x\n\n[Service]\n\
             ConditionPathExists=/opt/ados/bin/x\nExecStart=/opt/ados/bin/x\n",
            true,
        ),
    ];
    for (name, body, want_flagged) in cases {
        let got = misplaced_directives(name, body);
        assert_eq!(
            !got.is_empty(),
            *want_flagged,
            "{name}: expected flagged={want_flagged}, got {got:?}"
        );
    }

    // A directory that matched nothing would make this pass vacuously. The
    // floor is a broken-scan tripwire, not a unit-count contract: units do get
    // retired, so a legitimate shrink below this is a deliberate edit here,
    // and the message has to say so rather than accusing the scan.
    assert!(
        // 39 -> 37: ados-discovery, ados-health and ados-wifi-client were
        // removed and ados-vision-hailo was added.
        checked >= 37,
        "parsed only {checked} unit files under {}. Either the scan is broken, \
         or units were legitimately removed -- if the latter, lower this floor \
         deliberately rather than deleting the check",
        dir.display()
    );
    assert!(
        problems.is_empty(),
        "systemd ignores a misplaced directive silently, so these are no-ops \
         that read as configured:\n{}",
        problems.join("\n")
    );
}

/// Every long-running agent unit restarts forever: `Restart=always`,
/// `RestartSec=3`, and `StartLimitIntervalSec=0` in `[Unit]` (the only
/// section that reads it). A start limit latches a unit `failed` after a
/// burst of quick exits and nothing re-arms it until a reboot or an
/// operator's `reset-failed`, which for the supervisor means MAVLink, radio,
/// video and the control surface all stay down. Oneshots are exempt: they
/// run to completion by design.
#[test]
fn every_long_running_unit_restarts_without_a_start_limit() {
    /// The restart-policy violations in one unit body (pure).
    fn restart_policy_problems(name: &str, body: &str) -> Vec<String> {
        let mut section = "";
        let mut kv: Vec<(&str, &str, &str)> = Vec::new();
        for line in body.lines().map(str::trim) {
            if line.starts_with('[') && line.ends_with(']') {
                section = line;
                continue;
            }
            if line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                kv.push((section, k.trim(), v.trim()));
            }
        }
        let has = |sec: &str, key: &str, val: &str| {
            kv.iter()
                .any(|(s, k, v)| *s == sec && *k == key && *v == val)
        };
        if has("[Service]", "Type", "oneshot") {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !has("[Service]", "Restart", "always") {
            out.push(format!("  {name}: [Service] needs Restart=always"));
        }
        if !has("[Service]", "RestartSec", "3") {
            out.push(format!("  {name}: [Service] needs RestartSec=3"));
        }
        if !has("[Unit]", "StartLimitIntervalSec", "0") {
            out.push(format!("  {name}: [Unit] needs StartLimitIntervalSec=0"));
        }
        for (_, k, _) in &kv {
            if *k == "StartLimitBurst" || *k == "StartLimitInterval" {
                out.push(format!("  {name}: {k}= re-imposes a start limit"));
            }
        }
        out
    }

    // The shape that latched: a burst limit and on-failure.
    let latched = "[Unit]\nDescription=x\nStartLimitInterval=60s\nStartLimitBurst=5\n\n\
                   [Service]\nType=simple\nExecStart=/bin/x\nRestart=on-failure\nRestartSec=1\n";
    assert_eq!(restart_policy_problems("latched.service", latched).len(), 5);
    // The limit written under [Service] is ignored by systemd: still flagged.
    let misplaced = "[Unit]\nDescription=x\n\n[Service]\nExecStart=/bin/x\n\
                     Restart=always\nRestartSec=3\nStartLimitIntervalSec=0\n";
    assert_eq!(
        restart_policy_problems("misplaced.service", misplaced).len(),
        1
    );

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    let mut checked = 0usize;
    let mut problems: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("data/systemd is readable") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        if !(name.starts_with("ados-") && name.ends_with(".service")) {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("unit is readable");
        checked += 1;
        problems.extend(restart_policy_problems(name, &body));
    }
    assert!(
        checked >= 30,
        "parsed only {checked} ados-*.service files under {}",
        dir.display()
    );
    assert!(
        problems.is_empty(),
        "these units can give up restarting:\n{}",
        problems.join("\n")
    );
}

/// Every sandbox path a unit names without the `-` (ignore-if-missing)
/// prefix exists when the unit starts: systemd aborts namespace setup with
/// 226/NAMESPACE on an absent one, before any `Exec*` line runs, so the
/// unit crash-loops with nothing in its own log. A strict path is fine when
/// the OS always has it, the installer creates it, or the unit's own
/// `ConditionPathExists=` / `AssertPathExists=` gates the start on it.
#[test]
fn every_strict_sandbox_path_exists_before_its_unit_starts() {
    /// Paths the base OS always has, or the installer creates (the
    /// `/run/ados` tmpfiles entry, the state and data roots, the plugin
    /// and peripheral dirs, and the `/var/log/ados` retention tmpfiles
    /// entry plus the plugin log dir the slice setup creates).
    const PRESENT: &[&str] = &[
        "/dev",
        "/dev/shm",
        "/etc",
        "/etc/systemd/system",
        "/sys/class/net",
        "/tmp",
        "/var/lock",
        "/etc/ados",
        "/etc/ados/peripherals",
        "/run/ados",
        "/var/ados",
        "/var/lib/ados",
        "/var/log/ados",
    ];
    const SANDBOX_KEYS: &[&str] = &[
        "ReadWritePaths",
        "ReadOnlyPaths",
        "InaccessiblePaths",
        "ExecPaths",
        "NoExecPaths",
    ];

    /// The strict sandbox paths in one unit body that nothing guarantees.
    fn unguaranteed_paths(name: &str, body: &str) -> Vec<String> {
        let mut gated: Vec<&str> = Vec::new();
        let mut strict: Vec<&str> = Vec::new();
        for line in body.lines().map(str::trim) {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if key == "ConditionPathExists" || key == "AssertPathExists" {
                gated.push(value.trim());
            } else if SANDBOX_KEYS.contains(&key) {
                strict.extend(
                    value
                        .split_whitespace()
                        .map(|p| p.trim_start_matches('+'))
                        .filter(|p| !p.starts_with('-')),
                );
            }
        }
        strict
            .into_iter()
            .filter(|p| !PRESENT.contains(p) && !gated.contains(p))
            .map(|p| format!("  {name}: {p} needs a `-` prefix or a guarantee"))
            .collect()
    }

    // A strict path nothing creates is flagged; the `-` prefix or a start
    // condition on the path clears it.
    let bad = "[Service]\nReadWritePaths=/run/ados /etc/ados/mesh\n";
    assert_eq!(unguaranteed_paths("bad.service", bad).len(), 1);
    let optional = "[Service]\nReadWritePaths=/run/ados -/etc/ados/mesh\n";
    assert!(unguaranteed_paths("ok.service", optional).is_empty());
    let gated = "[Unit]\nConditionPathExists=/sys/kernel/config/usb_gadget\n\
                 [Service]\nReadWritePaths=/sys/kernel/config/usb_gadget\n";
    assert!(unguaranteed_paths("ok.service", gated).is_empty());

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    let mut checked = 0usize;
    let mut problems: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("data/systemd is readable") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        if !(name.starts_with("ados-") && name.ends_with(".service")) {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("unit is readable");
        checked += 1;
        problems.extend(unguaranteed_paths(name, &body));
    }
    assert!(checked >= 30, "parsed only {checked} units");
    assert!(
        problems.is_empty(),
        "these units fail namespace setup when the path is absent:\n{}",
        problems.join("\n")
    );
}
