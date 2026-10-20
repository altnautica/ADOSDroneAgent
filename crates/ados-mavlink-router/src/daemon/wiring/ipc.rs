//! The IPC socket pumps: FC frames out to the MAVLink socket, raw MSP bytes
//! out to the MSP socket, and client commands from both sockets into the FC.

use std::sync::Arc;

use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
use ados_protocol::ipc::{InboundCommand, IpcBroadcast};
use ados_protocol::mavlink_socket::encode_record;
use ados_protocol::shutdown::Shutdown;
use tokio::sync::mpsc;

use super::super::Tasks;
use crate::connection::{ClientOrigin, ConnFramers, FcConnection};

// FC frames -> MAVLink socket clients. The socket contract is 4-byte
// big-endian length-prefixed in both directions (the inbound reader decodes
// the prefix); each outbound body is the v2 record, the frame's 8-byte
// little-endian host receive time followed by the raw frame
// ([`ados_protocol::mavlink_socket`]). The proxies consume the raw frame
// stream directly and are unaffected. The socket is one endpoint for routing,
// shared by the on-box clients and the cloud relay that writes through it.
pub(in crate::daemon) fn spawn_fc_to_mavlink(
    fc: &Arc<FcConnection>,
    mavlink_ipc: &Arc<IpcBroadcast>,
    cancel: &Shutdown,
    tasks: &mut Tasks,
) {
    let mavlink_ipc = mavlink_ipc.clone();
    let cancel = cancel.clone();
    let mut rx = fc.frame_feed(&["ipc", "cloud"]);
    tasks.push(tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = rx.recv() => match frame {
                    Ok(f) => match encode_record(f.rx_ns, &f.bytes) {
                        Ok(framed) => mavlink_ipc.broadcast(framed.into()).await,
                        Err(e) => tracing::warn!(error = %e, "mavlink_frame_encode_failed"),
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = cancel.wait() => break,
            }
        }
    }));
}

/// Client commands from the MAVLink and MSP sockets into the FC, plus the raw
/// MSP byte lane out to the MSP socket.
pub(in crate::daemon) fn spawn_command_pumps(
    fc: &Arc<FcConnection>,
    mut inbound: mpsc::Receiver<InboundCommand>,
    msp_ipc: &Arc<IpcBroadcast>,
    mut msp_inbound: mpsc::Receiver<InboundCommand>,
    cancel: &Shutdown,
    tasks: &mut Tasks,
) {
    // MAVLink socket client commands -> FC.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        let route_eps = fc
            .routes()
            .enabled()
            .then(|| (fc.routes().register("ipc"), fc.routes().register("cloud")));
        tasks.push(tokio::spawn(async move {
            let mut framers = ConnFramers::default();
            loop {
                tokio::select! {
                    cmd = inbound.recv() => match cmd {
                        // The on-box IPC socket is not reachable off the node,
                        // so a caller here has already crossed a boundary the
                        // raw network sockets do not have — unless the caller
                        // is forwarding bytes that reached the node some other
                        // way, which it now says so the reading is its own
                        // rather than the socket's.
                        Some(InboundCommand { payload, peer }) => {
                            let origin = ClientOrigin::from_ipc_peer(&peer);
                            for batch in visor_frames(&fc, &mut framers, peer.conn, payload).await {
                                if let Some((ipc, cloud)) = &route_eps {
                                    let ep = if origin == ClientOrigin::Relayed { cloud } else { ipc };
                                    fc.routes().ingress(ep, &batch);
                                }
                                let _ = fc.send_client_bytes(&batch, origin, peer.injector.as_ref()).await;
                            }
                        }
                        None => break,
                    },
                    _ = cancel.wait() => break,
                }
            }
        }));
    }

    // FC raw MSP bytes -> MSP socket clients. An MSP FC's FC->host bytes travel
    // the raw byte lane (the frame lane stays silent for it), so length-prefix each
    // chunk exactly like the MAVLink socket and fan it to the MSP clients. A ≤2 KB
    // MSP response fits the 64 KB frame cap; the byte stream is never parsed here
    // (transparent passthrough). The `RecvError::Lagged`/`Closed` handling mirrors
    // the MAVLink frame producer above.
    {
        let msp_ipc = msp_ipc.clone();
        let cancel = cancel.clone();
        let mut raw_rx = fc.subscribe_raw();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    chunk = raw_rx.recv() => match chunk {
                        Ok(bytes) => match encode_frame(&bytes, MAVLINK_MAX_FRAME) {
                            Ok(framed) => msp_ipc.broadcast(framed.into()).await,
                            Err(e) => tracing::warn!(error = %e, "msp_chunk_encode_failed"),
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = cancel.wait() => break,
                }
            }
        }));
    }

    // MSP socket client commands -> FC. The GCS->FC path is protocol-agnostic, so
    // the received bytes are written verbatim to the FC (no MSP parse), identical
    // to the MAVLink socket's inbound path.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut framers = ConnFramers::default();
            loop {
                tokio::select! {
                    cmd = msp_inbound.recv() => match cmd {
                        Some(InboundCommand { payload, peer }) => {
                            let origin = ClientOrigin::from_ipc_peer(&peer);
                            for batch in visor_frames(&fc, &mut framers, peer.conn, payload).await {
                                fc.send_client_raw(&batch, origin, peer.injector.as_ref()).await;
                            }
                        }
                        None => break,
                    },
                    _ = cancel.wait() => break,
                }
            }
        }));
    }
}

/// The writes to pass on for one IPC payload. On a visor with a MAVLink flight
/// controller the flight-action filter must see whole frames, so a frame a
/// writer split across two payloads is reassembled per connection first;
/// everywhere else the payload passes unchanged.
async fn visor_frames(
    fc: &FcConnection,
    framers: &mut ConnFramers,
    conn: u64,
    payload: Vec<u8>,
) -> Vec<Vec<u8>> {
    if fc.is_visor() && fc.fc_variant().await.is_none() {
        framers.push(conn, &payload)
    } else {
        vec![payload]
    }
}
