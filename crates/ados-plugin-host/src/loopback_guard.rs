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
//! order, and a load failure fails the unit closed. The daemon records its
//! verdict in a run-dir sidecar every grant path and renderer reads; the
//! pre-start only reloads (see [`reload`]). When the rule cannot load,
//! `network.outbound` is refused at grant time and a unit rendered for an
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
        #[serde(rename = "type", default = "kind_default")]
        kind: String,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default = "enabled_default")]
        enabled: bool,
    }
    // The schema default of `EndpointConfig.type`: an entry without a type is a
    // WebSocket endpoint, as the router reads it.
    fn kind_default() -> String {
        "websocket".to_string()
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
/// daemon startup, whose view of the run dir is the host's.
pub fn install(ruleset: &str, sidecar: &Path) -> GuardState {
    install_with(ruleset, sidecar, load)
}

/// Load the ruleset without recording the verdict. Blocking; run in every
/// plugin unit's privileged pre-start.
///
/// The `+` prefix lifts the unit's privilege restrictions, but systemd still
/// builds the pre-start's mount namespace from the unit's
/// `TemporaryFileSystem=` and bind mounts, so the run dir it sees is the
/// unit's private read-only tmpfs. A sidecar write there fails, and would
/// reach no reader if it succeeded. The daemon's record stands; a failed
/// reload fails the unit instead.
pub fn reload(ruleset: &str) -> GuardState {
    report(load(ruleset))
}

fn install_with(
    ruleset: &str,
    sidecar: &Path,
    load: impl FnOnce(&str) -> GuardState,
) -> GuardState {
    let state = load(ruleset);
    if let Err(e) = write_sidecar(sidecar, &state) {
        tracing::error!(path = %sidecar.display(), error = %e, "plugin_loopback_guard_sidecar_failed");
    }
    report(state)
}

fn report(state: GuardState) -> GuardState {
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

/// Atomic replace through a uniquely named temp file, so concurrent writers
/// never share one; the parent dir is created. World-readable: the status and
/// grant paths read it from other processes.
fn write_sidecar(path: &Path, state: &GuardState) -> std::io::Result<()> {
    ados_protocol::sidecar::write_json_atomic(path, state, 0o644)
}

#[cfg(test)]
mod tests;
