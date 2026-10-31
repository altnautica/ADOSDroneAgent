//! Unix-socket IPC transport for the MAVLink and state sockets.
//!
//! Semantics must agree with `src/ados/core/ipc.py`:
//!
//! - The owning service binds a Unix socket (perms `0o660`, group
//!   `ados-operator`, stale socket removed first) and broadcasts byte buffers to
//!   every connected client. The trusted local plane is root plus that group and
//!   nothing wider: two of these sockets forward inbound bytes straight to the
//!   flight controller. Plugins run as the `ados` user, which is never in the
//!   operator group, and every accept re-checks the peer's kernel credentials
//!   (see [`OperatorListener`]).
//! - Each client has a bounded outbound queue. A client whose queue fills is
//!   dropped rather than allowed to grow unbounded (slow-client policy).
//! - The state socket additionally replays the last buffer to a client the
//!   moment it connects, so a late subscriber does not wait for the next
//!   publish.
//! - The MAVLink socket is bidirectional: a frame written by a client is a
//!   command toward the flight controller, surfaced here on an inbound channel.
//!
//! Framing is the caller's concern. [`broadcast`](IpcBroadcast::broadcast)
//! sends the exact bytes given (the caller frames via [`crate::frame`] or
//! [`crate::state`]). The inbound reader, used only by the MAVLink socket,
//! decodes 4-byte big-endian length-prefixed frames.
//!
//! ## Who wrote this
//!
//! An inbound payload arrives as an [`InboundCommand`], which carries the
//! writing connection's [`IpcPeer`] alongside the bytes. The owning service
//! used to receive a bare buffer and had no way to tell one writer from
//! another, so it recorded a single hardcoded provenance for every one of
//! them. That reading was wrong for the writer that matters most: a process
//! that accepts bytes from off the node and forwards them here is not the same
//! as a process that produced them itself, and a service that cannot see the
//! difference reports remote traffic as locally originated.

use std::future::Future;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::frame;

mod broadcast;
mod client_conn;
mod listener;
mod rpc;
pub(crate) use broadcast::*;
pub use listener::*;
pub use rpc::*;

/// The control payload a writer sends to declare that the bytes it is about to
/// forward did not originate on this node.
///
/// Sent as an ordinary length-prefixed frame, so no framing changes and no
/// existing writer is affected. It cannot collide with real traffic on either
/// socket that carries inbound bytes: a MAVLink frame starts `0xFD` (v2) or
/// `0xFE` (v1) and an MSP one starts `$`, while this starts with a NUL.
///
/// The declaration is one-way and downward. A writer can only ever give up the
/// benefit of the doubt with it, never claim one, so a writer that does not
/// send it is treated exactly as it was before and a writer that forges it only
/// declassifies itself. It is consumed by the reader and never forwarded on.
pub const IPC_DECLARE_OFF_BOX_SOURCE: &[u8] = b"\x00ados-ipc/source=off-box";

mod injector;
pub use injector::{
    encode_injector_declaration, parse_injector_declaration, InjectorClaim,
    IPC_DECLARE_INJECTOR_PREFIX,
};

/// Who wrote an inbound payload, as the accepting socket can tell.
///
/// The credentials come from the kernel (`SO_PEERCRED` and its equivalents),
/// so they cannot be forged by the writer. `off_box_source` and `injector` are
/// the writer's own declarations (see [`IPC_DECLARE_OFF_BOX_SOURCE`] /
/// [`IPC_DECLARE_INJECTOR_PREFIX`]) and are sticky for the life of the
/// connection, except that a plugin-plane writer ([`is_plugin_peer`]) starts
/// out as an injector whatever it declares. Not `Copy`: the injector claim
/// carries owned strings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IpcPeer {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub pid: Option<i32>,
    /// The writing process has declared that it is forwarding bytes that
    /// reached this node from somewhere else.
    pub off_box_source: bool,
    /// The writer is an autonomous injector, so its commands are subject to
    /// the PIC arbiter: it declared itself one, or its kernel credentials put
    /// it on the plugin plane. `None` = an operator/human writer, never gated.
    pub injector: Option<InjectorClaim>,
    /// This connection's serial, unique within the accepting process, so a
    /// consumer can keep per-connection state (a partial frame) apart.
    pub conn: u64,
}

/// One inbound payload plus the identity of the connection that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundCommand {
    pub payload: Vec<u8>,
    pub peer: IpcPeer,
}

/// The shared service user plugin processes ran as before each plugin got its
/// own user. Still treated as plugin plane wherever it exists.
pub const LEGACY_PLUGIN_USER: &str = "ados";

/// Whether a connecting peer is on the plugin plane: the shared plugin user,
/// or any process carrying [`PLUGIN_GROUP`] as its primary or a supplementary
/// group. Such a writer is autonomous by construction, so it is gated as an
/// injector whether or not it declares itself one; a self-declaration only
/// binds the cooperating ones.
///
/// Pure over the kernel-reported credentials so the decision is testable
/// without a second uid.
pub fn is_plugin_peer(
    peer_uid: u32,
    peer_gids: &[u32],
    plugin_user_uid: Option<u32>,
    plugin_gid: Option<u32>,
) -> bool {
    plugin_user_uid == Some(peer_uid) || plugin_gid.is_some_and(|gid| peer_gids.contains(&gid))
}

/// The injector claim a plugin-plane writer carries from the moment it
/// connects. It has no ticket, so on a paired node it can never verify, and
/// the PIC arbiter lets it through only on an affirmative "nobody holds
/// control" report.
fn plugin_plane_claim(uid: u32) -> InjectorClaim {
    InjectorClaim {
        client_id: format!("uid:{uid}"),
        ticket: None,
    }
}

/// One connected client: its outbound queue plus the writer and reader tasks,
/// so both can be aborted when the client is pruned or the server is dropped.
///
/// The queue carries [`Bytes`], not `Vec<u8>`: one broadcast buffer is handed to
/// every attached client, so a `Vec` made that a full copy per client per frame.
struct ClientHandle {
    tx: mpsc::Sender<Bytes>,
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
}

/// A Unix-socket broadcast server. Drops slow clients; optionally replays the
/// last buffer on connect; optionally decodes inbound length-prefixed frames.
pub struct IpcBroadcast {
    path: PathBuf,
    queue_depth: usize,
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    last: Arc<Mutex<Option<Bytes>>>,
    keep_last: bool,
    accept_task: JoinHandle<()>,
    /// Monotonic count of clients evicted for falling behind — a full outbound
    /// queue, and only that. A consumer that simply disconnected is pruned
    /// without advancing this counter, so the number means "a consumer could
    /// not keep up" rather than "a client went away". A slow consumer is
    /// otherwise pruned silently; surfacing this counter lets the owning
    /// service report the eviction the same way the radio TX-liveness watchdog
    /// surfaces a stalled link — a silent drop is itself the failure mode to
    /// detect, and a counter that also ticks on normal disconnects would drown
    /// that signal.
    dropped_clients: Arc<AtomicU64>,
}

/// The group that owns every agent command-plane socket: the control socket,
/// the `*-cmd.sock` command sockets, the MAVLink/MSP/state lanes, and the rest
/// of the sockets bound through [`bind_command_socket`]. The installer creates
/// it and adds the human operator; the plugin user is never a member, whatever
/// capabilities it holds (a sensor-bus reader joins [`SENSORS_GROUP`]).
pub const OPERATOR_GROUP: &str = "ados-operator";

/// The group that may write `/run/ados/capture` (the capture-active marker
/// directory, `2775 root:ados-capture`) and the capture root, which the
/// supervisor's janitor and the installer keep at `3775 root:ados-capture`.
/// A capture plugin joins it only while it holds `capture.storage.write`; it
/// guards nothing else.
pub const CAPTURE_GROUP: &str = "ados-capture";

/// The reader group of the sensor bus (`/run/ados/sensing/sensors.sock`), bound with
/// [`bind_reader_socket`]. A member may connect and subscribe; what it
/// publishes is dropped. A plugin joins it only while it holds
/// `sensor.stream.read`, which is how a sandboxed plugin reads the bus
/// without joining [`OPERATOR_GROUP`]. The installer also adds the human
/// operator, whose operator membership still decides what it may publish.
pub const SENSORS_GROUP: &str = "ados-sensors";

/// The group every plugin unit runs in as its primary group. Each plugin has
/// its own system user; this shared group owns only the per-plugin sockets
/// ([`bind_plugin_socket`]), which gate every request on the plugin's own
/// capability token, and it is what the plugin loopback guard and the router
/// match a plugin process by.
pub const PLUGIN_GROUP: &str = "ados-plugins";

/// Whether a connecting peer may use a command-plane socket.
///
/// Root always may. So may a peer running as the same uid as the serving
/// process: every agent service runs as root, so on a node this adds nothing,
/// and on a dev host it lets the owning user's own tools in. Anyone else must
/// carry the operator group, primary or supplementary. With the group absent
/// from the host (`operator_gid` is `None`) nobody else is admitted.
///
/// Pure over the kernel-reported credentials so the decision is testable
/// without a second uid.
pub fn operator_peer_allowed(
    peer_uid: u32,
    peer_gids: &[u32],
    server_uid: u32,
    operator_gid: Option<u32>,
) -> bool {
    peer_uid == 0
        || peer_uid == server_uid
        || operator_gid.is_some_and(|gid| peer_gids.contains(&gid))
}

/// A bound command-plane socket. [`accept`](Self::accept) yields only peers
/// [`operator_peer_allowed`] admits (plus, on a socket bound with
/// [`bind_reader_socket`], members of its reader group); any other peer is
/// closed before a byte of it is read.
///
/// The socket file is already `0o660 root:ados-operator` (or `root:<reader
/// group>`), so the kernel refuses `connect()` to anyone outside the group. The
/// per-accept check is the second line: it holds when the chgrp could not be
/// applied, when the mode is widened by hand, or when a descriptor to the
/// socket is passed to a process that could not have opened it.
pub struct OperatorListener {
    inner: UnixListener,
    path: PathBuf,
    server_uid: u32,
    operator_gid: Option<u32>,
    /// The reader group's gid on a [`bind_reader_socket`] socket.
    reader_gid: Option<u32>,
}

mod peer_class;
pub use peer_class::{bind_reader_socket, classify_peer, PeerClass};

#[cfg(test)]
mod tests;
