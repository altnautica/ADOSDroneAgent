//! The unit table and the shipped unit files name exactly the same units.

use std::collections::BTreeSet;
use std::path::Path;

use ados_protocol::units::{find, role_unit_count, role_unit_names, Install, RETIRED, UNITS};

fn shipped() -> BTreeSet<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "ados.slice")
        .collect()
}

#[test]
fn units_are_exactly_the_shipped_unit_files() {
    let table: BTreeSet<String> = UNITS.iter().map(|u| u.name.to_string()).collect();
    assert_eq!(table.len(), UNITS.len(), "a unit is listed twice");
    assert_eq!(table, shipped(), "UNITS drifted from data/systemd");
}

#[test]
fn no_retired_unit_is_shipped() {
    let shipped = shipped();
    for unit in RETIRED {
        assert!(!shipped.contains(*unit), "{unit} is retired but shipped");
    }
}

#[test]
fn names_are_unique_service_files() {
    for u in UNITS {
        assert!(u.name.starts_with("ados-") && u.name.ends_with(".service"));
        assert!(
            !RETIRED.contains(&u.name),
            "{} is shipped and retired",
            u.name
        );
    }
}

#[test]
fn find_accepts_the_stem_and_the_file_name() {
    assert_eq!(find("ados-wfb").unwrap().name, "ados-wfb.service");
    assert_eq!(find("ados-wfb.service").unwrap().stem(), "ados-wfb");
    assert!(find("ados-radio").is_none());
}

#[test]
fn role_sets_follow_the_table() {
    const RELAY: [&str; role_unit_count(Some("relay"))] = role_unit_names(Some("relay"));
    assert_eq!(RELAY, ["ados-batman.service", "ados-wfb-relay.service"]);
    const ALL: [&str; role_unit_count(None)] = role_unit_names(None);
    assert_eq!(
        ALL,
        [
            "ados-wfb-rx.service",
            "ados-batman.service",
            "ados-wfb-relay.service",
            "ados-wfb-receiver.service",
        ]
    );
}

#[test]
fn profile_and_install_rules_hold() {
    for u in UNITS {
        // A per-profile enable or a teardown needs a profile to act on.
        if u.install == Install::Profile || u.teardown {
            assert!(!u.profiles.is_empty(), "{}", u.name);
        }
        // Role ownership only exists on a ground station.
        if !u.roles.is_empty() {
            assert_eq!(u.profiles, ["ground_station"], "{}", u.name);
        }
    }
}

/// The last value of `key=` in a unit file, as systemd keeps the last one.
fn directive<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    body.lines()
        .filter_map(|l| l.trim().strip_prefix(prefix.as_str()).map(str::trim))
        .next_back()
}

#[test]
fn a_unit_that_exits_zero_when_done_is_not_restarted_for_it() {
    // A unit whose process exits 0 to say "nothing to run" or "done" must not
    // have that exit restarted: `Restart=always` with no
    // `RestartPreventExitStatus=0` respawns it every `RestartSec=` forever.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    let mut checked = 0usize;
    for u in UNITS.iter().filter(|u| u.idle_ok || u.self_terminating) {
        let path = dir.join(u.name);
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let restart = directive(&body, "Restart").unwrap_or("no");
        let prevents_zero = directive(&body, "RestartPreventExitStatus")
            .is_some_and(|v| v.split_whitespace().any(|s| s == "0"));
        assert!(
            !matches!(restart, "always" | "on-success") || prevents_zero,
            "{} exits 0 when it has nothing to do but Restart={restart} restarts that exit",
            u.name
        );
        // An idle exit is told apart from a death by the prevent list, so the
        // supervisor can park it instead of counting a crash.
        if u.idle_ok {
            assert!(
                prevents_zero,
                "{} is idle_ok without RestartPreventExitStatus=0",
                u.name
            );
        }
        checked += 1;
    }
    assert!(checked > 0);
}
