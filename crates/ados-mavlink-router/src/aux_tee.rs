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
use ados_protocol::shutdown::Shutdown;
use serde::Serialize;
#[cfg(test)]
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

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

/// How long a partly-filled batch waits for company before it goes out.
///
/// The auxiliary lane's loss is driven by PACKETS per second, not bytes per
/// second: measured on a live link, an identical 2.4 KB/s lost 15% when sent as
/// ten small datagrams a second and 0% as two large ones. Every datagram is a
/// separate radio transmission at the lane's minimal FEC, so the fix is to send
/// fewer, fuller datagrams rather than to shed frames.
///
/// 50 ms bounds the added latency at well under a control cycle while letting a
/// 10 Hz telemetry stream pack several frames per transmission. A batch that
/// fills first goes immediately, so a burst never waits.
const BATCH_WINDOW: Duration = Duration::from_millis(50);

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
/// Takes the frame source rather than the FC connection so the whole loop is
/// exercisable against a plain broadcast channel and a fake radio, with no
/// flight controller and no radio hardware. A plain receiver converts into
/// the flat [`crate::routing::FrameFeed`]; the router passes the routed one.
pub async fn run(
    frames: impl Into<crate::routing::FrameFeed>,
    egress: AuxEgress,
    counters: Arc<TeeCounters>,
    shaper_config: ShaperConfig,
    cancel: Shutdown,
) {
    let mut frames = frames.into();
    let mut shaper = RateShaper::new(shaper_config);
    let mut gate = LaneGate::default();
    let mut last_report = TeeCountersSnapshot::default();
    // Frames accumulate here and leave as one datagram. `batched` is the frame
    // count in flight so the counters still report frames, not datagrams.
    let mut batch: Vec<u8> = Vec::with_capacity(AUX_MAX_PAYLOAD);
    let mut batched: u64 = 0;
    let mut batch_deadline: Option<Instant> = None;
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
        // A partly-filled batch must not sit forever on a quiet link, so the
        // select waits on its deadline as well as on the next frame.
        let flush_at = batch_deadline.unwrap_or_else(|| Instant::now() + BATCH_WINDOW);
        let frame = tokio::select! {
            biased;
            _ = cancel.wait() => break,
            _ = report_tick.tick() => {
                last_report = report(&counters, last_report);
                continue;
            }
            _ = tokio::time::sleep_until(flush_at.into()), if batch_deadline.is_some() => {
                flush(&egress, &mut batch, &mut batched, &counters, &mut gate).await;
                batch_deadline = None;
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

        // Budget is charged per frame, so shedding stays per-frame and the
        // priority reserve keeps meaning what it did. Only the carrier changes.
        let cost = AUX_HEADER_LEN + frame.len();
        if !shaper.admit(Instant::now(), cost, frame_priority(&frame)) {
            counters.dropped_rate_limit.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Batch rather than send one datagram per frame. Flush first if this
        // frame would not fit, so a datagram never exceeds the lane's budget.
        if !batch.is_empty() && batch.len() + frame.len() > AUX_MAX_PAYLOAD {
            flush(&egress, &mut batch, &mut batched, &counters, &mut gate).await;
        }
        batch.extend_from_slice(&frame);
        batched += 1;
        if batch_deadline.is_none() {
            batch_deadline = Some(Instant::now() + BATCH_WINDOW);
        }
        // A full batch goes now; a partial one waits out the window above.
        if batch.len() >= AUX_MAX_PAYLOAD {
            flush(&egress, &mut batch, &mut batched, &counters, &mut gate).await;
            batch_deadline = None;
        }
    }

    // Whatever is still buffered when the loop ends has been admitted by the
    // shaper already, so it is owed a send attempt rather than a silent drop.
    flush(&egress, &mut batch, &mut batched, &counters, &mut gate).await;

    report(&counters, last_report);
    tracing::info!("mavlink_aux_tee_stopped");
}

/// Send the accumulated batch as one auxiliary-lane datagram.
///
/// Counters still tally FRAMES, not datagrams, so the numbers stay comparable
/// across the change and a reader is never misled into thinking traffic fell
/// when only the carrier got fuller.
async fn flush(
    egress: &AuxEgress,
    batch: &mut Vec<u8>,
    batched: &mut u64,
    counters: &Arc<TeeCounters>,
    gate: &mut LaneGate,
) {
    if batch.is_empty() {
        return;
    }
    let frames = *batched;
    let bytes = (AUX_HEADER_LEN + batch.len()) as u64;
    match egress.send(AuxChannel::Mavlink, batch).await {
        Ok(()) => {
            counters
                .frames_forwarded
                .fetch_add(frames, Ordering::Relaxed);
            counters.bytes_forwarded.fetch_add(bytes, Ordering::Relaxed);
        }
        Err(error) => {
            counters.send_errors.fetch_add(1, Ordering::Relaxed);
            // The send path already dropped its socket, so the next batch
            // re-opens; back off first so a persistent fault does not turn
            // into an open storm.
            gate.on_failure(Instant::now(), &error);
        }
    }
    batch.clear();
    *batched = 0;
}

#[cfg(test)]
mod tests;
