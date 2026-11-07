//! The drone pilot's socket on a ground station.
//!
//! A ground station runs no pilot of its own. This serves `pilot.sock` with the
//! same `pilot.v1` framing so the ground station's native controller service,
//! its cockpit and the control front's pilot stream talk to it exactly as they
//! would to a drone's pilot:
//!
//! - `input`, `release`, `session.take`, `hb` and `action` requests go up the
//!   aux uplink on the `Pilot` channel, one datagram each and never batched,
//!   addressed to the drone whose state arrived last (0 before any did). An
//!   action is answered here with whether it left the ground station; the
//!   drone's own verdict shows in the state that follows.
//! - `subscribe` clients receive every `state` the drone sends down.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use ados_protocol::aux_egress::AuxEgress;
use ados_protocol::aux_mux::AuxChannel;
use ados_protocol::pilot::{self, ActionResult, Request};
use ados_protocol::shutdown::Shutdown;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

use crate::aux_consumer::PilotStateSink;

/// The socket path (the run dir, honouring `ADOS_RUN_DIR`).
pub fn socket_path() -> PathBuf {
    PathBuf::from(crate::paths::run_path(ados_protocol::paths::PILOT_SOCK))
}

pub struct PilotRelay {
    egress: Option<AuxEgress>,
    /// System id of the flight controller whose state arrived last.
    drone: AtomicU8,
    state: broadcast::Sender<Arc<Vec<u8>>>,
}

impl PilotRelay {
    pub fn new(egress: Option<AuxEgress>) -> Arc<Self> {
        let (state, _) = broadcast::channel(16);
        Arc::new(Self {
            egress,
            drone: AtomicU8::new(0),
            state,
        })
    }

    /// Send one request body up to the drone.
    async fn uplink(&self, body: &[u8]) -> bool {
        let Some(egress) = &self.egress else {
            return false;
        };
        let mut payload = Vec::with_capacity(body.len() + 1);
        payload.push(self.drone.load(Ordering::Relaxed));
        payload.extend_from_slice(body);
        match egress.send(AuxChannel::Pilot, &payload).await {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(error = %e, "pilot_relay_uplink_failed");
                false
            }
        }
    }
}

impl PilotStateSink for PilotRelay {
    fn deliver(&self, payload: &[u8]) {
        let Some((&sysid, body)) = payload.split_first() else {
            return;
        };
        if pilot::peek_op(body).is_ok_and(|(op, _)| op == "state") {
            if sysid != 0 {
                self.drone.store(sysid, Ordering::Relaxed);
            }
            let _ = self.state.send(Arc::new(body.to_vec()));
        }
    }
}

/// Serve the socket until `shutdown` fires.
pub async fn serve(relay: Arc<PilotRelay>, path: &Path, shutdown: Shutdown) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "pilot_relay_bind_failed");
            return;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
    }
    tracing::info!(path = %path.display(), "pilot_relay_listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(client(Arc::clone(&relay), stream));
                }
                Err(e) => tracing::debug!(error = %e, "pilot_relay_accept_failed"),
            },
            _ = shutdown.wait() => return,
        }
    }
}

async fn client(relay: Arc<PilotRelay>, stream: UnixStream) {
    let (mut rd, mut wr) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
    let writer = tokio::spawn(async move {
        while let Some(b) = out_rx.recv().await {
            if pilot::write_msg(&mut wr, &b).await.is_err() {
                break;
            }
        }
    });
    let mut forwarder: Option<tokio::task::JoinHandle<()>> = None;
    while let Ok(Some(body)) = pilot::read_msg(&mut rd).await {
        let Ok(req) = pilot::decode_request(&body) else {
            continue;
        };
        match req {
            Request::Subscribe if forwarder.is_none() => {
                let mut rx = relay.state.subscribe();
                let out = out_tx.clone();
                forwarder = Some(tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(b) => {
                                if out.try_send(b.as_ref().clone()).is_err() && out.is_closed() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }));
            }
            Request::Input(_)
            | Request::Release { .. }
            | Request::SessionTake { .. }
            | Request::Hb { .. } => {
                relay.uplink(&body).await;
            }
            Request::Action(a) => {
                let sent = relay.uplink(&body).await;
                let reply = ActionResult {
                    name: a.name,
                    id: a.id,
                    ok: sent,
                    err: (!sent).then(|| "aux_unavailable".to_owned()),
                };
                if let Ok(b) = pilot::encode_op("action_result", &reply) {
                    let _ = out_tx.send(b).await;
                }
            }
            _ => {}
        }
    }
    if let Some(f) = forwarder {
        f.abort();
    }
    drop(out_tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::aux_mux;
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn sticks_go_up_and_state_comes_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pilot.sock");
        let radio = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let out = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        out.connect(radio.local_addr().unwrap()).await.unwrap();
        let relay = PilotRelay::new(Some(AuxEgress::connected_for_test(out)));
        let shutdown = Shutdown::new();
        {
            let (relay, path, shutdown) = (Arc::clone(&relay), path.clone(), shutdown.clone());
            tokio::spawn(async move { serve(relay, &path, shutdown).await });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (mut rd, mut wr) = stream.into_split();
        pilot::write_msg(&mut wr, &pilot::encode_op("subscribe", &()).unwrap())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The drone's state arrives: the client sees it, and the relay learns
        // the drone's system id.
        let state = pilot::encode_op("state", &serde_json::json!({"owner": "pilot"})).unwrap();
        let mut down = vec![7u8];
        down.extend(&state);
        relay.deliver(&down);
        let got = pilot::read_msg(&mut rd).await.unwrap().unwrap();
        assert_eq!(pilot::peek_op(&got).unwrap().0, "state");

        // A stick frame goes up on the Pilot channel, addressed to that drone.
        let input = serde_json::json!({"op": "input", "src": "gs-native", "session": 3, "seq": 1,
            "t_ms": 5, "axes": [0.0, 0.5, 0.0, 0.0], "buttons": 0});
        let (_, body) = pilot::json_to_request_body(&input).unwrap();
        pilot::write_msg(&mut wr, &body).await.unwrap();
        let mut buf = [0u8; 2048];
        let n = radio.recv(&mut buf).await.unwrap();
        let (ch, inner) = aux_mux::decode(&buf[..n]).unwrap();
        assert_eq!(ch, AuxChannel::Pilot);
        assert_eq!(inner[0], 7);
        assert_eq!(&inner[1..], &body[..]);

        // An action is answered locally.
        let action =
            serde_json::json!({"op": "action", "name": "pause", "src": "gs-native", "id": "a1"});
        let (_, body) = pilot::json_to_request_body(&action).unwrap();
        pilot::write_msg(&mut wr, &body).await.unwrap();
        let reply = pilot::read_msg(&mut rd).await.unwrap().unwrap();
        let (op, v) = pilot::peek_op(&reply).unwrap();
        assert_eq!(op, "action_result");
        let v = serde_json::to_value(&v).unwrap();
        assert_eq!(
            (v["id"].as_str(), v["ok"].as_bool()),
            (Some("a1"), Some(true))
        );
    }
}
