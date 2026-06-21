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

/// Legacy data-stream groups requested via `REQUEST_DATA_STREAM`, alongside the
/// modern `SET_MESSAGE_INTERVAL` above: `(MAV_DATA_STREAM id, Hz)`. Some
/// firmwares (iNav, older ArduPilot, Betaflight's MAVLink telemetry) honor only
/// this legacy mechanism and ignore `SET_MESSAGE_INTERVAL`; ArduPilot 4.1+ does
/// the reverse. The two requests are therefore mutually exclusive per firmware
/// and never double-rate the same message. `MAV_DATA_STREAM_ALL` (id 0) is
/// deliberately omitted: it would overlap the specific groups and double-rate on
/// any firmware that honored both.
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

    async fn send_msg(&self, msg: &MavMessage) -> bool {
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
        // Belt-and-suspenders for firmwares that honor only the legacy
        // REQUEST_DATA_STREAM mechanism (iNav / older ArduPilot / Betaflight).
        // Harmless on ArduPilot 4.1+, which ignores it in favor of the interval
        // requests above; see STREAM_GROUPS.
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
}
