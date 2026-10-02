//! The loopback guard: keeps a plugin granted `network.outbound` off every
//! listener on the node itself.
//!
//! The agent's HTTP front, the MAVLink WebSocket and raw proxies, the aux and
//! hop control ports, the media server and the local query endpoints treat a
//! peer on the node as on-box (a loopback peer) or as a lifeline peer (the
//! node's own access-point, USB or link-local address). A plugin that may use
//! the network must not inherit either trust. systemd's `IPAddressDeny=` cannot
//! express it: the address filter applies to the unit's ingress as well, so it
//! would also cut the host's `ready_check` probe and a plugin's own listeners.
//!
//! So the rule is inverted rather than enumerated: an nftables output rule
//! drops every TCP and UDP packet a plugin process sends to any address that
//! is local to this node (`fib daddr type local`: loopback, the AP and USB
//! gadget addresses, link-local, every interface address). A port list would
//! go stale the moment an operator moved an aux port or a new service bound
//! one; the address class cannot.
//!
//! The only exceptions are explicit and narrow:
//!
//! * DNS (port 53), so a network-capable plugin can resolve names through a
//!   local stub resolver. No agent service listens there.
//! * A plugin's own declared `listen_ports`, for that plugin's uid only, so a
//!   plugin can reach the service it serves. An agent-owned port is never
//!   excepted, whatever a manifest declares.
//!
//! The match is the socket owner's group (`meta skgid`), not the plugin slice's
//! cgroup. Every plugin process (main unit, declared service, readiness probe)
//! runs as its own per-plugin user whose primary group is
//! [`PLUGIN_GROUP`](ados_protocol::ipc::PLUGIN_GROUP), which nothing else on
//! the box uses, and a gid is stable. A cgroup match binds the rule to the
//! slice directory's inode at load time; systemd creates and prunes that
//! directory as plugins come and go, and a recreated directory would leave the
//! rule silently matching nothing.
//!
//! The daemon loads the table at startup, and every plugin unit reloads it in a
//! privileged `ExecStartPre` (`ados-plugin-host guard-load`) before its process
//! starts, so the guard is in place for every plugin start whatever the boot
//! order, and a load failure fails the unit closed. The verdict is recorded in
//! a run-dir sidecar every grant path and renderer reads. When the rule cannot
//! load, `network.outbound` is refused at grant time and a unit rendered for an
//! earlier grant keeps `IPAddressDeny=any`: without the guard, the capability
//! is not safe to hold.

use std::path::Path;

use ados_protocol::ipc::PLUGIN_GROUP;
use ados_protocol::plugin_loopback_guard::GuardState;
use serde::Deserialize;

/// The nftables table the guard owns.
pub const TABLE: &str = "ados_plugin_guard";

/// Agent-owned TCP listeners: the control front and its alternate port, the raw
/// MAVLink TCP proxy, the logging query endpoint, the compute job API, and the
/// media server's RTSP, HLS, WebRTC, playback, API, metrics and profiling
/// listeners. The guard drops every local destination regardless; this list
/// only keeps a manifest's `listen_ports` from excepting one of them. The
/// MAVLink WebSocket port is operator-configurable and joins the set at load.
pub const AGENT_TCP_PORTS: &[u16] = &[
    8080, 8082, 5760, 8090, 8092, 8554, 8888, 8889, 9996, 9997, 9998, 9999,
];

/// The default MAVLink WebSocket proxy port.
pub const DEFAULT_MAVLINK_WS_PORT: u16 = 8765;

/// The DNS port a local stub resolver answers on.
pub const DNS_PORT: u16 = 53;

/// One plugin's own listeners, reachable by that plugin's processes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenException {
    /// The plugin's system user id.
    pub uid: u32,
    /// The TCP ports its declared services listen on.
    pub tcp_ports: Vec<u16>,
}

/// The agent-owned TCP set: the fixed listeners plus the configured MAVLink
/// WebSocket port, sorted and de-duplicated.
pub fn agent_tcp_ports(ws_port: u16) -> Vec<u16> {
    let mut ports: Vec<u16> = AGENT_TCP_PORTS.to_vec();
    ports.push(ws_port);
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn port_set(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render the ruleset. The leading `table` + `delete table` pair makes a reload
/// replace the table atomically whether or not it already exists.
///
/// `exceptions` are the plugins' own listeners; any port in `agent_tcp` is
/// removed from them, and an exception left with no port renders nothing.
/// Exceptions render in uid order so an unchanged install set renders a
/// byte-identical ruleset.
pub fn render_ruleset(exceptions: &[ListenException], agent_tcp: &[u16]) -> String {
    let group = format!("meta skgid \"{PLUGIN_GROUP}\"");
    let local = "fib daddr type local";
    let mut rules: Vec<String> = vec![
        format!("{group} {local} udp dport {DNS_PORT} accept"),
        format!("{group} {local} tcp dport {DNS_PORT} accept"),
    ];
    let mut sorted: Vec<&ListenException> = exceptions.iter().collect();
    sorted.sort_by_key(|e| e.uid);
    for exception in sorted {
        let mut ports: Vec<u16> = exception
            .tcp_ports
            .iter()
            .copied()
            .filter(|p| !agent_tcp.contains(p) && *p != DNS_PORT)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        if ports.is_empty() {
            continue;
        }
        rules.push(format!(
            "meta skuid {} {local} tcp dport {{ {} }} accept",
            exception.uid,
            port_set(&ports)
        ));
    }
    rules.push(format!("{group} {local} meta l4proto tcp drop"));
    rules.push(format!("{group} {local} meta l4proto udp drop"));
    let body: String = rules.iter().map(|r| format!("\t\t{r}\n")).collect();
    format!(
        "table inet {TABLE}\n\
         delete table inet {TABLE}\n\
         table inet {TABLE} {{\n\
         \tchain output {{\n\
         \t\ttype filter hook output priority 0; policy accept;\n\
         {body}\
         \t}}\n\
         }}\n"
    )
}

/// The MAVLink WebSocket port from the agent config (`mavlink.endpoints`, the
/// first enabled `websocket` entry), or the default when none is configured.
pub fn configured_ws_port(yaml: &str) -> u16 {
    #[derive(Deserialize, Default)]
    struct Cfg {
        #[serde(default)]
        mavlink: Mav,
    }
    #[derive(Deserialize, Default)]
    struct Mav {
        #[serde(default)]
        endpoints: Option<Vec<Endpoint>>,
    }
    #[derive(Deserialize)]
    struct Endpoint {
        #[serde(rename = "type", default)]
        kind: String,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default = "enabled_default")]
        enabled: bool,
    }
    fn enabled_default() -> bool {
        true
    }
    serde_norway::from_str::<Cfg>(yaml)
        .ok()
        .and_then(|c| c.mavlink.endpoints)
        .and_then(|eps| {
            eps.into_iter()
                .find(|e| e.enabled && e.kind == "websocket")
                .and_then(|e| e.port)
        })
        .unwrap_or(DEFAULT_MAVLINK_WS_PORT)
}

/// Load the ruleset and record the verdict in the sidecar. Blocking; run at
/// daemon startup and in every plugin unit's privileged pre-start.
pub fn install(ruleset: &str, sidecar: &Path) -> GuardState {
    let state = load(ruleset);
    if let Err(e) = write_sidecar(sidecar, &state) {
        tracing::error!(path = %sidecar.display(), error = %e, "plugin_loopback_guard_sidecar_failed");
    }
    if state.active {
        tracing::info!(table = TABLE, "plugin_loopback_guard_active");
    } else {
        tracing::error!(reason = %state.reason, "plugin_loopback_guard_unavailable");
    }
    state
}

/// Off Linux there is no nftables and no plugin sandbox for the guard to
/// complement, so the verdict is inactive: a `network.outbound` grant stays
/// refused there rather than reading as guarded.
#[cfg(not(target_os = "linux"))]
fn load(_ruleset: &str) -> GuardState {
    GuardState::unavailable(format!(
        "the loopback guard needs nftables, which {} does not have",
        std::env::consts::OS
    ))
}

#[cfg(target_os = "linux")]
fn load(ruleset: &str) -> GuardState {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = match Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return GuardState::unavailable(format!("nft: {e}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(ruleset.as_bytes()) {
            return GuardState::unavailable(format!("nft stdin: {e}"));
        }
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => GuardState {
            active: true,
            reason: String::new(),
        },
        Ok(out) => GuardState::unavailable(format!(
            "nft rejected the ruleset: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => GuardState::unavailable(format!("nft: {e}")),
    }
}

fn write_sidecar(path: &Path, state: &GuardState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_vec(state).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
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
        assert!(rules
            .starts_with("table inet ados_plugin_guard\ndelete table inet ados_plugin_guard\n"));
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
        assert!(
            lines.contains(&"meta skgid \"ados-plugins\" fib daddr type local udp dport 53 accept")
        );
    }

    #[test]
    fn the_configured_websocket_port_joins_the_agent_set() {
        let yaml = "mavlink:\n  endpoints:\n    - type: websocket\n      host: 0.0.0.0\n      port: 9100\n      enabled: true\n";
        assert_eq!(configured_ws_port(yaml), 9100);
        assert_eq!(
            configured_ws_port("agent:\n  name: x\n"),
            DEFAULT_MAVLINK_WS_PORT
        );
        let disabled = "mavlink:\n  endpoints:\n    - type: websocket\n      port: 9100\n      enabled: false\n";
        assert_eq!(configured_ws_port(disabled), DEFAULT_MAVLINK_WS_PORT);
        assert!(agent_tcp_ports(9100).contains(&9100));
        assert_eq!(
            agent_tcp_ports(8080).iter().filter(|p| **p == 8080).count(),
            1
        );
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
}
