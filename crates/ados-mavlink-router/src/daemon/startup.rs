//! Process setup: logging, the bind ports and the proxy posture.

use ados_protocol::aux_ports::AuxPorts;
use serde_json::{json, Map, Value};
use std::time::Duration;

use crate::config::MavlinkConfig;
use crate::proxies::{proxy_bind_addr, ws_bind_addr, ListenFlag};

/// Install the fmt sink plus the logd layer.
pub(super) fn init_tracing() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;

    // fmt as the primary sink (this binary has no journald layer) plus the logd
    // layer that ships records to the logging daemon's ingest socket; the logd
    // layer is best-effort and never blocks the service.
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-mavlink-router"))
        .try_init();
}

pub(super) const MAVLINK_QUEUE_DEPTH: usize = 256;
pub(super) const STATE_QUEUE_DEPTH: usize = 32;
const TCP_PROXY_PORT: u16 = 5760;
const UDP_PROXY_PORTS: &[u16] = &[14550, 14551];
/// The auxiliary lane's loopback port pair, resolved from operator config.
///
/// These used to be matching literals here, in the control surface and in the
/// radio service, with comments pointing at each other. That is correct at
/// exactly one value: the ports are operator-settable, and changing one moved
/// the radio while the other two kept writing to the old number, so the uplink
/// carried nothing and nothing reported an error. Resolving from the shared
/// reader moves all three together.
pub(super) fn aux_ports() -> AuxPorts {
    AuxPorts::load()
}

/// Demo mode: drive synthetic telemetry instead of opening a serial FC. Enabled
/// by the `--demo` argument or `ADOS_MAVLINK_DEMO=1`. Off by default, so the
/// production unit (no argument, no env) keeps the serial path.
pub(super) fn demo_enabled() -> bool {
    std::env::args().any(|a| a == "--demo")
        || std::env::var("ADOS_MAVLINK_DEMO").ok().as_deref() == Some("1")
}

/// TCP proxy bind port. Overridable via `ADOS_MAVLINK_TCP_PORT` (the parity
/// harness uses this to run a second instance without a port clash); defaults to
/// the standard port.
pub(super) fn tcp_proxy_port() -> u16 {
    std::env::var("ADOS_MAVLINK_TCP_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(TCP_PROXY_PORT)
}

/// UDP proxy bind ports (comma-separated). Overridable via
/// `ADOS_MAVLINK_UDP_PORTS`; defaults to the standard ports. An empty or
/// unparseable override falls back to the defaults.
pub(super) fn udp_proxy_ports() -> Vec<u16> {
    let parsed: Vec<u16> = std::env::var("ADOS_MAVLINK_UDP_PORTS")
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|p| p.trim().parse::<u16>().ok())
                .collect()
        })
        .unwrap_or_default();
    if parsed.is_empty() {
        UDP_PROXY_PORTS.to_vec()
    } else {
        parsed
    }
}

/// WebSocket proxy bind port. `ADOS_MAVLINK_WS_PORT` overrides the configured
/// endpoint port when set (used by the parity harness); otherwise the first
/// enabled WebSocket endpoint from the config selects it.
pub(super) fn ws_proxy_port(cfg: &MavlinkConfig) -> Option<u16> {
    if let Ok(v) = std::env::var("ADOS_MAVLINK_WS_PORT") {
        return v.trim().parse().ok();
    }
    cfg.websocket_port()
}

/// The WebSocket proxy's bind address: the configured endpoint host, narrowed
/// by the operator's bind override.
pub(super) fn ws_proxy_bind(cfg: &MavlinkConfig) -> String {
    ws_bind_addr(cfg.websocket_endpoint().map_or("", |e| e.host.as_str()))
}

/// What the direct-GCS proxies are bound to and who they serve, carried on the
/// state snapshot as `mavlink_proxy_posture` so an operator surface can say
/// whether a desktop ground station can reach this node's raw MAVLink ports.
/// The configured part is fixed for the process; the `*_listening` flags are
/// read live from the proxy tasks, so a bind that failed (and is being
/// retried) reads `false` rather than as a configured address.
#[derive(Clone, Default)]
pub(super) struct ProxyPosture {
    pub(super) configured: Map<String, Value>,
    pub(super) tcp: ListenFlag,
    pub(super) udp: Vec<ListenFlag>,
    /// `None` when no WebSocket endpoint is enabled.
    pub(super) ws: Option<ListenFlag>,
}

impl ProxyPosture {
    /// The posture as published: the configured fields plus `tcp_listening`,
    /// `udp_listening` (one bool per UDP port, in port order) and
    /// `ws_listening` (null with no WebSocket endpoint).
    pub(super) fn to_value(&self) -> Value {
        let mut out = self.configured.clone();
        out.insert("tcp_listening".into(), json!(self.tcp.get()));
        out.insert(
            "udp_listening".into(),
            Value::Array(self.udp.iter().map(|f| json!(f.get())).collect()),
        );
        out.insert(
            "ws_listening".into(),
            self.ws.as_ref().map_or(Value::Null, |f| json!(f.get())),
        );
        Value::Object(out)
    }
}

/// The proxy posture for `cfg`, with one fresh listening flag per edge. `ws_bind`
/// is null when no WebSocket endpoint is enabled.
pub(super) fn proxy_posture(cfg: &MavlinkConfig) -> ProxyPosture {
    let ws_port = ws_proxy_port(cfg);
    let configured = json!({
        "ws_bind": ws_port.map(|_| ws_proxy_bind(cfg)),
        "ws_enforce_auth": cfg.ws_proxy_enforce_auth,
        "raw_bind": proxy_bind_addr(),
        "raw_enforce_auth": cfg.raw_proxy_enforce_auth,
        "raw_lan_access": cfg.raw_proxy_lan_access,
    });
    ProxyPosture {
        configured: configured.as_object().cloned().unwrap_or_default(),
        tcp: ListenFlag::new(),
        udp: udp_proxy_ports()
            .iter()
            .map(|_| ListenFlag::new())
            .collect(),
        ws: ws_port.map(|_| ListenFlag::new()),
    }
}
/// How often the attitude-cadence distribution is shipped to the store.
///
/// The window is already a summary by this point, so a faster cadence would add
/// writes without adding information, and this measurement must not disturb the
/// thing it is measuring.
pub(super) const CADENCE_REPORT_INTERVAL: Duration = Duration::from_secs(10);
