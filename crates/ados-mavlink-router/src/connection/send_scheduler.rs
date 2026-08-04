//! The FC send paths and the three periodic send cadences.
//!
//! Splits the writer-side of [`FcConnection`] out of the connect/read FSM: the
//! raw byte/message send primitives, the 1 Hz companion heartbeat, the adaptive
//! stream-interval requests, and the rate-limited parameter sweep. Each is a
//! method on `FcConnection` using its interior mutability, so the run loop and
//! the periodic sender tasks share one connection.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::{
    MavAutopilot, MavCmd, MavMessage, MavModeFlag, MavState, MavType, COMMAND_LONG_DATA,
    HEARTBEAT_DATA, PARAM_REQUEST_LIST_DATA, REQUEST_DATA_STREAM_DATA,
};
use ados_protocol::mavlink::{self, MavHeader};

use super::transport::write_then_flush;
use super::FcConnection;

/// Adaptive stream-request cadence (mirrors connection.py:24-32).
const STREAM_MIN: Duration = Duration::from_secs(10);
pub(super) const STREAM_DEFAULT: Duration = Duration::from_secs(30);
const STREAM_MAX: Duration = Duration::from_secs(60);
const STREAM_STALL: Duration = Duration::from_secs(5);
const STREAM_HEALTHY: Duration = Duration::from_secs(2);
const STREAM_STEP: Duration = Duration::from_secs(5);

/// Per-message stream rates requested from the FC: `(MAVLink message id, Hz)`.
const STREAM_RATES: &[(u32, f32)] = &[
    (0, 1.0),   // HEARTBEAT
    (30, 10.0), // ATTITUDE
    (33, 5.0),  // GLOBAL_POSITION_INT
    (1, 2.0),   // SYS_STATUS
    (24, 2.0),  // GPS_RAW_INT
    (74, 4.0),  // VFR_HUD
    (147, 1.0), // BATTERY_STATUS
    (65, 4.0),  // RC_CHANNELS
];

/// Legacy data-stream groups requested via `REQUEST_DATA_STREAM`: `(MAV_DATA_STREAM
/// id, Hz)`. Sent only when `mavlink.legacy_stream_request` is on; see
/// [`FcConnection::tick_streams`]. Some firmwares (iNav, older ArduPilot,
/// Betaflight's MAVLink telemetry) honor only this legacy mechanism and ignore
/// `SET_MESSAGE_INTERVAL`, which is what the flag is for.
/// `MAV_DATA_STREAM_ALL` (id 0) is deliberately omitted: it would overlap the
/// specific groups and double-rate on any firmware that honored both.
const STREAM_GROUPS: &[(u8, u16)] = &[
    (2, 2),   // EXTENDED_STATUS — SYS_STATUS, GPS_RAW_INT
    (6, 5),   // POSITION — GLOBAL_POSITION_INT
    (10, 10), // EXTRA1 — ATTITUDE
    (11, 4),  // EXTRA2 — VFR_HUD
    (3, 4),   // RC_CHANNELS
];

/// Parameter sweep timing.
const PARAM_RATE_LIMIT: Duration = Duration::from_secs(10);
const PARAM_DEADLINE: Duration = Duration::from_secs(30);

/// Where a client's bytes came from, as far as the socket that accepted them
/// could tell.
///
/// On a raw MAVLink socket only two answers are possible, because there is no
/// header to carry a credential and no handshake to hang one on: either the
/// peer address cleared the posture check, or it did not. The on-box IPC socket
/// can say one thing more, because a writer there can tell the socket it is
/// forwarding rather than producing. This is not a permission — it is
/// provenance, recorded so the one path that amplifies a client's reach can say
/// what it is amplifying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientOrigin {
    /// The peer cleared the posture check, or arrived on a trusted local
    /// transport (an on-box IPC socket, which is not reachable off the node)
    /// having claimed nothing to the contrary.
    Trusted,
    /// An on-box writer that has declared it is forwarding bytes which reached
    /// this node from elsewhere. Locally delivered, but not locally produced:
    /// the reach behind these bytes is whatever guards the hop they arrived on,
    /// which is not the on-box socket's own unreachability.
    Relayed,
    /// The peer did not clear the posture check. Admitted today, because these
    /// ports are advertised for third-party ground stations, but recorded.
    Unauthenticated,
}

impl ClientOrigin {
    /// Classify a payload the on-box IPC socket accepted.
    ///
    /// The socket is not reachable off the node, so a writer that says nothing
    /// is on-box in the sense that matters. A writer that has declared itself a
    /// forwarder is not, and the difference is the whole reason the identity is
    /// carried: the previous reading was a hardcoded [`Self::Trusted`] applied
    /// to every writer alike, which recorded traffic that had crossed a broker
    /// as if it had been produced on the node.
    pub fn from_ipc_peer(peer: &ados_protocol::ipc::IpcPeer) -> Self {
        if peer.off_box_source {
            Self::Relayed
        } else {
            Self::Trusted
        }
    }
}

impl FcConnection {
    pub(super) fn next_seq(&self) -> u8 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn our_header(&self) -> MavHeader {
        MavHeader {
            system_id: self.cfg.system_id,
            component_id: self.cfg.component_id,
            sequence: self.next_seq(),
        }
    }

    /// Write raw bytes to the FC (a client command). No-op when disconnected.
    /// On a write/flush error the current writer is dropped and a reconnect is
    /// signalled so the run loop tears the link down and re-opens it with a
    /// fresh writer. The write path deliberately does NOT clear `connected`:
    /// the run loop owns that lifecycle, so a transient write error during a
    /// heavy parameter dump (with reads still flowing) recovers to a live link
    /// rather than latching the FC permanently "disconnected".
    pub async fn send_bytes(&self, data: &[u8]) {
        let mut guard = self.writer.lock().await;
        if let Some(w) = guard.as_mut() {
            match write_then_flush(w, data).await {
                Ok(()) => self.wrote_since_open.store(true, Ordering::Relaxed),
                Err(e) => {
                    *guard = None;
                    drop(guard);
                    tracing::warn!(error = %e, "fc_write_failed");
                    self.reconnect.notify_one();
                }
            }
        }
    }

    /// Write raw bytes with a wall bound, treating a timeout as a broken link.
    ///
    /// The bound has to be applied here, inside the writer lock, rather than by
    /// wrapping [`Self::send_bytes`] at the call site. Cancelling that future
    /// mid-flight is not free: `write_then_flush` may already have put an
    /// arbitrary prefix of the frame on the wire, and dropping it releases the
    /// lock with the stream in an unknown state. The next frame is then written
    /// straight onto that prefix, and the flight controller sees a corrupt frame
    /// it reports as a CRC failure — indistinguishable from line noise, and
    /// attributed to the radio rather than to us.
    ///
    /// So a timeout drops the writer and asks for a reconnect, exactly as a
    /// write error already does. Losing one frame and re-opening the link is the
    /// honest outcome; silently corrupting the following frame is not.
    ///
    /// Returns whether the frame was written in full.
    pub async fn send_bytes_bounded(&self, data: &[u8], budget: Duration) -> bool {
        let mut guard = self.writer.lock().await;
        let Some(w) = guard.as_mut() else {
            return false;
        };
        match tokio::time::timeout(budget, write_then_flush(w, data)).await {
            Ok(Ok(())) => {
                self.wrote_since_open.store(true, Ordering::Relaxed);
                true
            }
            Ok(Err(e)) => {
                *guard = None;
                drop(guard);
                tracing::warn!(error = %e, "fc_write_failed");
                self.reconnect.notify_one();
                false
            }
            Err(_) => {
                // The stream may carry a partial frame. Re-open rather than
                // write the next frame onto it.
                *guard = None;
                drop(guard);
                tracing::warn!(
                    budget_ms = budget.as_millis() as u64,
                    "fc_write_timed_out_reconnecting"
                );
                self.reconnect.notify_one();
                false
            }
        }
    }

    /// Write raw bytes toward the flight controller on behalf of a connected
    /// GCS client (the three [`crate::proxies`] transports). Prefers the
    /// local FC writer exactly like [`Self::send_bytes`]; when none exists
    /// and an aux-uplink sender is installed (this node is relaying a linked
    /// drone rather than driving a local FC), forwards there instead of
    /// dropping the client's bytes silently.
    ///
    /// A linked ground agent holds full authority over the node it relays, so
    /// there is no per-message filter here. The message-id denylist that used to
    /// sit in this path was removed: it covered only this MAVLink lane and never
    /// the relay's HTTP lane, where the same caller could already reach the
    /// vehicle-command routes, so it filtered one door while the other stood
    /// open. Authority is settled at the relay's authentication boundary
    /// instead, which is the only place that can actually decide it.
    ///
    /// **That rationale holds for the HTTP lane and did not hold here.** The
    /// raw MAVLink sockets had no authentication boundary at all: they accepted
    /// the peer address and discarded it. So on a node with no local flight
    /// controller, bytes from an unauthenticated caller took the fallback below
    /// and were radiated to a *remote, possibly airborne* aircraft — a longer
    /// reach than the local case, since it needs no proximity to the vehicle.
    ///
    /// `caller` now carries the classification the socket made, so the fallback
    /// can tell an authorised relay client from an anonymous one. It is
    /// reported rather than refused for the same reason the socket gates are
    /// observation-only: this port is advertised as the path for a third-party
    /// ground station, and a ground station relaying a linked drone is exactly
    /// the shape that takes this fallback.
    ///
    /// **The claim that a boundary exists for every caller was not true of the
    /// on-box IPC lane either.** That lane was described as trusted because the
    /// socket cannot be reached off the node, and stamped
    /// [`ClientOrigin::Trusted`] on every payload without looking. But one of
    /// its writers is the cloud relay, whose whole job is to take bytes off a
    /// broker and put them on this socket, so the reach behind those bytes is
    /// the broker's, not the node's. On a ground station with no local flight
    /// controller they then took the fallback below and left over the radio.
    /// The socket now identifies its writers, a forwarder declares itself, and
    /// the classification says which it was — so this path can no longer report
    /// broker traffic as locally produced.
    ///
    /// Deliberately distinct from `send_bytes`, which this router's own
    /// heartbeat/stream-interval/param-sweep housekeeping also calls: those
    /// exist to talk to a directly-attached FC and must stay a silent no-op
    /// with none installed, not start radiating this router's own internal
    /// traffic onto a client's uplink.
    pub async fn send_client_bytes(
        &self,
        data: &[u8],
        caller: ClientOrigin,
        injector: Option<&ados_protocol::ipc::InjectorClaim>,
    ) {
        // PIC-arbiter gate. A writer that declared itself an autonomous injector
        // is refused whenever a human holds manual control (or the arbiter is not
        // reporting — fail closed). An undeclared writer (injector = None) is the
        // operator/human path and is NEVER gated. Inert until a producer is armed.
        if let Some(claim) = injector {
            if self.injector_gate.lock().unwrap().refused(claim) {
                tracing::warn!(
                    injector = %claim.client_id,
                    len = data.len(),
                    "injector_command_refused_operator_holds_pic"
                );
                return;
            }
        }
        let has_writer = { self.writer.lock().await.is_some() };
        if has_writer {
            self.send_bytes(data).await;
            return;
        }
        if let Some(uplink) = self.aux_uplink.lock().await.as_ref() {
            // The amplifying case: no local vehicle, so these bytes leave over
            // the radio to one that may be flying. Both non-local readings are
            // reported, and reported apart, because they are different reaches:
            // one is a caller on this LAN that presented nothing, the other is a
            // caller anywhere the broker is reachable from.
            match caller {
                ClientOrigin::Unauthenticated => {
                    tracing::warn!(len = data.len(), "relay_uplink_from_unauthenticated_client");
                }
                ClientOrigin::Relayed => {
                    tracing::warn!(len = data.len(), "relay_uplink_from_off_box_source");
                }
                ClientOrigin::Trusted => {}
            }
            uplink.send(data);
        }
    }

    /// Write raw bytes toward an MSP flight controller on behalf of a client,
    /// recording where they came from.
    ///
    /// Deliberately has no aux-uplink fallback, unlike
    /// [`Self::send_client_bytes`]. That fallback frames and radiates on the
    /// aux MAVLink lane, and MSP is a different protocol on a raw byte pipe; a
    /// ground station with no local flight controller has nothing to do with
    /// these bytes, so they stop here rather than being put on a lane that
    /// cannot carry them. What this does add is the classification the MSP path
    /// never had: it called the plain FC writer directly, so an MSP command that
    /// arrived over the broker was indistinguishable from one produced on the
    /// node.
    pub async fn send_client_raw(
        &self,
        data: &[u8],
        caller: ClientOrigin,
        injector: Option<&ados_protocol::ipc::InjectorClaim>,
    ) {
        // Same PIC-arbiter gate as send_client_bytes; MSP is a second way to
        // command an airframe, so it is gated identically.
        if let Some(claim) = injector {
            if self.injector_gate.lock().unwrap().refused(claim) {
                tracing::warn!(
                    injector = %claim.client_id,
                    len = data.len(),
                    "injector_msp_refused_operator_holds_pic"
                );
                return;
            }
        }
        if caller != ClientOrigin::Trusted {
            tracing::debug!(
                len = data.len(),
                origin = ?caller,
                "msp_command_from_non_local_source"
            );
        }
        self.send_bytes(data).await;
    }

    pub(super) async fn send_msg(&self, msg: &MavMessage) -> bool {
        match mavlink::serialize_v2(self.our_header(), msg) {
            Ok(bytes) => {
                self.send_bytes(&bytes).await;
                true
            }
            Err(_) => false,
        }
    }

    /// Send the 1 Hz companion heartbeat so the FC registers a valid GCS-class
    /// component and does not trip its GCS failsafe.
    pub async fn send_heartbeat(&self) {
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_ONBOARD_CONTROLLER,
            autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_UNINIT,
            mavlink_version: 3,
        });
        self.send_msg(&msg).await;
    }

    /// Adaptive stream request. Picks the interval from how long the link has
    /// been idle (stalled link re-requests fast; healthy link relaxes toward
    /// the max), then re-sends the per-message rates when the interval elapses.
    pub async fn tick_streams(&self) {
        if !self.transport_open() {
            return;
        }
        let idle = self.last_msg_at.lock().await.elapsed();
        {
            let mut interval = self.stream_interval.lock().await;
            *interval = if idle >= STREAM_STALL {
                STREAM_MIN
            } else if idle <= STREAM_HEALTHY {
                (*interval + STREAM_STEP).min(STREAM_MAX)
            } else {
                *interval
            };
        }
        let interval = *self.stream_interval.lock().await;
        let due = {
            let last = self.last_stream_req.lock().await;
            last.map(|t| t.elapsed() >= interval).unwrap_or(true)
        };
        if !due {
            return;
        }
        let target = self.target_system.load(Ordering::Relaxed);
        for &(msg_id, rate_hz) in STREAM_RATES {
            let interval_us = 1_000_000.0 / rate_hz;
            let cmd = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                target_system: target,
                target_component: 1,
                command: MavCmd::MAV_CMD_SET_MESSAGE_INTERVAL,
                confirmation: 0,
                param1: msg_id as f32,
                param2: interval_us,
                param3: 0.0,
                param4: 0.0,
                param5: 0.0,
                param6: 0.0,
                param7: 0.0,
            });
            self.send_msg(&cmd).await;
        }
        // The legacy `REQUEST_DATA_STREAM` groups, off by default.
        //
        // This used to be sent unconditionally, on the assumption that a firmware
        // honors either the interval requests above or the legacy groups but never
        // both. Measured MAVLink ingest on ArduPilot was 66.5 frames/s against the
        // 29 Hz the interval requests sum to — consistent with ArduPilot honoring
        // BOTH paths and streaming roughly twice the telemetry that was asked for,
        // on a radio link whose airtime is the binding constraint for a fleet.
        // Default-off therefore halves the request traffic and the ingest it
        // provokes.
        //
        // The flag, not a deletion, because the legacy path is the ONLY one iNav /
        // Betaflight / pre-4.1 ArduPilot honor: if a firmware turns out to answer
        // only `REQUEST_DATA_STREAM`, its ingest collapses instead of halving, and
        // setting `mavlink.legacy_stream_request: true` is the rollback.
        if self.cfg.legacy_stream_request {
            for &(stream_id, rate_hz) in STREAM_GROUPS {
                let req = MavMessage::REQUEST_DATA_STREAM(REQUEST_DATA_STREAM_DATA {
                    target_system: target,
                    target_component: 1,
                    req_stream_id: stream_id,
                    req_message_rate: rate_hz,
                    start_stop: 1,
                });
                self.send_msg(&req).await;
            }
        }
        *self.last_stream_req.lock().await = Some(Instant::now());
    }

    /// Parameter sweep with the priming/timeout flags the GCS spinner reads.
    /// Rate-limited to one PARAM_REQUEST_LIST per [`PARAM_RATE_LIMIT`]; flips
    /// the timeout flag when the deadline passes with no parameters cached.
    pub async fn tick_param_sweep(&self) {
        if !self.transport_open() {
            return;
        }
        // Progress check: clear priming once the cache is fully populated.
        let cached = self.params.lock().await.count();
        let expected = self.state.lock().await.param_count.max(0) as usize;
        if expected > 0 && cached >= expected {
            self.param_priming.store(false, Ordering::Relaxed);
            self.param_sweep_timed_out.store(false, Ordering::Relaxed);
            return;
        }
        let due = {
            let last = self.param_last_request.lock().await;
            last.map(|t| t.elapsed() >= PARAM_RATE_LIMIT)
                .unwrap_or(true)
        };
        if !due {
            // While priming and past the deadline with nothing cached, flag the timeout.
            if self.param_priming.load(Ordering::Relaxed) && cached == 0 {
                if let Some(started) = *self.param_sweep_started.lock().await {
                    if started.elapsed() >= PARAM_DEADLINE {
                        self.param_sweep_timed_out.store(true, Ordering::Relaxed);
                    }
                }
            }
            return;
        }

        // `due` only means PARAM_RATE_LIMIT has elapsed since our own last
        // sweep send — it says nothing about whether the download actually
        // needs help. Compare the cache size now against its size the last
        // time we checked: if it grew at all in that window, something (our
        // own prior sweep response still landing, or a downstream client's
        // own PARAM_REQUEST_LIST) is genuinely still making progress, and
        // injecting a competing PARAM_REQUEST_LIST here would livelock it —
        // the FC tracks queued-parameter state PER LINK, not per requester,
        // so a second request on this link restarts its enumeration from
        // index 0, undoing exactly the progress that is happening. Only fire
        // once the cache has genuinely stalled across a full rate-limit
        // window, which is what this housekeeping sweep exists to recover
        // from in the first place.
        let last_cached = self.param_last_cached_count.swap(cached, Ordering::Relaxed);
        if cached > last_cached {
            return;
        }

        let target = self.target_system.load(Ordering::Relaxed);
        let req = MavMessage::PARAM_REQUEST_LIST(PARAM_REQUEST_LIST_DATA {
            target_system: target,
            target_component: 1,
        });
        let ok = self.send_msg(&req).await;
        self.param_sweep_send_failed.store(!ok, Ordering::Relaxed);
        self.param_priming.store(true, Ordering::Relaxed);
        let now = Instant::now();
        *self.param_last_request.lock().await = Some(now);
        let mut started = self.param_sweep_started.lock().await;
        if started.is_none() {
            *started = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MavlinkConfig;
    use crate::param_cache::ParamCache;
    use crate::state::VehicleState;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;
    use tokio::sync::Mutex;

    /// A write half whose first `write_all` fails, standing in for a serial
    /// port that drops writes while reads keep flowing (the failure mode that
    /// used to latch the FC "disconnected" forever).
    struct FailingWriter;

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected write failure",
            )))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_connection() -> std::sync::Arc<FcConnection> {
        let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
        let params = std::sync::Arc::new(Mutex::new(ParamCache::new("/tmp/ados-test-params.json")));
        FcConnection::new(MavlinkConfig::default(), state, params)
    }

    /// A write half that forwards everything written to it down a channel, so a
    /// send cadence can be asserted on the frames it actually put on the wire.
    /// A channel rather than a shared buffer keeps the sync `poll_write` free of
    /// any lock.
    struct CapturingWriter(std::sync::mpsc::Sender<Vec<u8>>);

    impl AsyncWrite for CapturingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let _ = self.0.send(data.to_vec());
            Poll::Ready(Ok(data.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Run one `tick_streams` on an open link and return the MAVLink message ids
    /// it wrote, in order.
    async fn stream_request_message_ids(legacy_stream_request: bool) -> Vec<u32> {
        let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
        let params = std::sync::Arc::new(Mutex::new(ParamCache::new("/tmp/ados-test-params.json")));
        let cfg = MavlinkConfig {
            legacy_stream_request,
            ..MavlinkConfig::default()
        };
        let conn = FcConnection::new(cfg, state, params);
        let (tx, rx) = std::sync::mpsc::channel();
        conn.connected.store(true, Ordering::Relaxed);
        *conn.writer.lock().await = Some(Box::pin(CapturingWriter(tx)));

        conn.tick_streams().await;

        // Drop the writer so the channel closes and the drain terminates.
        *conn.writer.lock().await = None;
        let mut buf: Vec<u8> = rx.into_iter().flatten().collect();
        crate::connection::framing::extract_frames(&mut buf)
            .iter()
            .filter_map(|frame| crate::aux_tee::mavlink_message_id(frame))
            .collect()
    }

    /// `COMMAND_LONG` (the `SET_MESSAGE_INTERVAL` carrier) and the legacy
    /// `REQUEST_DATA_STREAM`.
    const COMMAND_LONG_ID: u32 = 76;
    const REQUEST_DATA_STREAM_ID: u32 = 66;

    #[tokio::test]
    async fn stream_refresh_omits_the_legacy_request_by_default() {
        let ids = stream_request_message_ids(false).await;
        assert_eq!(
            ids.iter().filter(|&&id| id == COMMAND_LONG_ID).count(),
            STREAM_RATES.len(),
            "every per-message interval request must still be sent"
        );
        assert!(
            !ids.contains(&REQUEST_DATA_STREAM_ID),
            "the legacy REQUEST_DATA_STREAM loop must be off by default: measured \
             ingest of 66.5 f/s against the 29 Hz asked for showed ArduPilot honoring \
             both paths, so sending both doubles the telemetry on a shared radio"
        );
    }

    #[tokio::test]
    async fn stream_refresh_sends_the_legacy_request_when_the_flag_is_on() {
        let ids = stream_request_message_ids(true).await;
        assert_eq!(
            ids.iter().filter(|&&id| id == COMMAND_LONG_ID).count(),
            STREAM_RATES.len(),
            "the interval requests are unaffected by the flag"
        );
        assert_eq!(
            ids.iter()
                .filter(|&&id| id == REQUEST_DATA_STREAM_ID)
                .count(),
            STREAM_GROUPS.len(),
            "the rollback for a firmware that honors only REQUEST_DATA_STREAM must \
             send every legacy group"
        );
    }

    #[tokio::test]
    async fn write_failure_does_not_latch_disconnected_and_signals_reconnect() {
        let conn = test_connection();

        // Simulate a live link with a writer that will fail on the next write.
        conn.connected.store(true, Ordering::Relaxed);
        *conn.writer.lock().await = Some(Box::pin(FailingWriter));

        conn.send_bytes(b"\xfd\x00").await;

        // The failing writer is dropped so the run loop reinstalls a fresh one.
        assert!(
            conn.writer.lock().await.is_none(),
            "writer must be cleared after a write failure"
        );

        // The write path must NOT declare the FC permanently disconnected; the
        // run loop owns the transport-open flag and clears it only on a real
        // teardown.
        assert!(
            conn.transport_open(),
            "send_bytes must not latch the transport closed on a transient write error"
        );

        // The reconnect signal must have been raised so run() rebuilds the link.
        // notify_one() leaves a permit, so notified() resolves immediately.
        let signalled = tokio::time::timeout(Duration::from_millis(100), conn.reconnect.notified())
            .await
            .is_ok();
        assert!(signalled, "a write failure must signal a reconnect");
    }

    #[tokio::test]
    async fn send_bytes_is_a_noop_when_no_writer() {
        let conn = test_connection();
        // No writer installed: send_bytes does nothing and raises no reconnect.
        conn.send_bytes(b"\xfd\x00").await;
        let signalled = tokio::time::timeout(Duration::from_millis(50), conn.reconnect.notified())
            .await
            .is_ok();
        assert!(
            !signalled,
            "no writer means nothing to fail and no reconnect to raise"
        );
    }

    /// Backdate `param_last_request` past `PARAM_RATE_LIMIT` without mocking
    /// the clock (this crate's tokio features don't include `test-util`) —
    /// `Instant::elapsed()` reads real wall-clock time regardless, so a
    /// stored `Instant` in the past makes the rate-limit check `due` exactly
    /// as if real time had elapsed.
    fn long_ago() -> Instant {
        Instant::now()
            .checked_sub(PARAM_RATE_LIMIT + Duration::from_secs(1))
            .expect("PARAM_RATE_LIMIT + 1s must not underflow Instant::now()")
    }

    #[tokio::test]
    async fn sweep_defers_while_a_clients_own_download_is_making_progress() {
        let conn = test_connection();
        conn.connected.store(true, Ordering::Relaxed);
        conn.state.lock().await.param_count = 100;

        // A sweep already fired once, and PARAM_RATE_LIMIT has fully elapsed
        // since — a naive time-only gate would fire again here. But the cache
        // has grown since the last check, via a downstream client's own
        // PARAM_REQUEST_LIST landing real, ongoing progress.
        let backdated = long_ago();
        *conn.param_last_request.lock().await = Some(backdated);
        conn.param_priming.store(true, Ordering::Relaxed);
        for i in 0..40 {
            conn.params.lock().await.set(&format!("P{i}"), i as f64, 9);
        }

        conn.tick_param_sweep().await;

        assert_eq!(
            *conn.param_last_request.lock().await,
            Some(backdated),
            "a growing cache must not be interrupted by the sweep's own competing \
             PARAM_REQUEST_LIST — the FC tracks queued-parameter state per link, so a \
             second request would restart its enumeration from index 0"
        );
    }

    #[tokio::test]
    async fn sweep_fires_again_once_genuinely_stalled() {
        let conn = test_connection();
        conn.connected.store(true, Ordering::Relaxed);
        conn.state.lock().await.param_count = 100;

        let backdated = long_ago();
        *conn.param_last_request.lock().await = Some(backdated);
        conn.param_priming.store(true, Ordering::Relaxed);
        // No cache growth at all since the last check — genuinely stalled.

        conn.tick_param_sweep().await;

        let after = *conn.param_last_request.lock().await;
        assert!(
            after.is_some_and(|t| t != backdated),
            "a genuinely stalled sweep must still retry once the rate limit elapses"
        );
    }

    #[test]
    fn request_data_stream_serializes_and_round_trips() {
        // The legacy stream request must be a real ardupilotmega variant and must
        // round-trip through the same v2 codec the send path uses.
        let msg = MavMessage::REQUEST_DATA_STREAM(REQUEST_DATA_STREAM_DATA {
            target_system: 1,
            target_component: 1,
            req_stream_id: 6, // POSITION
            req_message_rate: 5,
            start_stop: 1,
        });
        let bytes = mavlink::serialize_v2(
            MavHeader {
                system_id: 191,
                component_id: 1,
                sequence: 0,
            },
            &msg,
        )
        .expect("REQUEST_DATA_STREAM serializes");
        let (_h, parsed) = mavlink::parse_v2(&bytes).expect("round-trips");
        match parsed {
            MavMessage::REQUEST_DATA_STREAM(d) => {
                assert_eq!(d.req_stream_id, 6);
                assert_eq!(d.req_message_rate, 5);
                assert_eq!(d.start_stop, 1);
            }
            other => panic!("expected REQUEST_DATA_STREAM, got {other:?}"),
        }
    }

    fn command_long_bytes() -> Vec<u8> {
        let msg = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
            target_system: 1,
            target_component: 1,
            command: ados_protocol::mavlink::ardupilotmega::MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            confirmation: 0,
            param1: 1.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            param5: 0.0,
            param6: 0.0,
            param7: 0.0,
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

    fn param_request_list_bytes() -> Vec<u8> {
        let msg = MavMessage::PARAM_REQUEST_LIST(PARAM_REQUEST_LIST_DATA {
            target_system: 1,
            target_component: 1,
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

    /// Spawns a real aux-uplink sender against a loopback listener, the same
    /// setup `aux_uplink`'s own tests use, so `send_client_bytes`'s fallback
    /// path is exercised end to end rather than mocked.
    async fn test_uplink() -> (crate::aux_uplink::AuxUplinkSender, tokio::net::UdpSocket) {
        let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (crate::aux_uplink::spawn(port), listener)
    }

    /// A linked ground agent holds full authority over the node it relays, so
    /// every frame a connected client sends reaches the uplink. This used to be
    /// filtered by a message-id denylist that covered only this MAVLink lane
    /// while the relay's HTTP lane carried the same vehicle commands ungated,
    /// so it filtered one door and left the other open. Authority now belongs
    /// to the relay's authentication boundary.
    #[tokio::test]
    async fn an_anonymous_client_still_reaches_the_uplink_but_is_recorded() {
        // The amplifying case. With no local vehicle these bytes leave over the
        // radio to one that may be flying, so the reach is longer than the
        // local-controller case: it needs no proximity to the aircraft.
        //
        // Still delivered, deliberately. A ground station relaying a linked
        // drone is exactly the shape that takes this fallback, and these ports
        // are advertised for third-party ground stations, so refusing here
        // would break the workflow at the operator rather than at install. The
        // provenance is what changes.
        let conn = test_connection();
        let (sender, listener) = test_uplink().await;
        conn.set_aux_uplink(sender).await;

        let frame = command_long_bytes();
        conn.send_client_bytes(&frame, ClientOrigin::Unauthenticated, None)
            .await;

        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf))
            .await
            .expect("an anonymous client's frame still reaches the uplink today")
            .unwrap();
        let (_, payload) = ados_protocol::aux_mux::decode(&buf[..n]).unwrap();
        assert_eq!(payload, frame, "the exact bytes still cross");
    }

    #[test]
    fn the_origins_are_distinguishable() {
        // If any two of these compare equal the recording is silently useless,
        // which is worse than not recording at all.
        assert_ne!(ClientOrigin::Trusted, ClientOrigin::Unauthenticated);
        assert_ne!(ClientOrigin::Trusted, ClientOrigin::Relayed);
        assert_ne!(ClientOrigin::Relayed, ClientOrigin::Unauthenticated);
    }

    /// The on-box socket's provenance was a constant, so a writer forwarding
    /// broker traffic and a writer producing its own bytes recorded the same
    /// thing. The first must not read as the second.
    #[test]
    fn a_forwarding_writer_is_not_read_as_a_local_producer() {
        use ados_protocol::ipc::IpcPeer;

        let local = IpcPeer {
            uid: Some(0),
            gid: Some(0),
            pid: Some(1234),
            off_box_source: false,
            injector: None,
        };
        assert_eq!(ClientOrigin::from_ipc_peer(&local), ClientOrigin::Trusted);

        let forwarder = IpcPeer {
            off_box_source: true,
            injector: None,
            ..local
        };
        assert_ne!(
            ClientOrigin::from_ipc_peer(&forwarder),
            ClientOrigin::Trusted,
            "bytes that reached the node over a broker must not be recorded as \
             produced on it"
        );
        assert_eq!(
            ClientOrigin::from_ipc_peer(&forwarder),
            ClientOrigin::Relayed
        );
    }

    /// The raw MSP byte lane called the plain FC writer, so it recorded no
    /// provenance at all and could not have told a broker-sourced command from
    /// a local one. It must also never reach the aux uplink: that lane frames
    /// MAVLink, and MSP bytes put on it are not carried, they are corrupt.
    #[tokio::test]
    async fn msp_client_bytes_never_reach_the_aux_uplink() {
        let conn = test_connection();
        let (sender, listener) = test_uplink().await;
        conn.set_aux_uplink(sender).await;

        conn.send_client_raw(b"$M<\x00\x64\x64", ClientOrigin::Relayed, None)
            .await;

        let mut buf = [0u8; 256];
        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.recv_from(&mut buf))
                .await
                .is_err(),
            "MSP bytes must not be radiated on the MAVLink aux lane"
        );
    }

    #[tokio::test]
    async fn a_command_frame_reaches_the_uplink() {
        let conn = test_connection();
        let (sender, listener) = test_uplink().await;
        conn.set_aux_uplink(sender).await;

        let frame = command_long_bytes();
        conn.send_client_bytes(&frame, ClientOrigin::Trusted, None)
            .await;

        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf))
            .await
            .expect("a command frame must reach the aux uplink")
            .unwrap();
        let (_, payload) = ados_protocol::aux_mux::decode(&buf[..n]).unwrap();
        assert_eq!(payload, frame, "the exact bytes must reach the uplink");
    }

    #[tokio::test]
    async fn a_param_frame_reaches_the_uplink() {
        let conn = test_connection();
        let (sender, listener) = test_uplink().await;
        conn.set_aux_uplink(sender).await;

        let frame = param_request_list_bytes();
        conn.send_client_bytes(&frame, ClientOrigin::Trusted, None)
            .await;

        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf))
            .await
            .expect("a param frame must reach the aux uplink")
            .unwrap();
        let (_, payload) = ados_protocol::aux_mux::decode(&buf[..n]).unwrap();
        assert_eq!(payload, frame);
    }

    /// A raw TCP read is not frame-aligned, so one call can carry several
    /// frames concatenated. Nothing splits them any more, so the buffer must
    /// cross whole and byte-identical rather than being reassembled or dropped.
    #[tokio::test]
    async fn a_concatenated_buffer_crosses_whole() {
        let conn = test_connection();
        let (sender, listener) = test_uplink().await;
        conn.set_aux_uplink(sender).await;

        let mut concatenated = param_request_list_bytes();
        concatenated.extend_from_slice(&command_long_bytes());

        conn.send_client_bytes(&concatenated, ClientOrigin::Trusted, None)
            .await;

        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf))
            .await
            .expect("the concatenated buffer must reach the aux uplink")
            .unwrap();
        let (_, payload) = ados_protocol::aux_mux::decode(&buf[..n]).unwrap();
        assert_eq!(
            payload, concatenated,
            "the buffer must cross whole, not be split or filtered"
        );
    }
}
