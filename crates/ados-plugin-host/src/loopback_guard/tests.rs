use super::*;
use ados_protocol::plugin_loopback_guard::{read_state_at, SIDECAR_NAME};

fn rule_lines(rules: &str) -> Vec<&str> {
    rules
        .lines()
        .map(str::trim)
        .filter(|l| l.ends_with(" drop") || l.ends_with(" accept"))
        .collect()
}

#[test]
fn every_local_destination_is_dropped_for_the_plugin_group_on_tcp_and_udp() {
    let rules = render_ruleset(&[], &agent_tcp_ports(DEFAULT_MAVLINK_WS_PORT));
    assert!(
        rules.starts_with("table inet ados_plugin_guard\ndelete table inet ados_plugin_guard\n")
    );
    assert!(rules.contains("type filter hook output priority 0; policy accept;"));
    let lines = rule_lines(&rules);
    for proto in ["tcp", "udp"] {
        assert!(
            lines.contains(
                &format!(
                    "meta skgid \"ados-plugins\" fib daddr type local meta l4proto {proto} drop"
                )
                .as_str()
            ),
            "{proto} drop missing: {rules}"
        );
    }
    // No port list scopes the drop: the aux, hop and second raw MAVLink
    // ports are covered by the address class, not by enumeration.
    for line in lines.iter().filter(|l| l.ends_with(" drop")) {
        assert!(!line.contains("dport"), "{line}");
    }
    // The drops are the last rules, so no accept can follow and widen them.
    assert!(lines.last().unwrap().ends_with("udp drop"));
}

#[test]
fn a_plugin_reaches_only_its_own_declared_ports_and_never_an_agent_port() {
    let rules = render_ruleset(
        &[
            ListenException {
                uid: 990,
                tcp_ports: vec![9100, 5760, 8080],
            },
            ListenException {
                uid: 991,
                tcp_ports: vec![8080],
            },
        ],
        &agent_tcp_ports(DEFAULT_MAVLINK_WS_PORT),
    );
    let lines = rule_lines(&rules);
    assert!(lines.contains(&"meta skuid 990 fib daddr type local tcp dport { 9100 } accept"));
    // A plugin whose only declared port is an agent port gets no exception.
    assert!(!lines.iter().any(|l| l.contains("skuid 991")));
    let accept_at = lines.iter().position(|l| l.contains("skuid 990")).unwrap();
    let drop_at = lines.iter().position(|l| l.ends_with("tcp drop")).unwrap();
    assert!(accept_at < drop_at);
    // DNS is the one fixed exception, for both protocols.
    assert!(lines.contains(&"meta skgid \"ados-plugins\" fib daddr type local udp dport 53 accept"));
}

#[test]
fn the_configured_websocket_port_joins_the_agent_set() {
    let yaml = "mavlink:\n  endpoints:\n    - type: websocket\n      host: 0.0.0.0\n      port: 9100\n      enabled: true\n";
    assert_eq!(configured_ws_port(yaml), 9100);
    assert_eq!(
        configured_ws_port("agent:\n  name: x\n"),
        DEFAULT_MAVLINK_WS_PORT
    );
    let disabled =
        "mavlink:\n  endpoints:\n    - type: websocket\n      port: 9100\n      enabled: false\n";
    assert_eq!(configured_ws_port(disabled), DEFAULT_MAVLINK_WS_PORT);
    assert!(agent_tcp_ports(9100).contains(&9100));
    assert_eq!(
        agent_tcp_ports(8080).iter().filter(|p| **p == 8080).count(),
        1
    );
}

#[test]
fn an_endpoint_without_a_type_is_the_schema_default_websocket() {
    let yaml = "mavlink:\n  endpoints:\n    - port: 9100\n";
    assert_eq!(configured_ws_port(yaml), 9100);
}

#[test]
fn the_sidecar_round_trips_a_loaded_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(SIDECAR_NAME);
    write_sidecar(
        &path,
        &GuardState {
            active: true,
            reason: String::new(),
        },
    )
    .unwrap();
    assert!(read_state_at(&path).active);
}

fn loaded(_ruleset: &str) -> GuardState {
    GuardState {
        active: true,
        reason: String::new(),
    }
}

#[test]
fn install_records_the_verdict_under_a_run_dir_that_does_not_exist_yet() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path().join("run").join("ados");
    let path = run.join(SIDECAR_NAME);
    let rules = render_ruleset(&[], &agent_tcp_ports(DEFAULT_MAVLINK_WS_PORT));
    assert!(install_with(&rules, &path, loaded).active);
    assert!(read_state_at(&path).active);
    let names: Vec<_> = std::fs::read_dir(&run)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [SIDECAR_NAME], "no temp file left behind");
}

#[test]
fn concurrent_installs_never_share_a_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(SIDECAR_NAME);
    let rules = render_ruleset(&[], &agent_tcp_ports(DEFAULT_MAVLINK_WS_PORT));
    let (rules, path) = (rules.as_str(), path.as_path());
    std::thread::scope(|s| {
        let writers: Vec<_> = (0..8)
            .map(|_| s.spawn(move || install_with(rules, path, loaded)))
            .collect();
        for w in writers {
            assert!(w.join().unwrap().active);
        }
    });
    assert!(read_state_at(path).active);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn an_unavailable_verdict_is_recorded_as_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(SIDECAR_NAME);
    std::fs::write(&path, br#"{"active":true,"reason":""}"#).unwrap();
    let state = install_with("", &path, |_| GuardState::unavailable("nft: missing"));
    assert!(!state.active);
    assert!(!read_state_at(&path).active);
}
