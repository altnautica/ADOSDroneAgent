//! The FC send paths and the three periodic send cadences.
//!
//! Splits the writer-side of [`FcConnection`] out of the connect/read FSM: the
//! raw byte/message send primitives, the 1 Hz companion heartbeat, the adaptive
//! stream-interval requests, and the rate-limited parameter sweep. Each is a
//! method on `FcConnection` using its interior mutability, so the run loop and
//! the periodic sender tasks share one connection.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

// `REQUEST_DATA_STREAM_DATA` is marked superseded by MAV_CMD_SET_MESSAGE_INTERVAL
// in the dialect, and it is deliberately retained here: see the comment on the
// `legacy_stream_request` branch in `tick_streams` — it is the only stream-rate
// mechanism iNav, Betaflight and pre-4.1 ArduPilot honor.
#[allow(deprecated)]
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
/// How often an unidentified ArduPilot is asked for its firmware banner.
const BANNER_RETRY: Duration = Duration::from_secs(3);

/// Per-message stream rates requested from the FC: `(MAVLink message id, Hz)`.
const BASE_RATES: &[(u32, f32)] = &[
    (0, 1.0),   // HEARTBEAT
    (30, 10.0), // ATTITUDE
    (33, 5.0),  // GLOBAL_POSITION_INT
    (1, 2.0),   // SYS_STATUS
    (24, 2.0),  // GPS_RAW_INT
    (74, 4.0),  // VFR_HUD
    (147, 1.0), // BATTERY_STATUS
    (65, 4.0),  // RC_CHANNELS
];

/// [`BASE_RATES`] plus the sensor streams a capture session records: the
/// inertial, attitude, local position, GNSS, clock and pressure streams at
/// rates a visual-inertial pipeline can use.
const CAPTURE_RATES: &[(u32, f32)] = &[
    (0, 1.0),     // HEARTBEAT
    (30, 10.0),   // ATTITUDE
    (33, 5.0),    // GLOBAL_POSITION_INT
    (1, 2.0),     // SYS_STATUS
    (24, 10.0),   // GPS_RAW_INT
    (74, 4.0),    // VFR_HUD
    (147, 1.0),   // BATTERY_STATUS
    (65, 4.0),    // RC_CHANNELS
    (105, 200.0), // HIGHRES_IMU
    (31, 100.0),  // ATTITUDE_QUATERNION
    (32, 50.0),   // LOCAL_POSITION_NED
    (2, 1.0),     // SYSTEM_TIME
    (29, 10.0),   // SCALED_PRESSURE
];

/// The stream table for a node of `profile`: a visor always records sensor
/// streams, a drone only while a capture session is active (`capture_active`),
/// every other profile never.
pub(crate) fn stream_rates(profile: &str, capture_active: bool) -> &'static [(u32, f32)] {
    if profile == "visor" || (profile == "drone" && capture_active) {
        CAPTURE_RATES
    } else {
        BASE_RATES
    }
}

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

/// Wall bound applied to every write to the flight controller.
///
/// No write may hold the writer mutex indefinitely. The mutex is the one choke
/// point every FC-bound byte passes through, including the unconditional 1 Hz
/// companion heartbeat, so a single unbounded write starves that heartbeat for
/// as long as the kernel takes to drain it — at 57600 baud a 64 KB blob from an
/// IPC client is roughly eleven seconds, long enough for the autopilot to trip
/// its own GCS failsafe while every surface on this node still reads connected.
const FC_WRITE_BUDGET: Duration = Duration::from_millis(500);

/// Largest single write admitted to the flight-controller writer.
///
/// A MAVLink v2 frame is at most 280 bytes and the batching producers cap a
/// concatenation at [`ados_protocol::aux_mux::AUX_MAX_PAYLOAD`] (1200), while
/// the IPC MAVLink socket will hand over anything up to the 64 KB frame cap.
/// Refusing an oversized blob BEFORE the lock is taken is strictly better than
/// starting it and hitting [`FC_WRITE_BUDGET`] partway, which costs a link
/// reconnect for one malformed client write. 4 KiB is comfortably above every
/// legitimate producer and is ~0.7 s of wire time on the slowest supported
/// link.
pub(crate) const FC_MAX_SINGLE_WRITE: usize = 4096;

/// How long the flight-controller TX byte counter may stay flat, on a link with
/// a writer installed, before the writer is declared dead.
///
/// This is the delta-counter half of the liveness judgement, and it exists
/// because "the writer mutex is held" and "the future has not returned" are
/// necessary but never sufficient evidence that bytes are moving. The demand
/// side needs no guessing: the 1 Hz companion heartbeat is unconditional
/// whenever a writer exists, so a healthy link advances this counter at least
/// once per second. Four seconds is four consecutive missed heartbeats — a
/// margin no scheduling jitter explains — and is shorter than the autopilot's
/// own GCS-failsafe windows, so the link is rebuilt before the vehicle reacts.
const FC_TX_STALL: Duration = Duration::from_secs(4);

/// The writer-liveness reference sample: the TX byte count last observed and
/// when it last actually moved.
#[derive(Debug)]
pub(super) struct TxLiveness {
    last_bytes: u64,
    last_progress_at: Instant,
}

impl TxLiveness {
    pub(super) fn new() -> Self {
        Self {
            last_bytes: 0,
            last_progress_at: Instant::now(),
        }
    }

    /// Re-arm the reference so the next window is judged from `at` onward. Used
    /// when a fresh writer is installed or when nothing is expected to move.
    fn rearm(&mut self, bytes: u64, at: Instant) {
        self.last_bytes = bytes;
        self.last_progress_at = at;
    }
}

/// Whether the writer must be torn down: `bytes` has not advanced for at least
/// `stall` since it last did. Advancing the counter re-arms the window.
///
/// Pure so the window itself is testable without a serial port.
fn tx_is_stalled(bytes: u64, state: &mut TxLiveness, at: Instant, stall: Duration) -> bool {
    if bytes != state.last_bytes {
        state.rearm(bytes, at);
        return false;
    }
    at.saturating_duration_since(state.last_progress_at) >= stall
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
    /// Returns whether the frame was written in full.
    ///
    /// Bounded by [`FC_WRITE_BUDGET`], so no caller can hold the writer mutex —
    /// and therefore block the companion heartbeat — for an unbounded time.
    /// There is deliberately no unbounded write path left on this type.
    pub async fn send_bytes(&self, data: &[u8]) -> bool {
        self.send_bytes_bounded(data, FC_WRITE_BUDGET).await
    }

    /// Write raw bytes with a wall bound, treating a timeout as a broken link.
    ///
    /// The bound is applied here, inside the writer lock, rather than by
    /// wrapping this call in `tokio::time::timeout`. Cancelling the write future
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
        // Refused before the lock is taken: an oversized blob must not get the
        // chance to occupy the writer at all, and abandoning it partway would
        // force a reconnect for what is one bad client write.
        if data.len() > FC_MAX_SINGLE_WRITE {
            tracing::warn!(
                len = data.len(),
                cap = FC_MAX_SINGLE_WRITE,
                "fc_write_refused_oversize"
            );
            return false;
        }
        let mut guard = self.writer.lock().await;
        let Some(w) = guard.as_mut() else {
            return false;
        };
        match tokio::time::timeout(budget, write_then_flush(w, data)).await {
            Ok(Ok(())) => {
                // The delta the TX watchdog reads. Bumped only on a completed
                // write-and-flush, so the counter means bytes handed to the
                // transport rather than bytes a caller offered.
                self.tx_bytes
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                true
            }
            Ok(Err(e)) => {
                // The generation is read under the writer lock, so it names
                // the link this write actually failed on.
                let failed_gen = self.link_gen();
                *guard = None;
                drop(guard);
                tracing::warn!(error = %e, "fc_write_failed");
                self.request_reconnect(failed_gen);
                false
            }
            Err(_) => {
                // The stream may carry a partial frame. Re-open rather than
                // write the next frame onto it.
                let failed_gen = self.link_gen();
                *guard = None;
                drop(guard);
                tracing::warn!(
                    budget_ms = budget.as_millis() as u64,
                    "fc_write_timed_out_reconnecting"
                );
                self.request_reconnect(failed_gen);
                false
            }
        }
    }

    /// Monotonic count of bytes this process has successfully written to the
    /// flight controller since start. The quantity the TX watchdog takes deltas
    /// of; also the honest answer to "is the host->FC direction moving".
    pub fn tx_bytes(&self) -> u64 {
        self.tx_bytes.load(Ordering::Relaxed)
    }

    /// One tick of the FC writer's TX-liveness watchdog. Call at ~1 Hz.
    ///
    /// A writer that exists and whose future has not returned is NOT evidence
    /// that bytes are reaching the flight controller: a wedged USB CDC-ACM node
    /// accepts writes into a driver buffer that never drains, and a caller
    /// blocked forever on the writer mutex leaves every send path silently
    /// stalled while the transport still reads open. This asserts the delta
    /// instead — [`Self::tx_bytes`] must advance, which the unconditional 1 Hz
    /// companion heartbeat guarantees on any live link — and tears the writer
    /// down when it does not, so the run loop rebuilds the link.
    pub async fn tick_tx_watchdog(&self) {
        let now = Instant::now();
        let bytes = self.tx_bytes.load(Ordering::Relaxed);

        // Nothing is expected to move with no writer installed: the link is
        // down, or this is the deliberately command-down-gated MAVLink-over-ELRS
        // lane, which installs no writer by design. Re-arm so a writer that
        // appears later is judged over a full window rather than inheriting a
        // stale one.
        if !self.transport_open() || self.writer.lock().await.is_none() {
            self.tx_liveness.lock().await.rearm(bytes, now);
            return;
        }

        let stalled = {
            let mut state = self.tx_liveness.lock().await;
            tx_is_stalled(bytes, &mut state, now, FC_TX_STALL)
        };
        if !stalled {
            return;
        }

        let failed_gen = {
            let mut writer = self.writer.lock().await;
            *writer = None;
            self.link_gen()
        };
        tracing::warn!(
            stall_ms = FC_TX_STALL.as_millis() as u64,
            tx_bytes = bytes,
            "fc_tx_stalled_reconnecting"
        );
        self.request_reconnect(failed_gen);
        // Re-arm so the replacement writer gets its own full window instead of
        // being torn down again on the next tick.
        self.tx_liveness.lock().await.rearm(bytes, now);
    }

    /// Serialise `msg` with this node's identity and write it to the FC,
    /// signed with link id 2 when the agent holds a signing key.
    /// Returns whether it reached the flight controller: `false` for a frame
    /// that could not be serialised, no installed writer, or a failed write.
    pub(super) async fn send_msg(&self, msg: &MavMessage) -> bool {
        match mavlink::serialize_v2(self.our_header(), msg) {
            Ok(bytes) => self.send_bytes(&self.agent_signing().sign(bytes)).await,
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
}

mod client;
mod sensor_sends;
mod ticks;

#[cfg(test)]
mod stream_rate_tests;
#[cfg(test)]
mod tests;
