//! Consume the drone's aux UPLINK (radio_id 3) and inject decoded MAVLink
//! into the local flight controller.
//!
//! The mirror of `ados-groundlink::aux_consumer` (which does the equivalent
//! job on the ground, for the opposite direction), and the receiving
//! counterpart to [`crate::aux_tee`] (which reads the FC and radiates
//! outward on radio_id 2). The drone's `wfb_rx -p3` has always run and
//! re-emitted decoded uplink datagrams to a loopback port; until now nothing
//! read that port, so a ground station's `PARAM_REQUEST_LIST` — or any other
//! client command relayed over the radio — arrived at the ground station and
//! went no further.
//!
//! Datagrams may carry several batched frames, the same way the drone's own
//! downlink tee batches: [`crate::proxies`]'s `send_client_bytes` fallback
//! (`ados-mavlink-router::aux_uplink`, the ground-side sender this consumes)
//! batches a client's outbound bytes for the same packet-rate reason, so
//! frames are split on the header-derived boundary before injection.
//!
//! ## Decode failures are not all alike
//!
//! The lane is a shared, general-purpose application channel, so a datagram
//! whose magic does not match is simply another application's traffic and is
//! counted on its own rather than read as a fault. A datagram that matched
//! our magic and version and then failed some other check is our frame
//! arriving damaged, which is worth telling apart.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ados_protocol::aux_mux::{self, AuxChannel, AuxDecodeError};
use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

use crate::connection::FcConnection;

/// Largest datagram read in one go. Matches `ados_protocol::aux_mux::AUX_MAX_PAYLOAD`
/// plus header room; a datagram over this size cannot be one of ours anyway.
const BUF_SIZE: usize = 4096;

#[derive(Default)]
struct CountersInner {
    datagrams_received: AtomicU64,
    mavlink_frames: AtomicU64,
    mavlink_injected: AtomicU64,
    decode_foreign: AtomicU64,
    decode_damaged: AtomicU64,
    non_mavlink_channel: AtomicU64,
}

#[derive(Clone, Default)]
pub struct AuxUplinkConsumerCounters(Arc<CountersInner>);

#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct AuxUplinkConsumerSnapshot {
    pub datagrams_received: u64,
    pub mavlink_frames: u64,
    pub mavlink_injected: u64,
    pub decode_foreign: u64,
    pub decode_damaged: u64,
    pub non_mavlink_channel: u64,
}

impl AuxUplinkConsumerCounters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> AuxUplinkConsumerSnapshot {
        let c = &self.0;
        AuxUplinkConsumerSnapshot {
            datagrams_received: c.datagrams_received.load(Ordering::Relaxed),
            mavlink_frames: c.mavlink_frames.load(Ordering::Relaxed),
            mavlink_injected: c.mavlink_injected.load(Ordering::Relaxed),
            decode_foreign: c.decode_foreign.load(Ordering::Relaxed),
            decode_damaged: c.decode_damaged.load(Ordering::Relaxed),
            non_mavlink_channel: c.non_mavlink_channel.load(Ordering::Relaxed),
        }
    }
}

/// Bind `port` (the drone's aux-uplink re-emit loopback — `WfbConfig::aux_rx_port`,
/// default 5603) and inject every decoded MAVLink frame into `fc`. Runs until
/// `cancel` fires; a bind failure is logged and the task exits (the uplink is
/// simply unavailable, matching how a missing aux receiver already degrades
/// elsewhere in this pipeline).
pub async fn run(
    port: u16,
    fc: Arc<FcConnection>,
    counters: AuxUplinkConsumerCounters,
    cancel: Arc<Notify>,
) {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let sock = match UdpSocket::bind(addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, port, "aux_uplink_consumer_bind_failed");
            return;
        }
    };
    tracing::info!(port, "aux_uplink_consumer_started");

    let mut buf = [0u8; BUF_SIZE];
    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => break,
            recvd = sock.recv(&mut buf) => {
                match recvd {
                    Ok(n) => {
                        counters.0.datagrams_received.fetch_add(1, Ordering::Relaxed);
                        dispatch(&buf[..n], &fc, &counters).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "aux_uplink_consumer_recv_failed");
                    }
                }
            }
        }
    }
    tracing::info!(port, "aux_uplink_consumer_stopped");
}

async fn dispatch(payload: &[u8], fc: &Arc<FcConnection>, counters: &AuxUplinkConsumerCounters) {
    let (channel, inner) = match aux_mux::decode(payload) {
        Ok(v) => v,
        Err(AuxDecodeError::BadMagic) => {
            counters.0.decode_foreign.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => {
            counters.0.decode_damaged.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    if channel != AuxChannel::Mavlink {
        counters
            .0
            .non_mavlink_channel
            .fetch_add(1, Ordering::Relaxed);
        return;
    }

    // A sender may batch several frames into one datagram (see
    // `aux_uplink::run`'s own batching); split on the header-derived
    // boundary, which needs no dialect knowledge. A payload that yields no
    // whole frame is injected intact rather than dropped — an older sender
    // that predates batching sends exactly one frame per datagram.
    let split = aux_mux::split_frames(inner);
    let frames: Vec<&[u8]> = if split.is_empty() { vec![inner] } else { split };
    counters
        .0
        .mavlink_frames
        .fetch_add(frames.len() as u64, Ordering::Relaxed);
    for frame in frames {
        fc.send_bytes(frame).await;
        counters.0.mavlink_injected.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MavlinkConfig;
    use crate::param_cache::ParamCache;
    use crate::state::VehicleState;
    use ados_protocol::mavlink::ardupilotmega::{
        MavAutopilot, MavMessage, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
    };
    use ados_protocol::mavlink::{self, MavHeader};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;
    use tokio::sync::Mutex;

    fn heartbeat_bytes() -> Vec<u8> {
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_GCS,
            autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        });
        mavlink::serialize_v2(
            MavHeader {
                system_id: 255,
                component_id: 190,
                sequence: 0,
            },
            &msg,
        )
        .unwrap()
    }

    /// Captures every write, standing in for a real FC serial port so a test
    /// can assert exactly what reached "the flight controller".
    struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl AsyncWrite for CapturingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_connection() -> (Arc<FcConnection>, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
        let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
            "/tmp/ados-uplink-test-params.json",
        )));
        let fc = FcConnection::new(MavlinkConfig::default(), state, params);
        (fc, std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
    }

    #[tokio::test]
    async fn a_decoded_uplink_frame_is_injected_into_the_fc_writer() {
        let (fc, captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

        let counters = AuxUplinkConsumerCounters::new();
        let frame = heartbeat_bytes();
        let datagram = aux_mux::encode(AuxChannel::Mavlink, &frame).unwrap();

        dispatch(&datagram, &fc, &counters).await;

        assert_eq!(*captured.lock().unwrap(), frame);
        let snap = counters.snapshot();
        assert_eq!(snap.mavlink_frames, 1);
        assert_eq!(snap.mavlink_injected, 1);
        assert_eq!(snap.decode_foreign, 0);
        assert_eq!(snap.decode_damaged, 0);
    }

    #[tokio::test]
    async fn foreign_traffic_on_the_shared_lane_is_counted_not_alarmed() {
        let (fc, captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

        let counters = AuxUplinkConsumerCounters::new();
        dispatch(b"not-an-aux-frame-at-all", &fc, &counters).await;

        assert!(
            captured.lock().unwrap().is_empty(),
            "foreign bytes must never reach the FC"
        );
        assert_eq!(counters.snapshot().decode_foreign, 1);
    }

    #[tokio::test]
    async fn a_batched_datagram_splits_into_separate_injections() {
        let (fc, captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

        let counters = AuxUplinkConsumerCounters::new();
        let one = heartbeat_bytes();
        let mut batch = one.clone();
        batch.extend_from_slice(&one);
        let datagram = aux_mux::encode(AuxChannel::Mavlink, &batch).unwrap();

        dispatch(&datagram, &fc, &counters).await;

        assert_eq!(
            *captured.lock().unwrap(),
            batch,
            "both frames must reach the FC, in order"
        );
        assert_eq!(counters.snapshot().mavlink_frames, 2);
        assert_eq!(counters.snapshot().mavlink_injected, 2);
    }
}
