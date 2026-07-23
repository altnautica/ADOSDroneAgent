//! The ground-side injector: chunk a config request onto the bearer and await
//! the drone's reassembled response, correlated by `request_id`.
//!
//! The bearer is a shared broadcast lane (not a fresh socket per request), so
//! correlation is a `request_id → oneshot::Sender` pending-map: `submit`
//! registers a oneshot, sends the chunked request, and awaits the oneshot; a
//! background receive loop reassembles inbound RESPONSE frames and resolves the
//! matching oneshot. A request that never gets a reply times out and frees its
//! pending slot — nothing accumulates.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch, Mutex};
use tokio::time::Instant;

use ados_protocol::mavlink::{build_tunnel_v2, tunnel_payload, tunnel_payload_type, MavHeader};
use ados_protocol::tunnel_config::{
    chunk_message, CompletedMessage, PushOutcome, Reassembler, TunnelConfigError,
    CONFIG_TUNNEL_PAYLOAD_TYPE,
};

use crate::stats::Counters;
use crate::transport::TunnelTransport;
use crate::{MAX_BODY_BYTES, MAX_CHUNKS, REASSEMBLY_TIMEOUT, SWEEP_INTERVAL};

/// The drone's reassembled reply to one submitted request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectorResponse {
    /// True when the drone returned an error envelope (the `is_error` chunk
    /// flag), false on a config-surface success body relayed verbatim.
    pub is_error: bool,
    pub body: Vec<u8>,
}

/// Why a `submit` failed to produce a reply.
#[derive(Debug, thiserror::Error)]
pub enum InjectorError {
    /// The request body could not be chunked (it exceeded the chunk budget).
    #[error("request could not be chunked: {0}")]
    Chunk(#[from] TunnelConfigError),
    /// No response arrived within the deadline. Honest: over a lossy low-rate
    /// bearer a request or its reply can be dropped; the caller sees a timeout,
    /// never a fabricated success.
    #[error("no response within {0:?}")]
    Timeout(Duration),
    /// The bearer send failed for every frame of the request.
    #[error("bearer send failed")]
    SendFailed,
}

/// The injector handle: submit requests and read the shared counters. The
/// background receive loop is spawned by [`Injector::spawn`].
pub struct Injector {
    transport: Arc<dyn TunnelTransport>,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<CompletedMessage>>>>,
    next_id: AtomicU32,
    seq: AtomicU32,
    counters: Arc<Counters>,
}

impl Injector {
    /// Build the injector and spawn its receive loop (which runs until
    /// `shutdown` flips true).
    pub fn spawn(
        transport: Arc<dyn TunnelTransport>,
        counters: Arc<Counters>,
        shutdown: watch::Receiver<bool>,
    ) -> Arc<Self> {
        let injector = Arc::new(Self {
            transport,
            pending: Arc::new(Mutex::new(HashMap::new())),
            // Start ids at 1 so 0 is never a live correlation id.
            next_id: AtomicU32::new(1),
            seq: AtomicU32::new(0),
            counters,
        });
        let recv = injector.clone();
        tokio::spawn(async move { recv.recv_loop(shutdown).await });
        injector
    }

    /// The shared counters for the sidecar.
    #[must_use]
    pub fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }

    /// Chunk `op_body` onto the bearer as a request and await the drone's
    /// reassembled reply, or time out.
    pub async fn submit(
        &self,
        op_body: &[u8],
        timeout: Duration,
    ) -> Result<InjectorResponse, InjectorError> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frames = chunk_message(request_id, false, false, op_body, MAX_CHUNKS)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id, tx);

        let mut any_sent = false;
        for payload in frames {
            let seq = (self.seq.fetch_add(1, Ordering::Relaxed) & 0xFF) as u8;
            let header = MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: seq,
            };
            match build_tunnel_v2(header, CONFIG_TUNNEL_PAYLOAD_TYPE, 0, 0, &payload) {
                Ok(frame) => {
                    if self.transport.send_frame(&frame).await.is_ok() {
                        any_sent = true;
                        self.counters.mark_tx();
                    }
                }
                Err(e) => tracing::warn!(error = %e, "config tunnel request frame build failed"),
            }
        }
        if !any_sent {
            self.pending.lock().await.remove(&request_id);
            return Err(InjectorError::SendFailed);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(msg)) => Ok(InjectorResponse {
                is_error: msg.is_error,
                body: msg.body,
            }),
            // The sender was dropped without a reply (never happens today), or
            // the deadline passed — either way free the slot and report honest.
            Ok(Err(_)) | Err(_) => {
                self.pending.lock().await.remove(&request_id);
                Err(InjectorError::Timeout(timeout))
            }
        }
    }

    async fn recv_loop(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut re = Reassembler::new(MAX_CHUNKS, MAX_BODY_BYTES);
        let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                _ = sweep.tick() => {
                    let dropped = re.sweep(Instant::now().into_std(), REASSEMBLY_TIMEOUT);
                    if dropped > 0 {
                        self.counters.add_timeouts(dropped as u64);
                    }
                }
                frame = self.transport.recv_frame() => {
                    let Ok(frame) = frame else { continue };
                    if tunnel_payload_type(&frame) != Some(CONFIG_TUNNEL_PAYLOAD_TYPE) {
                        continue;
                    }
                    let Some(payload) = tunnel_payload(&frame) else { continue };
                    self.counters.mark_rx();
                    match re.push(&payload, Instant::now().into_std()) {
                        // Only RESPONSE frames are ours to correlate; a request
                        // echoed onto the shared bearer is ignored.
                        PushOutcome::Complete(msg) if msg.is_response => {
                            if let Some(tx) = self.pending.lock().await.remove(&msg.request_id) {
                                self.counters.mark_response();
                                let _ = tx.send(msg);
                            }
                        }
                        PushOutcome::Complete(_) => {}
                        PushOutcome::Rejected(reason) => {
                            self.counters.mark_rejected();
                            tracing::warn!(reason, "config tunnel response chunk rejected");
                        }
                        PushOutcome::Incomplete | PushOutcome::Ignored => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_client::{ConfigClient, ConfigResponse, Unreachable};
    use crate::terminator::run_terminator;
    use crate::transport::mock::duplex;
    use async_trait::async_trait;
    use tokio::sync::Notify;

    struct FixedClient(Vec<u8>);

    #[async_trait]
    impl ConfigClient for FixedClient {
        async fn get(&self) -> Result<ConfigResponse, Unreachable> {
            Ok(ConfigResponse {
                status: 200,
                body: self.0.clone(),
            })
        }
        async fn put(&self, _k: &str, _v: &str) -> Result<ConfigResponse, Unreachable> {
            Ok(ConfigResponse {
                status: 200,
                body: br#"{"status":"ok","persisted":true}"#.to_vec(),
            })
        }
    }

    /// The whole substrate, in-process: a GS injector and a drone terminator
    /// wired over the crossed mock bearer, a multi-chunk config body relayed
    /// end-to-end.
    #[tokio::test]
    async fn injector_and_terminator_round_trip_a_multichunk_body() {
        let (gs_side, drone_side) = duplex();
        let (sd_tx, sd_rx) = watch::channel(false);

        // A response body bigger than one 116-byte chunk to exercise chunking
        // both ways (the request is tiny, the response spans frames).
        let big_body = format!(r#"{{"blob":"{}"}}"#, "z".repeat(300)).into_bytes();
        let client: Arc<dyn ConfigClient> = Arc::new(FixedClient(big_body.clone()));
        let drone_counters = Arc::new(Counters::default());
        let drone = tokio::spawn(run_terminator(
            Arc::new(drone_side),
            false,
            client,
            drone_counters.clone(),
            sd_rx.clone(),
            Arc::new(Notify::new()),
        ));

        let gs_counters = Arc::new(Counters::default());
        let injector = Injector::spawn(Arc::new(gs_side), gs_counters.clone(), sd_rx);

        let resp = injector
            .submit(br#"{"op":"get"}"#, Duration::from_secs(5))
            .await
            .expect("a reply arrives");
        assert!(!resp.is_error);
        assert_eq!(resp.body, big_body);

        // Counters told the truth on both sides.
        assert!(gs_counters.snapshot().responses >= 1);
        assert!(drone_counters.snapshot().requests >= 1);
        assert!(drone_counters.snapshot().tx_frames >= 3); // multi-chunk reply

        let _ = sd_tx.send(true);
        let _ = drone.await;
    }

    #[tokio::test(start_paused = true)]
    async fn submit_times_out_when_no_terminator_answers() {
        // A GS injector with a dead peer: the request goes out but nothing
        // replies, so submit returns an honest timeout, not a hang.
        let (gs_side, _dead_peer) = duplex();
        let (_sd_tx, sd_rx) = watch::channel(false);
        let injector = Injector::spawn(Arc::new(gs_side), Arc::new(Counters::default()), sd_rx);
        let err = injector
            .submit(br#"{"op":"get"}"#, Duration::from_secs(2))
            .await
            .expect_err("no reply");
        assert!(matches!(err, InjectorError::Timeout(_)));
    }
}
