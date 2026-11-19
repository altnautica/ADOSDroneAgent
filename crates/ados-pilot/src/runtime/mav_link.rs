//! The pilot's MAVLink socket session to the router, and its part in the
//! simulator's lockstep barrier.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::mavlink::{self, ardupilotmega::MavMessage, MavHeader};
use ados_protocol::mavlink_socket;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{COMPID, SYSID};

/// A request to answer once every frame queued before it is written.
pub(super) type Sync = oneshot::Sender<()>;

/// The core's part in the lockstep barrier (`simulation.clock_mode:
/// lockstep`): after a tick and its flush, the core reports done until its
/// next tick, once the socket writer has written every frame the tick
/// queued, so the FC's commands are on their way before the simulator steps.
pub(super) struct Lockstep {
    part: Arc<ados_time::Participant>,
    sync: mpsc::UnboundedSender<Sync>,
}

impl Lockstep {
    /// Register with the barrier at `socket`; `sync` reaches the socket
    /// writer. Needs a Tokio runtime.
    pub(super) fn new(socket: PathBuf, sync: mpsc::UnboundedSender<Sync>) -> Self {
        Self {
            part: Arc::new(ados_time::Participant::connect(socket)),
            sync,
        }
    }

    /// The tick's frames are queued: report done until `next_due_ns` once
    /// the writer has written them.
    pub(super) fn after_flush(&self, next_due_ns: u64) {
        let (tx, rx) = oneshot::channel();
        if self.sync.send(tx).is_err() {
            return;
        }
        let part = Arc::clone(&self.part);
        tokio::spawn(async move {
            if rx.await.is_ok() {
                part.done(next_due_ns);
            }
        });
    }
}

/// The next sync request; never, without lockstep or once it is closed.
async fn next_sync(rx: &mut Option<mpsc::UnboundedReceiver<Sync>>) -> Sync {
    match rx {
        Some(r) => match r.recv().await {
            Some(s) => s,
            None => {
                *rx = None;
                std::future::pending().await
            }
        },
        None => std::future::pending().await,
    }
}

/// Write one frame with the pilot's identity; false when the socket failed.
async fn write_mav(wr: &mut OwnedWriteHalf, seq: &mut u8, m: &MavMessage) -> bool {
    let header = MavHeader {
        system_id: SYSID,
        component_id: COMPID,
        sequence: *seq,
    };
    *seq = seq.wrapping_add(1);
    let Ok(bytes) = mavlink::serialize_v2(header, m) else {
        return true;
    };
    let Ok(f) = ados_protocol::frame::encode_frame(&bytes, ados_protocol::frame::MAVLINK_MAX_FRAME)
    else {
        return true;
    };
    wr.write_all(&f).await.is_ok()
}

/// Keep a MAVLink socket session alive: read FC frames into `frames`, write
/// queued frames with the pilot's identity. A sync request is answered once
/// every frame queued before it is written; while the router is not
/// connected there is nothing to write and it is answered at once.
pub(super) async fn mavlink_link(
    path: PathBuf,
    mut out_rx: mpsc::Receiver<MavMessage>,
    mut sync_rx: Option<mpsc::UnboundedReceiver<Sync>>,
    frames: broadcast::Sender<(u8, u8, MavMessage)>,
    version_frames: broadcast::Sender<Vec<u8>>,
) {
    let mut seq: u8 = 0;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                tracing::info!(path = %path.display(), "pilot_mavlink_connected");
                let (mut rd, mut wr) = stream.into_split();
                let decl = ados_protocol::ipc::encode_injector_declaration("pilot", None);
                let ok = match ados_protocol::frame::encode_frame(
                    &decl,
                    ados_protocol::frame::MAVLINK_MAX_FRAME,
                ) {
                    Ok(f) => wr.write_all(&f).await.is_ok(),
                    Err(_) => false,
                };
                if ok {
                    let frames = frames.clone();
                    let version_frames = version_frames.clone();
                    let reader = tokio::spawn(async move {
                        while let Ok(Some(rec)) = mavlink_socket::read_frame(&mut rd).await {
                            if let Ok((h, msg)) = mavlink::parse_any(&rec.frame) {
                                if h.system_id == SYSID && h.component_id == COMPID {
                                    continue;
                                }
                                if matches!(msg, MavMessage::AUTOPILOT_VERSION(_)) {
                                    let _ = version_frames.send(rec.frame.clone());
                                }
                                let _ = frames.send((h.system_id, h.component_id, msg));
                            }
                        }
                    });
                    loop {
                        tokio::select! {
                            m = out_rx.recv() => {
                                let Some(m) = m else { break };
                                if !write_mav(&mut wr, &mut seq, &m).await || reader.is_finished() {
                                    break;
                                }
                            }
                            s = next_sync(&mut sync_rx) => {
                                // Every frame queued before the request is in
                                // the channel: write them, then answer.
                                let mut alive = true;
                                while let Ok(m) = out_rx.try_recv() {
                                    if !write_mav(&mut wr, &mut seq, &m).await {
                                        alive = false;
                                        break;
                                    }
                                }
                                let _ = s.send(());
                                if !alive || reader.is_finished() {
                                    break;
                                }
                            }
                        }
                    }
                    if out_rx.is_closed() {
                        reader.abort();
                        return;
                    }
                    reader.abort();
                }
            }
            Err(e) => tracing::debug!(error = %e, "pilot_mavlink_connect_failed"),
        }
        let retry = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(retry);
        loop {
            tokio::select! {
                () = &mut retry => break,
                s = next_sync(&mut sync_rx) => { let _ = s.send(()); }
            }
        }
    }
}
