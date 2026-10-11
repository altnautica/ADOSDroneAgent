//! The direct-GCS TCP, UDP and WebSocket proxies.

use std::sync::Arc;

use ados_protocol::shutdown::Shutdown;

use super::super::startup::{
    tcp_proxy_port, udp_proxy_ports, ws_proxy_bind, ws_proxy_port, ProxyPosture,
};
use super::super::Tasks;
use crate::config::MavlinkConfig;
use crate::connection::FcConnection;
use crate::proxies::{
    proxy_bind_addr, run_tcp_proxy, run_udp_proxy, run_ws_proxy, ProxyAuth, WsProxyAuth,
};

/// Spawn one task per proxy edge.
pub(in crate::daemon) fn spawn(
    cfg: &MavlinkConfig,
    fc: &Arc<FcConnection>,
    posture: &ProxyPosture,
    cancel: &Shutdown,
    tasks: &mut Tasks,
) {
    // Direct-GCS proxies. Bind ports default to the standard values and are
    // overridable via env so a second instance (the parity harness) can run
    // alongside the first without a port clash.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        let port = tcp_proxy_port();
        let bind = proxy_bind_addr();
        // Same posture object the WebSocket uses, on its own flags: these
        // ports carry no credential channel, so a paired node serves them
        // off-box only when the operator opened them to the LAN. See
        // `config::default_raw_proxy_lan_access`.
        let auth = ProxyAuth::from_config(cfg.raw_proxy_enforce_auth)
            .with_raw_lan_access(cfg.raw_proxy_lan_access);
        let listening = posture.tcp.clone();
        tasks.push(tokio::spawn(async move {
            run_tcp_proxy(fc, &bind, port, auth, listening, cancel).await
        }));
    }
    for (port, listening) in udp_proxy_ports()
        .into_iter()
        .zip(posture.udp.iter().cloned())
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        let auth = ProxyAuth::from_config(cfg.raw_proxy_enforce_auth)
            .with_raw_lan_access(cfg.raw_proxy_lan_access);
        let bind = proxy_bind_addr();
        tasks.push(tokio::spawn(async move {
            run_udp_proxy(fc, &bind, port, auth, listening, cancel).await
        }));
    }
    if let (Some(ws_port), Some(listening)) = (ws_proxy_port(cfg), posture.ws.clone()) {
        let fc = fc.clone();
        let cancel = cancel.clone();
        // The direct WebSocket proxy carries raw MAVLink to/from the FC, so a
        // paired agent gates an off-box connection on the stored pairing key or
        // a valid ticket, and an unpaired one serves only the local operator
        // and the first-boot lifelines. Enforcement is config-driven and
        // defaults ON: the endpoint has two credential channels every
        // first-party client already presents.
        let auth = WsProxyAuth::from_config(cfg.ws_proxy_enforce_auth);
        let bind = ws_proxy_bind(cfg);
        tasks.push(tokio::spawn(async move {
            run_ws_proxy(fc, &bind, ws_port, auth, listening, cancel).await
        }));
    }
}
