//! Direct GCS transport proxies.
//!
//! Mirror the Python TCP (`tcp_proxy.py`, port 5760), UDP (`udp_proxy.py`,
//! ports 14550/14551), and WebSocket (port 8765) proxies: a GCS connects
//! directly and exchanges raw MAVLink frames with the FC, bypassing the cloud
//! relay. Each proxy relays the FC frame stream (via [`FcConnection::subscribe`])
//! out to its clients and forwards client bytes back with
//! [`FcConnection::send_client_bytes`] — to the local FC when one is attached,
//! or onto the aux uplink toward a relayed drone when this node is a ground
//! station relaying one instead. The WebSocket proxy ([`run_ws_proxy`]) carries
//! MAVLink in binary WebSocket frames, the way a browser GCS connects.
//!
//! Every edge retries a failed bind on a fixed interval with no cap and
//! reports whether it holds its socket ([`ListenFlag`]); a failed `accept`
//! backs off instead of spinning; the TCP and WebSocket edges forward whole
//! MAVLink frames only and re-check each live session's admission
//! periodically.

mod edge;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use ados_protocol::pairing_posture::{
    classify_caller, data_plane_access, load_pairing, Access, CallerClass, Pairing,
};
use ados_protocol::shutdown::Shutdown;
use ados_protocol::ws_ticket::{now_unix, WsTicketIssuer, SCOPE_MAVLINK_WS};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::connection::{ClientFramer, ClientOrigin, FcConnection};
pub use edge::ListenFlag;
use edge::{bind_until, forward_client_bytes, AcceptErrors, ListeningGuard, UdpPeers, UdpVerdicts};

/// The data-path auth header a paired GCS presents to the direct WebSocket
/// proxy, matching the HTTP control surface's `X-ADOS-Key`.
const WS_KEY_HEADER: &str = "x-ados-key";

/// The WebSocket subprotocol marker carrying an auth ticket, for a browser GCS
/// that cannot set `X-ADOS-Key` on the handshake: it dials
/// `new WebSocket(url, ["ados-ws-ticket", <token>])`. The agent echoes the
/// marker back on the accepted handshake per RFC 6455. Matches the GCS
/// `WS_TICKET_PROTOCOL` and the Python `ws_auth.WS_TICKET_PROTOCOL`.
const WS_TICKET_SUBPROTOCOL: &str = "ados-ws-ticket";

/// How long a loaded pairing state is trusted before `pairing.json` is re-read.
/// Short enough that a pair/unpair is honoured for a new connection within a few
/// seconds, long enough that a connection burst does not stat the file every
/// time. Mirrors the HTTP control surface's pairing TTL.
const PAIRING_TTL: Duration = Duration::from_secs(2);

/// Bind address for the direct-GCS proxies when nothing overrides it.
///
/// Wide on purpose: a fresh unit is reached over its AP hotspot and USB gadget,
/// neither of which is loopback, and an operator who opts the raw edges into
/// LAN access (`mavlink.raw_proxy_lan_access`) needs them on the LAN. Who is
/// actually served is decided per connection by [`WsProxyAuth`], which follows
/// the pairing state with no restart; on a paired node without that opt-in the
/// raw edges serve on-box callers only.
pub const DEFAULT_PROXY_BIND_ADDR: &str = "0.0.0.0";

/// The operator's `ADOS_MAVLINK_BIND_ADDR` override, trimmed; `None` when it is
/// unset or blank (a blank override is a mistake, not a request to bind
/// nowhere).
fn bind_override() -> Option<String> {
    std::env::var("ADOS_MAVLINK_BIND_ADDR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The address the raw direct-GCS proxies (TCP, UDP) bind, from
/// `ADOS_MAVLINK_BIND_ADDR`.
///
/// Settable because the advertisement above is not right for every deployment.
/// A unit that never needs a desktop ground station on its LAN can bind
/// loopback and stop carrying an unauthenticated path to its flight controller
/// at all — a stronger remedy than inspecting callers on a socket that stays
/// open, because there is then nothing left to inspect.
pub fn proxy_bind_addr() -> String {
    bind_override().unwrap_or_else(|| DEFAULT_PROXY_BIND_ADDR.to_string())
}

/// The address the WebSocket proxy binds: the `ADOS_MAVLINK_BIND_ADDR`
/// override when set (it narrows every direct-GCS proxy at once), else the
/// configured WebSocket endpoint's `host`, else the default.
pub fn ws_bind_addr(configured_host: &str) -> String {
    bind_override()
        .or_else(|| Some(configured_host.trim().to_string()).filter(|h| !h.is_empty()))
        .unwrap_or_else(|| DEFAULT_PROXY_BIND_ADDR.to_string())
}

mod auth;
pub use auth::*;
mod tcp;
pub use tcp::*;
mod udp;
pub use udp::*;
mod ws;
pub use ws::*;

#[cfg(test)]
mod tests;
