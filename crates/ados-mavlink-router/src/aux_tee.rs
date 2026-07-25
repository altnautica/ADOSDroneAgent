//! Tee the flight controller's MAVLink onto the radio's auxiliary lane.
//!
//! A drone linked to a ground station over the radio must deliver its flight
//! controller's MAVLink to that ground station, so an operator who paired only
//! the ground station still sees the vehicle. Nothing carried it: the video lane
//! is video, and the control lane admits two fixed-size frames and hard-drops
//! the rest. This module copies each FC frame onto the auxiliary application
//! stream, tagged as the MAVLink channel, for the ground station to decode and
//! republish.
//!
//! ## Scope: this is the downlink only
//!
//! Frames travel vehicle to ground here and nowhere else. Commands from the
//! ground back to the flight controller need the mirror of this path — a
//! ground-side sender and a drone-side aux receiver writing into the FC command
//! socket — and none of that exists yet. So this module makes the vehicle
//! visible over the radio; it does not by itself make it commandable, and no
//! surface should be read as saying otherwise.
//!
//! ## Why this crate
//!
//! This crate already owns every MAVLink frame: it holds the FC link and fans
//! the frames out to the IPC socket and the direct-GCS proxies. Teeing here
//! subscribes to that same in-process fan-out, which means no second process, no
//! extra socket hop, and — the point — no second frame reader. A tee living
//! anywhere else would have to connect back to `mavlink.sock` and re-decode the
//! length-prefixed stream to recover the frames this crate already has in hand.
//!
//! ## Why the radio can never starve behind it
//!
//! The aux lane shares one radio with video, and video is the lane that matters.
//! Three properties keep this tee incapable of harming it:
//!
//! * **A hard byte budget.** A token bucket caps the tee's throughput. It is not
//!   a target rate, it is a ceiling: frames that do not fit the budget are
//!   dropped, never queued for later.
//! * **Nothing here can apply backpressure.** The frame source is a broadcast
//!   receiver, which drops for a slow consumer rather than blocking the
//!   producer. If the aux socket stalls, this task falls behind and the runtime
//!   discards the frames it missed — the FC read loop, the IPC socket, and the
//!   proxies are untouched. The number skipped is counted, not guessed at.
//! * **Bounded, self-limiting recovery.** A lane that will not open backs off
//!   exponentially instead of hammering the radio's command socket.
//!
//! ## Which frames are shed first
//!
//! Under budget pressure the shed order is deliberate. A small allow list of
//! message ids ([`CRITICAL_MESSAGE_IDS`]) is what an operator needs to fly and
//! understand the vehicle, plus the transfer traffic that would otherwise stall
//! a mission or parameter download halfway. Everything else — the high-rate
//! sensor and servo streams, and any frame whose id cannot be read — is bulk,
//! and bulk may only draw tokens above a reserve line. So a saturated lane sheds
//! the noisy streams and keeps the vehicle legible. The allow list is a policy,
//! not a law of the protocol: it is meant to be revised against what real links
//! actually shed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::aux_egress::{AuxEgress, AuxEgressError};
use ados_protocol::aux_mux::{AuxChannel, AUX_HEADER_LEN, AUX_MAX_PAYLOAD};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, Notify};

/// MAVLink v2 start-of-frame magic.
const STX_V2: u8 = 0xFD;
/// MAVLink v1 start-of-frame magic.
const STX_V1: u8 = 0xFE;

/// Sustained ceiling on what the tee may put on the aux lane, in payload bytes
/// per second.
///
/// Sized to be generous for MAVLink telemetry (a typical stream set runs a few
/// kilobytes per second) while staying a small fraction of the radio. The aux
/// pair defaults to a doubling forward-error-correction ratio, so this ceiling
/// costs roughly twice as many bytes on air, which is still far below a video
/// stream's budget.
pub const DEFAULT_BUDGET_BYTES_PER_SEC: f64 = 16_384.0;

/// Burst allowance. Lets a short spike (a mode change, a parameter response
/// burst) through without shedding, while the sustained rate stays capped.
pub const DEFAULT_BURST_BYTES: f64 = 32_768.0;

/// Bytes of the bucket that only critical frames may draw from.
///
/// Bulk traffic stops at this line, so a saturating stream cannot consume the
/// budget that heartbeat, position, status text and command acknowledgements
/// need. Half the burst leaves bulk a working share on an idle link.
pub const DEFAULT_BULK_RESERVE_BYTES: f64 = 16_384.0;

/// First retry delay after a failed attempt to open the aux lane.
const OPEN_BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Ceiling on the retry delay, so a radio that is down for a long time is
/// probed occasionally rather than continuously.
const OPEN_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Re-check interval once the operator dead switch is found off.
///
/// The refusal is reported once and then the tee idles. It still re-checks this
/// slowly so that enabling the lane and restarting the radio recovers on its
/// own, without an operator having to remember to restart this service too.
const DISABLED_RECHECK: Duration = Duration::from_secs(300);

/// How often the counters are logged (only when something changed).
const COUNTER_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Message ids kept when the lane is congested, sorted by id.
///
/// Two groups: the state an operator needs to fly and understand the vehicle,
/// and the request/response traffic of a mission or parameter transfer, which is
/// bursty but would leave the operator with a half-downloaded mission if it were
/// shed partway.
pub const CRITICAL_MESSAGE_IDS: &[u32] = &[
    0,   // HEARTBEAT
    1,   // SYS_STATUS
    22,  // PARAM_VALUE
    24,  // GPS_RAW_INT
    30,  // ATTITUDE
    33,  // GLOBAL_POSITION_INT
    39,  // MISSION_ITEM
    40,  // MISSION_REQUEST
    42,  // MISSION_CURRENT
    44,  // MISSION_COUNT
    46,  // MISSION_ITEM_REACHED
    47,  // MISSION_ACK
    51,  // MISSION_REQUEST_INT
    62,  // NAV_CONTROLLER_OUTPUT
    73,  // MISSION_ITEM_INT
    74,  // VFR_HUD
    77,  // COMMAND_ACK
    147, // BATTERY_STATUS
    148, // AUTOPILOT_VERSION
    193, // EKF_STATUS_REPORT
    242, // HOME_POSITION
    245, // EXTENDED_SYS_STATE
    253, // STATUSTEXT
];

/// How readily a frame is shed when the lane is congested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Kept while any budget remains.
    Critical,
    /// Shed first: may only draw tokens above the reserve line.
    Bulk,
}

/// Read the message id out of a raw MAVLink frame without decoding it.
///
/// Deliberately a header read rather than a full parse: it costs nothing per
/// frame and, more importantly, it works for messages this build's dialect does
/// not know. A full parse would fail on an unknown message and misreport it,
/// which on a shed decision means silently mis-prioritising real traffic.
///
/// Returns `None` for anything that is not a recognisable frame header.
pub fn mavlink_message_id(frame: &[u8]) -> Option<u32> {
    match frame.first().copied()? {
        // v2: magic, length, incompat, compat, sequence, system, component,
        // then a 24-bit little-endian message id at bytes 7 through 9.
        STX_V2 if frame.len() >= 10 => {
            Some(u32::from(frame[7]) | (u32::from(frame[8]) << 8) | (u32::from(frame[9]) << 16))
        }
        // v1: magic, length, sequence, system, component, then a 1-byte id.
        STX_V1 if frame.len() >= 6 => Some(u32::from(frame[5])),
        _ => None,
    }
}

/// Classify a frame for shedding. An unreadable header is bulk: an
/// unrecognisable frame is the last thing worth spending a congested lane on.
pub fn frame_priority(frame: &[u8]) -> Priority {
    match mavlink_message_id(frame) {
        Some(id) if CRITICAL_MESSAGE_IDS.contains(&id) => Priority::Critical,
        _ => Priority::Bulk,
    }
}

/// The tee's throughput bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShaperConfig {
    /// Sustained ceiling in bytes per second.
    pub budget_bytes_per_sec: f64,
    /// Bucket size, so a short burst is not shed.
    pub burst_bytes: f64,
    /// Bucket floor that only critical frames may draw below.
    pub bulk_reserve_bytes: f64,
}

impl Default for ShaperConfig {
    fn default() -> Self {
        Self {
            budget_bytes_per_sec: DEFAULT_BUDGET_BYTES_PER_SEC,
            burst_bytes: DEFAULT_BURST_BYTES,
            bulk_reserve_bytes: DEFAULT_BULK_RESERVE_BYTES,
        }
    }
}

/// A token bucket over bytes with a reserve that only critical frames may use.
///
/// The bucket is the hard cap and applies to every frame regardless of priority.
/// Priority decides only who is shed first: bulk must leave the bottom
/// `bulk_reserve_bytes` untouched, so a stream that saturates the lane runs the
/// bucket down to the reserve and stops there, and the refill from that point on
/// is available to critical frames.
#[derive(Debug)]
pub struct RateShaper {
    cfg: ShaperConfig,
    tokens: f64,
    last: Instant,
}

impl RateShaper {
    /// A shaper starting with a full bucket, as of now.
    pub fn new(cfg: ShaperConfig) -> Self {
        Self::starting_at(cfg, Instant::now())
    }

    /// A shaper starting with a full bucket at an explicit instant, so a test
    /// can drive time by hand instead of sleeping.
    pub fn starting_at(cfg: ShaperConfig, now: Instant) -> Self {
        let cfg = ShaperConfig {
            budget_bytes_per_sec: cfg.budget_bytes_per_sec.max(0.0),
            burst_bytes: cfg.burst_bytes.max(0.0),
            // A reserve above the bucket size would starve bulk outright; a
            // negative one would be meaningless. Neither is reachable from the
            // shipped defaults, so this is only defensive.
            bulk_reserve_bytes: cfg.bulk_reserve_bytes.clamp(0.0, cfg.burst_bytes.max(0.0)),
        };
        Self {
            tokens: cfg.burst_bytes,
            cfg,
            last: now,
        }
    }

    /// Tokens currently available.
    pub fn tokens(&self) -> f64 {
        self.tokens
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed <= 0.0 {
            return;
        }
        self.last = now;
        self.tokens =
            (self.tokens + elapsed * self.cfg.budget_bytes_per_sec).min(self.cfg.burst_bytes);
    }

    /// Whether a frame of `cost` on-air bytes may be sent now, spending its
    /// tokens if so. A refusal spends nothing.
    pub fn admit(&mut self, now: Instant, cost: usize, priority: Priority) -> bool {
        self.refill(now);
        let floor = match priority {
            Priority::Critical => 0.0,
            Priority::Bulk => self.cfg.bulk_reserve_bytes,
        };
        let cost = cost as f64;
        if self.tokens - cost >= floor {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}

/// Live tee counters.
///
/// Every frame that enters the tee leaves through exactly one of these, so the
/// tally accounts for the whole stream rather than only its successes. That is
/// the difference between knowing the lane works and assuming it does.
#[derive(Debug, Default)]
pub struct TeeCounters {
    /// Datagrams handed to the kernel. Not proof the radio radiated them.
    pub frames_forwarded: AtomicU64,
    /// On-air bytes those datagrams accounted for, framing included.
    pub bytes_forwarded: AtomicU64,
    /// Frames too large for one aux frame. Dropped whole, never truncated.
    pub dropped_oversize: AtomicU64,
    /// Frames shed because the byte budget was exhausted.
    pub dropped_rate_limit: AtomicU64,
    /// Frames dropped because the aux lane was not open, including while
    /// backing off from a failed open or while the dead switch is off.
    pub dropped_lane_down: AtomicU64,
    /// Frames the runtime discarded because this tee fell behind the fan-out.
    /// A non-zero value here is the tee shedding load, not the router stalling.
    pub dropped_lagged: AtomicU64,
    /// Failed attempts to open the lane. A refusal by the operator dead switch
    /// is not counted here: it is a deliberate state, not a fault.
    pub open_errors: AtomicU64,
    /// Datagrams the kernel refused.
    pub send_errors: AtomicU64,
}

impl TeeCounters {
    /// A consistent-enough point-in-time copy for logging and status surfaces.
    pub fn snapshot(&self) -> TeeCountersSnapshot {
        TeeCountersSnapshot {
            frames_forwarded: self.frames_forwarded.load(Ordering::Relaxed),
            bytes_forwarded: self.bytes_forwarded.load(Ordering::Relaxed),
            dropped_oversize: self.dropped_oversize.load(Ordering::Relaxed),
            dropped_rate_limit: self.dropped_rate_limit.load(Ordering::Relaxed),
            dropped_lane_down: self.dropped_lane_down.load(Ordering::Relaxed),
            dropped_lagged: self.dropped_lagged.load(Ordering::Relaxed),
            open_errors: self.open_errors.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
        }
    }
}

/// A plain copy of [`TeeCounters`], for logs and the state snapshot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TeeCountersSnapshot {
    pub frames_forwarded: u64,
    pub bytes_forwarded: u64,
    pub dropped_oversize: u64,
    pub dropped_rate_limit: u64,
    pub dropped_lane_down: u64,
    pub dropped_lagged: u64,
    pub open_errors: u64,
    pub send_errors: u64,
}

/// Retry gate for opening the aux lane.
///
/// Holds the backoff so a radio that is not up (or a lane the operator has
/// forbidden) is retried on a schedule rather than on every single frame, which
/// at telemetry rates would be a command-socket connection storm.
#[derive(Debug)]
struct LaneGate {
    /// When the next open may be attempted. `None` means immediately.
    next_attempt: Option<Instant>,
    backoff: Duration,
    /// Whether the current outage has already been logged, so a long outage
    /// produces one line rather than one per retry.
    outage_logged: bool,
}

impl Default for LaneGate {
    fn default() -> Self {
        Self {
            next_attempt: None,
            backoff: OPEN_BACKOFF_MIN,
            outage_logged: false,
        }
    }
}

impl LaneGate {
    fn may_attempt(&self, now: Instant) -> bool {
        self.next_attempt.is_none_or(|at| now >= at)
    }

    /// The lane came up. Only ever called on a transition, because an open lane
    /// short-circuits before the gate is consulted.
    fn on_open(&mut self) {
        self.next_attempt = None;
        self.backoff = OPEN_BACKOFF_MIN;
        self.outage_logged = false;
        tracing::info!("mavlink_aux_lane_open");
    }

    /// The operator dead switch is off. Reported once, then re-checked slowly.
    fn on_disabled(&mut self, now: Instant) {
        if !self.outage_logged {
            self.outage_logged = true;
            tracing::info!(
                recheck_s = DISABLED_RECHECK.as_secs(),
                "mavlink_aux_lane_disabled"
            );
        }
        self.next_attempt = Some(now + DISABLED_RECHECK);
    }

    /// The lane could not be opened, or a send failed. Backs off exponentially.
    fn on_failure(&mut self, now: Instant, error: &AuxEgressError) {
        if !self.outage_logged {
            self.outage_logged = true;
            tracing::warn!(error = %error, "mavlink_aux_lane_unavailable");
        } else {
            tracing::debug!(error = %error, "mavlink_aux_lane_still_unavailable");
        }
        self.next_attempt = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(OPEN_BACKOFF_MAX);
    }
}

/// Log the counters when they have moved since the last report.
fn report(counters: &TeeCounters, last: TeeCountersSnapshot) -> TeeCountersSnapshot {
    let now = counters.snapshot();
    if now != last {
        tracing::info!(
            frames_forwarded = now.frames_forwarded,
            bytes_forwarded = now.bytes_forwarded,
            dropped_oversize = now.dropped_oversize,
            dropped_rate_limit = now.dropped_rate_limit,
            dropped_lane_down = now.dropped_lane_down,
            dropped_lagged = now.dropped_lagged,
            open_errors = now.open_errors,
            send_errors = now.send_errors,
            "mavlink_aux_tee_counters"
        );
    }
    now
}

/// Run the tee until cancelled or the frame source closes.
///
/// Takes the frame receiver rather than the FC connection so the whole loop is
/// exercisable against a plain broadcast channel and a fake radio, with no
/// flight controller and no radio hardware.
pub async fn run(
    mut frames: broadcast::Receiver<Vec<u8>>,
    egress: AuxEgress,
    counters: Arc<TeeCounters>,
    shaper_config: ShaperConfig,
    cancel: Arc<Notify>,
) {
    let mut shaper = RateShaper::new(shaper_config);
    let mut gate = LaneGate::default();
    let mut last_report = TeeCountersSnapshot::default();
    let mut report_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + COUNTER_REPORT_INTERVAL,
        COUNTER_REPORT_INTERVAL,
    );
    report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(
        budget_bytes_per_sec = shaper_config.budget_bytes_per_sec,
        burst_bytes = shaper_config.burst_bytes,
        "mavlink_aux_tee_started"
    );

    loop {
        let frame = tokio::select! {
            biased;
            _ = cancel.notified() => break,
            _ = report_tick.tick() => {
                last_report = report(&counters, last_report);
                continue;
            }
            received = frames.recv() => match received {
                Ok(frame) => frame,
                // The runtime discarded frames this tee was too slow to take.
                // That is the shed working as designed: the fan-out producer was
                // never blocked. Count what was lost rather than assume none was.
                Err(RecvError::Lagged(skipped)) => {
                    counters.dropped_lagged.fetch_add(skipped, Ordering::Relaxed);
                    continue;
                }
                Err(RecvError::Closed) => break,
            },
        };

        // Reject before spending any budget or opening anything. A frame that
        // cannot be framed whole is dropped: truncating it would put a corrupt
        // frame on air that fails its checksum on the far side and reads as
        // radio damage rather than as our bug.
        if frame.len() > AUX_MAX_PAYLOAD {
            counters.dropped_oversize.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        if !egress.is_open().await {
            let now = Instant::now();
            if !gate.may_attempt(now) {
                counters.dropped_lane_down.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match egress.ensure_open().await {
                Ok(()) => gate.on_open(),
                Err(AuxEgressError::Disabled) => {
                    gate.on_disabled(Instant::now());
                    counters.dropped_lane_down.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(error) => {
                    counters.open_errors.fetch_add(1, Ordering::Relaxed);
                    gate.on_failure(Instant::now(), &error);
                    counters.dropped_lane_down.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
        }

        let cost = AUX_HEADER_LEN + frame.len();
        if !shaper.admit(Instant::now(), cost, frame_priority(&frame)) {
            counters.dropped_rate_limit.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        match egress.send(AuxChannel::Mavlink, &frame).await {
            Ok(()) => {
                counters.frames_forwarded.fetch_add(1, Ordering::Relaxed);
                counters
                    .bytes_forwarded
                    .fetch_add(cost as u64, Ordering::Relaxed);
            }
            Err(error) => {
                counters.send_errors.fetch_add(1, Ordering::Relaxed);
                // The send path already dropped its socket, so the next frame
                // re-opens; back off first so a persistent fault does not turn
                // into an open storm.
                gate.on_failure(Instant::now(), &error);
            }
        }
    }

    report(&counters, last_report);
    tracing::info!("mavlink_aux_tee_stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::aux_mux;
    use std::path::{Path, PathBuf};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UdpSocket, UnixListener};

    /// A minimal MAVLink v2 frame carrying `msg_id` and `payload_len` bytes of
    /// payload. Only the header fields the tee reads need to be real.
    fn v2_frame(msg_id: u32, payload_len: usize) -> Vec<u8> {
        let mut f = vec![
            STX_V2,
            payload_len as u8,
            0, // incompat flags: unsigned
            0, // compat flags
            0, // sequence
            1, // system id
            1, // component id
            (msg_id & 0xFF) as u8,
            ((msg_id >> 8) & 0xFF) as u8,
            ((msg_id >> 16) & 0xFF) as u8,
        ];
        f.extend(std::iter::repeat_n(0xAB, payload_len));
        f.extend_from_slice(&[0, 0]); // checksum
        f
    }

    /// A fake radio command socket. `disabled` makes every open refuse the way
    /// the operator dead switch does; otherwise opens succeed and point at
    /// `tx_port`.
    fn fake_radio(sock_path: PathBuf, tx_port: u16, disabled: bool) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let listener = UnixListener::bind(&sock_path).unwrap();
            while let Ok((stream, _)) = listener.accept().await {
                let (rx, mut tx) = stream.into_split();
                let mut line = String::new();
                let _ = BufReader::new(rx).read_line(&mut line).await;
                let reply = if disabled {
                    "{\"ok\":false,\"error\":\"E_AUX_DISABLED\"}\n".to_string()
                } else {
                    format!(
                        "{{\"ok\":true,\"active\":true,\"tx_port\":{tx_port},\"rx_port\":5603}}\n"
                    )
                };
                let _ = tx.write_all(reply.as_bytes()).await;
            }
        })
    }

    /// Wait until the fake radio's socket exists, so a test never races it.
    async fn await_socket(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("fake radio never bound its socket");
    }

    // ---- message id and shed classification ----

    #[test]
    fn reads_the_message_id_from_both_frame_versions() {
        assert_eq!(mavlink_message_id(&v2_frame(0, 4)), Some(0));
        assert_eq!(mavlink_message_id(&v2_frame(33, 8)), Some(33));
        // A three-byte id exercises all of the little-endian span.
        assert_eq!(mavlink_message_id(&v2_frame(0x0A_BC_DE, 2)), Some(0x0ABCDE));
        // v1: magic, length, sequence, system, component, id.
        let v1 = vec![STX_V1, 1, 0, 1, 1, 30, 0xAB, 0, 0];
        assert_eq!(mavlink_message_id(&v1), Some(30));
    }

    #[test]
    fn an_unreadable_header_has_no_message_id_and_is_shed_first() {
        assert_eq!(mavlink_message_id(&[]), None);
        assert_eq!(mavlink_message_id(b"not a frame"), None);
        // Truncated below the header: refuse rather than read past the end.
        assert_eq!(mavlink_message_id(&[STX_V2, 1, 0]), None);
        assert_eq!(mavlink_message_id(&[STX_V1, 1]), None);
        assert_eq!(frame_priority(b"not a frame"), Priority::Bulk);
    }

    #[test]
    fn flight_critical_frames_outrank_the_high_rate_streams() {
        // Heartbeat, position and status text keep the vehicle legible.
        assert_eq!(frame_priority(&v2_frame(0, 9)), Priority::Critical);
        assert_eq!(frame_priority(&v2_frame(33, 28)), Priority::Critical);
        assert_eq!(frame_priority(&v2_frame(253, 51)), Priority::Critical);
        // A transfer stays whole rather than stalling half downloaded.
        assert_eq!(frame_priority(&v2_frame(22, 25)), Priority::Critical);
        assert_eq!(frame_priority(&v2_frame(73, 37)), Priority::Critical);
        // The noisy sensor and servo streams are what a congested lane sheds.
        assert_eq!(frame_priority(&v2_frame(27, 26)), Priority::Bulk); // RAW_IMU
        assert_eq!(frame_priority(&v2_frame(36, 21)), Priority::Bulk); // SERVO_OUTPUT_RAW
        assert_eq!(frame_priority(&v2_frame(241, 32)), Priority::Bulk); // VIBRATION
    }

    // ---- rate shaping ----

    #[test]
    fn a_burst_is_admitted_until_the_bucket_empties() {
        let cfg = ShaperConfig {
            budget_bytes_per_sec: 100.0,
            burst_bytes: 1000.0,
            bulk_reserve_bytes: 0.0,
        };
        let start = Instant::now();
        let mut shaper = RateShaper::starting_at(cfg, start);
        // Ten 100-byte frames exactly drain a 1000-byte bucket.
        for i in 0..10 {
            assert!(
                shaper.admit(start, 100, Priority::Critical),
                "frame {i} is within the burst"
            );
        }
        assert!(
            !shaper.admit(start, 100, Priority::Critical),
            "the eleventh exceeds the cap and is shed, not queued"
        );
    }

    #[test]
    fn the_bucket_refills_at_the_configured_rate_and_stops_at_the_burst() {
        let cfg = ShaperConfig {
            budget_bytes_per_sec: 100.0,
            burst_bytes: 1000.0,
            bulk_reserve_bytes: 0.0,
        };
        let start = Instant::now();
        let mut shaper = RateShaper::starting_at(cfg, start);
        assert!(shaper.admit(start, 1000, Priority::Critical));
        assert!(!shaper.admit(start, 100, Priority::Critical));
        // Two seconds buys exactly 200 bytes back.
        let later = start + Duration::from_secs(2);
        assert!(shaper.admit(later, 200, Priority::Critical));
        assert!(!shaper.admit(later, 1, Priority::Critical));
        // A long idle refills to the burst size and no further, so the cap
        // cannot be banked indefinitely and spent in one go.
        let much_later = start + Duration::from_secs(3600);
        shaper.admit(much_later, 0, Priority::Critical);
        assert!((shaper.tokens() - 1000.0).abs() < 1e-6);
    }

    #[test]
    fn a_saturating_bulk_stream_starves_before_critical_traffic_does() {
        // This is the property that keeps a vehicle legible on a congested
        // lane: bulk runs the bucket down to the reserve and stops, and what
        // is left is available only to critical frames.
        let cfg = ShaperConfig {
            budget_bytes_per_sec: 100.0,
            burst_bytes: 1000.0,
            bulk_reserve_bytes: 400.0,
        };
        let start = Instant::now();
        let mut shaper = RateShaper::starting_at(cfg, start);

        let mut bulk_admitted = 0;
        for _ in 0..100 {
            if shaper.admit(start, 100, Priority::Bulk) {
                bulk_admitted += 1;
            }
        }
        // Bulk spends only the 600 bytes above the reserve.
        assert_eq!(bulk_admitted, 6);
        assert!((shaper.tokens() - 400.0).abs() < 1e-6);

        // Critical traffic still flows on the very same saturated bucket.
        for i in 0..4 {
            assert!(
                shaper.admit(start, 100, Priority::Critical),
                "critical frame {i} still gets through"
            );
        }
        // And the hard cap still binds: it is a ceiling for everyone.
        assert!(!shaper.admit(start, 100, Priority::Critical));
    }

    #[test]
    fn the_shipped_defaults_can_carry_a_whole_aux_frame() {
        // A burst smaller than one frame would shed every frame forever.
        let cfg = ShaperConfig::default();
        assert!(cfg.burst_bytes >= (AUX_HEADER_LEN + AUX_MAX_PAYLOAD) as f64);
        assert!(cfg.bulk_reserve_bytes < cfg.burst_bytes);
    }

    // ---- the running tee ----

    /// Drive the tee over a fake radio, returning the counters and everything
    /// the aux transmit port received.
    async fn run_tee(
        frames_to_send: Vec<Vec<u8>>,
        disabled: bool,
        shaper: ShaperConfig,
    ) -> (TeeCountersSnapshot, Vec<Vec<u8>>) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("radio-aux.sock");
        let aux_rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tx_port = aux_rx.local_addr().unwrap().port();
        let radio = fake_radio(sock_path.clone(), tx_port, disabled);
        await_socket(&sock_path).await;

        let (tx, rx) = broadcast::channel::<Vec<u8>>(64);
        let counters = Arc::new(TeeCounters::default());
        let handle = tokio::spawn(run(
            rx,
            AuxEgress::with_timeout(&sock_path, Duration::from_millis(200)),
            counters.clone(),
            shaper,
            Arc::new(Notify::new()),
        ));

        for frame in frames_to_send {
            tx.send(frame).unwrap();
        }
        // Closing the source ends the loop, which is deterministic where a
        // cancellation notify would race the select.
        drop(tx);
        handle.await.unwrap();

        let mut received = Vec::new();
        let mut buf = vec![0u8; 4096];
        while let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(50), aux_rx.recv_from(&mut buf)).await
        {
            received.push(buf[..n].to_vec());
        }
        radio.abort();
        (counters.snapshot(), received)
    }

    #[tokio::test]
    async fn a_flight_controller_frame_reaches_the_aux_transmit_port_intact() {
        let heartbeat = v2_frame(0, 9);
        let (counters, received) =
            run_tee(vec![heartbeat.clone()], false, ShaperConfig::default()).await;

        assert_eq!(counters.frames_forwarded, 1);
        assert_eq!(
            counters.bytes_forwarded,
            (AUX_HEADER_LEN + heartbeat.len()) as u64
        );
        assert_eq!(received.len(), 1);
        let (channel, payload) = aux_mux::decode(&received[0]).unwrap();
        assert_eq!(channel, AuxChannel::Mavlink);
        assert_eq!(
            payload,
            &heartbeat[..],
            "the frame crosses the lane byte for byte"
        );
    }

    #[tokio::test]
    async fn an_oversized_frame_is_dropped_whole_and_never_truncated() {
        // Larger than one aux frame can carry. Sending a prefix of it would put
        // a corrupt frame on air; the tee must drop it and say so.
        let too_big = vec![STX_V2; AUX_MAX_PAYLOAD + 1];
        let ok = v2_frame(0, 9);
        let (counters, received) =
            run_tee(vec![too_big, ok.clone()], false, ShaperConfig::default()).await;

        assert_eq!(counters.dropped_oversize, 1);
        assert_eq!(counters.frames_forwarded, 1, "the healthy frame still goes");
        assert_eq!(received.len(), 1, "nothing partial was emitted");
        assert_eq!(aux_mux::decode(&received[0]).unwrap().1, &ok[..]);
    }

    #[tokio::test]
    async fn frames_over_the_byte_budget_are_shed_rather_than_queued() {
        // A bucket that admits two frames and no more within the test's span.
        let frame = v2_frame(0, 9);
        let cost = (AUX_HEADER_LEN + frame.len()) as f64;
        let shaper = ShaperConfig {
            budget_bytes_per_sec: 1.0,
            burst_bytes: cost * 2.0,
            bulk_reserve_bytes: 0.0,
        };
        let (counters, received) = run_tee(vec![frame; 10], false, shaper).await;

        assert_eq!(counters.frames_forwarded, 2);
        assert_eq!(counters.dropped_rate_limit, 8);
        assert_eq!(
            received.len(),
            2,
            "the budget is a ceiling: the rest are dropped, not buffered"
        );
    }

    #[tokio::test]
    async fn a_congested_lane_sheds_the_noisy_stream_and_keeps_the_heartbeat() {
        // One frame of headroom above the reserve, so bulk gets exactly one
        // through and every heartbeat still crosses.
        let bulk = v2_frame(27, 26); // RAW_IMU
        let beat = v2_frame(0, 9); // HEARTBEAT
        let bulk_cost = (AUX_HEADER_LEN + bulk.len()) as f64;
        let beat_cost = (AUX_HEADER_LEN + beat.len()) as f64;
        let shaper = ShaperConfig {
            budget_bytes_per_sec: 1.0,
            burst_bytes: bulk_cost + beat_cost * 3.0,
            bulk_reserve_bytes: beat_cost * 3.0,
        };

        let mut frames = vec![bulk.clone(); 6];
        frames.extend(vec![beat.clone(); 3]);
        let (counters, received) = run_tee(frames, false, shaper).await;

        assert_eq!(counters.dropped_rate_limit, 5, "five bulk frames shed");
        assert_eq!(counters.frames_forwarded, 4, "one bulk plus three beats");
        let payloads: Vec<Vec<u8>> = received
            .iter()
            .map(|d| aux_mux::decode(d).unwrap().1.to_vec())
            .collect();
        assert_eq!(
            payloads.iter().filter(|p| *p == &beat).count(),
            3,
            "every heartbeat survives the congestion"
        );
    }

    #[tokio::test]
    async fn a_disabled_lane_forwards_nothing_and_stops_retrying() {
        let (counters, received) =
            run_tee(vec![v2_frame(0, 9); 5], true, ShaperConfig::default()).await;

        assert_eq!(counters.frames_forwarded, 0);
        assert!(received.is_empty(), "nothing is put on a forbidden lane");
        assert_eq!(
            counters.dropped_lane_down, 5,
            "every frame is accounted for"
        );
        assert_eq!(
            counters.open_errors, 0,
            "an operator's dead switch is a state, not a fault"
        );
    }

    #[tokio::test]
    async fn a_missing_radio_backs_off_instead_of_reconnecting_per_frame() {
        // No radio at all: the first frame attempts an open, and the backoff
        // means the rest are dropped without touching the command socket.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.sock");
        let (tx, rx) = broadcast::channel::<Vec<u8>>(64);
        let counters = Arc::new(TeeCounters::default());
        let handle = tokio::spawn(run(
            rx,
            AuxEgress::with_timeout(&missing, Duration::from_millis(50)),
            counters.clone(),
            ShaperConfig::default(),
            Arc::new(Notify::new()),
        ));
        for _ in 0..8 {
            tx.send(v2_frame(0, 9)).unwrap();
        }
        drop(tx);
        handle.await.unwrap();

        let snap = counters.snapshot();
        assert_eq!(snap.frames_forwarded, 0);
        assert_eq!(snap.dropped_lane_down, 8);
        assert_eq!(
            snap.open_errors, 1,
            "one open attempt, then the gate holds the rest off"
        );
    }

    #[tokio::test]
    async fn cancelling_stops_the_tee() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.sock");
        let (_tx, rx) = broadcast::channel::<Vec<u8>>(8);
        let cancel = Arc::new(Notify::new());
        let handle = tokio::spawn(run(
            rx,
            AuxEgress::with_timeout(&missing, Duration::from_millis(50)),
            Arc::new(TeeCounters::default()),
            ShaperConfig::default(),
            cancel.clone(),
        ));
        // Let the task reach its select before notifying.
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.notify_waiters();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the tee shuts down promptly")
            .unwrap();
    }
}
