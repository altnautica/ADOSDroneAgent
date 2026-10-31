//! Command socket for the auxiliary application stream.
//!
//! A plugin that needs an isolated low-rate channel between nodes asks the
//! plugin host to open one; the host forwards the request to this socket, and the
//! running radio service brings up an additive transmit/receive pair on a
//! separate radio-port (it never touches the data or control planes). A matching
//! `close` (or the plugin disconnecting and the host closing on its behalf) tears
//! the pair down.
//!
//! SAFE-BY-DEFAULT: nothing here runs at boot. The aux pair exists ONLY between
//! an explicit `open` and the matching `close`. The radio service spawns this
//! socket per bring-up with the SAME process handle the watchdogs + operator
//! command socket hold, so an `open` reaches the live radio group.
//!
//! Wire protocol (mirrors the operator command socket): one newline-terminated
//! JSON request. `open` / `close` / `status` / `send` / `publish` reply with one
//! newline-terminated JSON response and close (one-shot). `subscribe` flips the
//! connection into a streaming subscriber: it replies `{"ok":true}` then pushes
//! every decoded application datagram as a newline-terminated JSON line until
//! the client disconnects or asks to close.
//!
//! ```text
//! {"op":"open"}
//!     -> {"ok":true,"active":true,"tx_port":5602,"rx_port":5603}
//! {"op":"close"}
//!     -> {"ok":true,"active":false}
//! {"op":"status"}
//!     -> {"ok":true,"active":false}
//! {"op":"send","frame":[170,2,1,8,0,5,104,101,108,108,111]}
//!     -> {"ok":true}
//! {"op":"publish","channel":8,"payload":[104,101,108,108,111]}
//!     -> {"ok":true,"delivered":1}
//! {"op":"subscribe"}
//!     -> {"ok":true}
//!     -> {"channel":8,"payload":[104,101,108,108,111]}
//!     -> ...
//! ```
//!
//! ## Why `publish` exists — the inbound half lives in another process
//!
//! `send` writes OUTBOUND: an aux-framed datagram to the local transmit
//! ingress, which `wfb_tx` radiates. `publish` is the INBOUND direction, and it
//! is a separate op because this service does not own the receive side.
//!
//! On a drone `send` accepts only `AppStream` (8) frames that decode. The
//! downlink also carries the vehicle's MAVLink, relay RPC responses, status,
//! identity and the config tunnel; the services that produce those write their
//! frames straight to the transmit ingress after an `open`, so `send` is only
//! ever a plugin's path and never needs any other channel.
//!
//! The aux-RX loopback port that `wfb_rx -p3` decodes onto is owned by
//! `ados-mavlink-router`'s aux-uplink consumer, which needs it for the MAVLink,
//! relay-RPC, link-feedback and config-tunnel channels on that same lane. This
//! service bound it too for a while; nothing set SO_REUSEADDR, so one of the two
//! always lost and a whole lane went dark with no surface reporting it. One
//! owner now: the consumer decodes, and the two APPLICATION channels
//! (`AppStream` 8 / `AppCommand` 9) come back here through `publish`, which
//! injects them into the same broadcast every `subscribe` connection reads.
//!
//! `delivered` is the number of attached subscribers the datagram reached. Zero
//! is NOT a failure — it is the normal state of a drone with no plugin
//! subscribed — so a caller counting drops must key on `ok:false`, not on a zero
//! delivered count, or a healthy rig reports total loss.
//!
//! A failed apply (a spawn failure on `open`) replies `{"ok":false,"error":"..."}`
//! and leaves the aux pair closed, so the host can surface the error. The socket
//! only mutates the aux pair it owns; it never round-trips the on-disk config.
//!
//! ## The same socket on a ground station
//!
//! A ground station serves this exact protocol from its receive-plane service,
//! so a plugin half written against it runs unchanged on either node. The
//! difference is only what backs it (see [`AuxCmdState::ground`]): the ground
//! uplink `wfb_tx` belongs to the receive chain and already carries MAVLink,
//! relay RPC, link feedback and the config tunnel, so there is no pair to spawn.
//! `open`/`close` gate this socket's application egress instead, `send` accepts
//! only the two application channels (a plugin must never be able to inject a
//! MAVLink frame onto that shared uplink), and inbound `AppStream` datagrams
//! arrive in-process from the service's own aux consumer rather than over
//! `publish`, which stays available with the same semantics.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ados_protocol::aux_egress::{AuxEgress, AuxEgressError};
use ados_protocol::aux_mux::{self, AuxChannel};
use ados_protocol::ipc::{bind_command_socket, read_newline_line, OperatorListener};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, Mutex};

use crate::config::WfbConfig;
use crate::process::RadioProcesses;

mod apply;
mod parse;

use apply::*;
use parse::*;

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared state the aux command handlers act on: what carries the lane, the
/// operator dead-switch, and the inbound application fan-out.
/// Constructed once per bring-up on a drone ([`Self::radio`]) or once per
/// service on a ground station ([`Self::ground`]) and shared with every accepted
/// connection.
#[derive(Clone)]
pub struct AuxCmdState {
    lane: AuxLane,
    /// The operator dead-switch (`aux_enable`). Off refuses every op that would
    /// move application traffic in either direction.
    aux_enable: bool,
    /// Fan-out for inbound application datagrams. `subscribe` connections stream
    /// from this; [`Self::publish`] (the `publish` op, or a ground station's
    /// in-process aux consumer) feeds it. Items are `(channel as u8, payload)`.
    rx_tx: broadcast::Sender<(u8, Vec<u8>)>,
}

/// What an outbound `send` goes through, and what `open` / `close` / `status`
/// act on.
#[derive(Clone)]
enum AuxLane {
    /// Drone: this service owns the additive aux transmit/receive pair. `proc` is
    /// the SAME handle the watchdogs + operator command socket hold, so an `open`
    /// reaches the live radio group; `cfg` is the boot config, the source of the
    /// effective aux ports / FEC / MCS an `open` applies.
    Radio {
        proc: Arc<Mutex<RadioProcesses>>,
        cfg: Arc<WfbConfig>,
    },
    /// Ground station: the uplink `wfb_tx` is owned by the receive chain and
    /// shared with every other plane on the lane, so `open` / `close` only gate
    /// this socket's application egress. `open` is shared by every connection.
    Uplink {
        egress: Arc<AuxEgress>,
        tx_port: u16,
        open: Arc<AtomicBool>,
    },
}

impl AuxCmdState {
    /// The drone side: `open` / `close` drive the additive aux pair in `proc`,
    /// `send` writes to its loopback transmit ingress, and the aux-uplink
    /// consumer in the MAVLink router feeds `rx_tx` over the `publish` op.
    pub fn radio(
        proc: Arc<Mutex<RadioProcesses>>,
        cfg: Arc<WfbConfig>,
        rx_tx: broadcast::Sender<(u8, Vec<u8>)>,
    ) -> Self {
        Self {
            aux_enable: cfg.aux_enable,
            lane: AuxLane::Radio { proc, cfg },
            rx_tx,
        }
    }

    /// The ground-station side: `send` goes out through an egress connected to
    /// the uplink transmit ingress `tx_port` (the receive chain's aux `wfb_tx`),
    /// and the caller feeds inbound application datagrams through
    /// [`Self::publish`]. Fails only when the loopback egress socket cannot be
    /// created.
    pub async fn ground(
        tx_port: u16,
        aux_enable: bool,
        rx_tx: broadcast::Sender<(u8, Vec<u8>)>,
    ) -> Result<Self, AuxEgressError> {
        let egress = AuxEgress::connected_to_udp(tx_port).await?;
        Ok(Self {
            lane: AuxLane::Uplink {
                egress: Arc::new(egress),
                tx_port,
                open: Arc::new(AtomicBool::new(false)),
            },
            aux_enable,
            rx_tx,
        })
    }

    /// Inject one inbound application payload into the subscriber fan-out and
    /// return the reply the `publish` op would send (see
    /// [`publish_app_datagram`]): refused when the lane is disabled or the
    /// channel is not an application channel, otherwise the subscriber count.
    pub fn publish(&self, channel: u8, payload: Vec<u8>) -> Value {
        publish_app_datagram(self.aux_enable, &self.rx_tx, channel, payload)
    }
}

/// Bind the aux command socket and serve connections until the listener errors.
/// Run as its own task from the service main loop. The shared helper owns the
/// create-dir / remove-stale / bind / chmod (0660, group `ados-operator`) hygiene
/// and admits only root and operator-group peers. Each connection serves one request:
/// `open`/`close`/`status`/`send` reply once and close; `subscribe` replies then
/// streams application datagrams until the client disconnects or asks to close.
///
/// A Unix-domain listener supports multiple concurrent connecting clients, so a
/// subscriber holding a connection open never blocks a one-shot caller.
pub async fn serve(state: AuxCmdState, sock_path: &Path) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "aux command socket listening");

    accept_loop(listener, state).await;
    Ok(())
}

/// Accept loop over the aux command listener. Backs off on a transient accept
/// error rather than dying (a command socket that dies while the service stays
/// up would need a manual restart), mirroring the shared one-shot helper. Each
/// connection runs on its own task; a streaming subscriber does not block the
/// other connections.
async fn accept_loop(listener: OperatorListener, state: AuxCmdState) {
    loop {
        let mut stream = match listener.accept().await {
            Ok((s, _addr)) => s,
            Err(e) => {
                tracing::warn!(error = %e, "aux command socket accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_connection(&mut stream, &state).await {
                tracing::debug!(error = %e, "aux command connection ended");
            }
        });
    }
}

/// Serve one connection: read one newline-terminated request, then either reply
/// once (`open`/`close`/`status`/`send`) or switch into the streaming subscriber
/// path (`subscribe`).
async fn serve_connection(stream: &mut UnixStream, state: &AuxCmdState) -> io::Result<()> {
    let request = match read_newline_line(stream, MAX_REQUEST_BYTES).await {
        Ok(Some(req)) => req,
        Ok(None) | Err(_) => return Ok(()),
    };
    match parse_command(&request) {
        Parsed::Cmd(cmd) => {
            let resp = apply(cmd, state).await;
            write_json_line(stream, &resp).await?;
        }
        Parsed::Reply(v) => write_json_line(stream, &v).await?,
        Parsed::Subscribe => serve_subscriber(stream, state).await?,
    }
    Ok(())
}

/// Write one newline-terminated JSON line and flush. Mirrors the shared one-shot
/// helper's framing (line + trailing newline, then flush).
async fn write_json_line<W: AsyncWriteExt + Unpin>(w: &mut W, v: &Value) -> io::Result<()> {
    let mut bytes =
        serde_json::to_vec(v).map_err(|e| io::Error::other(format!("E_ENCODE: {e}")))?;
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await
}

/// Hold a subscribe connection open, streaming each decoded application datagram
/// as a newline-terminated `{"channel":N,"payload":[...]}` line, until the client
/// disconnects or asks to close. The operator dead-switch is checked first so a
/// disabled deployment refuses the subscribe without holding a connection open.
async fn serve_subscriber(stream: &mut UnixStream, state: &AuxCmdState) -> io::Result<()> {
    if !state.aux_enable {
        return write_json_line(stream, &json!({"ok": false, "error": "E_AUX_DISABLED"})).await;
    }
    write_json_line(stream, &json!({"ok": true})).await?;
    let mut rx = state.rx_tx.subscribe();
    loop {
        tokio::select! {
            res = rx.recv() => {
                match res {
                    Ok((channel, payload)) => {
                        let line = json!({"channel": channel, "payload": payload});
                        if write_json_line(stream, &line).await.is_err() {
                            break; // client gone (write failed)
                        }
                    }
                    // Subscriber fell behind; the oldest frames were dropped. Keep
                    // going — an application lane is lossy-tolerant, and dropping
                    // is better than stalling the connection.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            req = read_newline_line(stream, MAX_REQUEST_BYTES) => {
                // The client sent a line (e.g. {"op":"close"}) or disconnected
                // (clean EOF / error). Either way the stream ends.
                let _ = req;
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
