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

use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use serde::Serialize;

mod lane;
mod shaper;
mod tee;

pub use shaper::{frame_priority, mavlink_message_id};
pub use tee::run;

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

#[cfg(test)]
mod tests;
