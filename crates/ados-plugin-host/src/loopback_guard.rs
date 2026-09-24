//! The loopback guard: keeps a plugin granted `network.outbound` off the agent's
//! own loopback listeners.
//!
//! The agent's HTTP front, the MAVLink WebSocket and raw proxies, the media
//! server and the local query endpoints treat a loopback peer as on-box. A
//! plugin that may use the network must not inherit that trust. systemd's
//! `IPAddressDeny=localhost` cannot express it: the address filter applies to
//! the unit's ingress as well, so it would also cut the host's `ready_check`
//! probe and a plugin's own loopback listeners. An nftables output rule matched
//! on the plugin user closes exactly the agent-owned ports instead.
//!
//! The match is the socket owner's uid, not the plugin slice's cgroup. Every
//! plugin process (main unit, declared service, readiness probe) runs as the
//! dedicated `ados` user, which nothing else on the box uses, and a uid is
//! stable. A cgroup match binds the rule to the slice directory's inode at load
//! time; systemd creates and prunes that directory as plugins come and go, and a
//! recreated directory would leave the rule silently matching nothing.
//!
//! The daemon loads the table at startup, before any plugin runs, and records
//! the verdict in a run-dir sidecar every grant path and renderer reads. When
//! the rule cannot load, `network.outbound` is refused at grant time and a unit
//! rendered for an earlier grant keeps `IPAddressDeny=any`: without the guard,
//! the capability is not safe to hold.

use std::path::Path;

use ados_protocol::plugin_loopback_guard::GuardState;
use serde::Deserialize;

/// The nftables table the guard owns.
pub const TABLE: &str = "ados_plugin_guard";

/// The user every plugin process runs as.
pub const PLUGIN_USER: &str = "ados";

/// Agent-owned TCP listeners a loopback peer can reach: the control front and
/// its alternate port, the raw MAVLink TCP proxy, the logging query endpoint,
/// the compute job API, and the media server's RTSP, HLS, WebRTC, playback,
/// API, metrics and profiling listeners. The MAVLink WebSocket port is
/// operator-configurable and joins the set at load time.
pub const AGENT_TCP_PORTS: &[u16] = &[
    8080, 8082, 5760, 8090, 8092, 8554, 8888, 8889, 9996, 9997, 9998, 9999,
];

/// Agent-owned UDP listeners on loopback: the raw MAVLink UDP proxy.
pub const AGENT_UDP_PORTS: &[u16] = &[14550];

/// The default MAVLink WebSocket proxy port.
pub const DEFAULT_MAVLINK_WS_PORT: u16 = 8765;

/// The full TCP port set: the fixed listeners plus the configured MAVLink
/// WebSocket port, sorted and de-duplicated so the rendered ruleset is stable.
pub fn tcp_ports(ws_port: u16) -> Vec<u16> {
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
pub fn render_ruleset(tcp: &[u16], udp: &[u16]) -> String {
    let tcp = port_set(tcp);
    let udp = port_set(udp);
    let m = format!("meta skuid \"{PLUGIN_USER}\"");
    format!(
        "table inet {TABLE}\n\
         delete table inet {TABLE}\n\
         table inet {TABLE} {{\n\
         \tchain output {{\n\
         \t\ttype filter hook output priority 0; policy accept;\n\
         \t\t{m} ip daddr 127.0.0.0/8 tcp dport {{ {tcp} }} drop\n\
         \t\t{m} ip6 daddr ::1 tcp dport {{ {tcp} }} drop\n\
         \t\t{m} ip daddr 127.0.0.0/8 udp dport {{ {udp} }} drop\n\
         \t\t{m} ip6 daddr ::1 udp dport {{ {udp} }} drop\n\
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

/// Load the ruleset and record the verdict in the sidecar. Blocking; run once
/// at daemon startup before any plugin unit starts.
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
    let tmp = path.with_extension("json.tmp");
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

    #[test]
    fn the_ruleset_drops_every_agent_port_for_the_plugin_user_only() {
        let rules = render_ruleset(&tcp_ports(9000), AGENT_UDP_PORTS);
        assert!(rules
            .starts_with("table inet ados_plugin_guard\ndelete table inet ados_plugin_guard\n"));
        assert!(rules.contains("type filter hook output priority 0; policy accept;"));
        let tcp_v4 = rules
            .lines()
            .find(|l| l.contains("ip daddr 127.0.0.0/8 tcp dport"))
            .unwrap();
        assert!(tcp_v4.contains("meta skuid \"ados\""));
        for port in [8080, 5760, 8889, 9997, 9000] {
            assert!(
                tcp_v4.contains(&port.to_string()),
                "{port} missing: {tcp_v4}"
            );
        }
        assert!(tcp_v4.ends_with(" drop"));
        assert!(rules.contains("ip6 daddr ::1 tcp dport"));
        assert!(rules.contains("ip daddr 127.0.0.0/8 udp dport { 14550 } drop"));
        // Nothing but the plugin user is matched: every rule line carries it.
        for line in rules.lines().filter(|l| l.contains(" drop")) {
            assert!(line.contains("meta skuid \"ados\""), "{line}");
        }
    }

    #[test]
    fn the_configured_websocket_port_joins_the_set() {
        let yaml = "mavlink:\n  endpoints:\n    - type: websocket\n      host: 0.0.0.0\n      port: 9100\n      enabled: true\n";
        assert_eq!(configured_ws_port(yaml), 9100);
        assert_eq!(
            configured_ws_port("agent:\n  name: x\n"),
            DEFAULT_MAVLINK_WS_PORT
        );
        let disabled = "mavlink:\n  endpoints:\n    - type: websocket\n      port: 9100\n      enabled: false\n";
        assert_eq!(configured_ws_port(disabled), DEFAULT_MAVLINK_WS_PORT);
        assert!(tcp_ports(9100).contains(&9100));
        assert_eq!(tcp_ports(8080).iter().filter(|p| **p == 8080).count(), 1);
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
