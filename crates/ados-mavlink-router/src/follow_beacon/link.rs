//! Who wrote a client frame, as the router saw it: the origin class and the
//! router's own id for the connection. Both are stamped by the router, never
//! taken from the sender.

use std::hash::{BuildHasher, RandomState};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use ados_protocol::ipc::IpcPeer;

use crate::connection::ClientOrigin;

/// How a writer reached the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A network ground link that passed the pairing posture check.
    GroundLink,
    /// A local service on the MAVLink socket.
    OnBox,
    /// The cloud relay or the radio's aux uplink.
    Relay,
    /// Plugin traffic: the plugin host's link (it declares so), a plugin-plane
    /// peer, or a declared autonomous injector. Never accepted.
    Plugin,
    /// A network peer that did not pass the posture check. Never accepted.
    Anonymous,
}

impl Origin {
    /// The origin of a network proxy client from its admission verdict.
    pub fn of_proxy(origin: ClientOrigin) -> Self {
        match origin {
            ClientOrigin::Trusted => Origin::GroundLink,
            ClientOrigin::Relayed => Origin::Relay,
            ClientOrigin::Unauthenticated => Origin::Anonymous,
        }
    }

    /// The origin of a MAVLink-socket writer from its kernel credentials and
    /// its declarations.
    pub fn of_ipc(peer: &IpcPeer) -> Self {
        if peer.plugin_relay || peer.injector.is_some() {
            Origin::Plugin
        } else if peer.off_box_source {
            Origin::Relay
        } else {
            Origin::OnBox
        }
    }
}

/// The router's id for one client connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkId {
    /// A MAVLink-socket connection, by its serial.
    Ipc(u64),
    /// A TCP proxy connection.
    Tcp(u64),
    /// A WebSocket proxy connection.
    Ws(u64),
    /// A UDP proxy peer, by address.
    Udp(SocketAddr),
    /// The radio's aux uplink.
    Aux,
}

impl LinkId {
    /// A fresh serial for a TCP or WebSocket connection.
    pub fn next_serial() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// The published label. A UDP peer's address is not published: it is
    /// named by a per-process keyed hash of it.
    pub fn label(self) -> String {
        match self {
            LinkId::Ipc(n) => format!("ipc-{n}"),
            LinkId::Tcp(n) => format!("tcp-{n}"),
            LinkId::Ws(n) => format!("ws-{n}"),
            LinkId::Udp(addr) => {
                static KEYS: OnceLock<RandomState> = OnceLock::new();
                let h = KEYS.get_or_init(RandomState::new).hash_one(addr);
                format!("udp-{:08x}", h as u32)
            }
            LinkId::Aux => "aux".to_string(),
        }
    }
}

/// One writer: its origin and its connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeaconLink {
    pub origin: Origin,
    pub id: LinkId,
}
