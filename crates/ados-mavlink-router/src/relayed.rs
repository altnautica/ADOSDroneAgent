//! Vehicle state for a vehicle reached over the radio, not over a local link.
//!
//! A ground station decodes another node's MAVLink off the auxiliary lane and
//! republishes it ([`crate::frame_ingest`]). That republish is deliberately
//! blind: [`crate::connection::FcConnection::inject_frame`] hands the frame to
//! the fan-out and touches nothing else, so a ground station relaying a vehicle
//! never claims to have one attached. That contract is right and this module
//! does not weaken it.
//!
//! What it left missing is a *reading* of the relayed vehicle. The frames were
//! reaching every transport the router serves, so a ground control station on
//! TCP 5760 or the MAVLink WebSocket saw the aircraft — but nothing on the node
//! itself decoded them, so every surface that reads vehicle state (the on-box
//! cockpit above all) had no attitude, no position and no battery, and rendered
//! a relayed aircraft as no aircraft at all.
//!
//! This is that reading, kept in its own type so the two can never be confused:
//!
//! * The attached-FC [`VehicleState`] is only ever driven by the local read
//!   loop. Nothing here touches it.
//! * This state carries provenance on every snapshot ([`RelayedVehicle::to_wire`]
//!   stamps `source: "relayed"`), so a consumer cannot render it as a direct
//!   link by accident.
//! * `fc_connected`, `transport_open` and `mavlink_alive` continue to describe
//!   the *local* link and stay false on a ground station. A relayed vehicle is
//!   still not an attached one; it is simply no longer invisible.
//!
//! ## Timestamps come from this node's clock, on purpose
//!
//! [`VehicleState::update_from_message`] stamps `last_update` / `last_heartbeat`
//! from the caller's `now_iso`, which here is the *ground station's* clock, not
//! the originating aircraft's. That is deliberate. Consumers judge freshness by
//! comparing those stamps against their own clock, and the browser rendering
//! this snapshot is the one running on this node. Carrying the aircraft's clock
//! across the radio would import its skew into every freshness gate on the
//! ground; stamping arrival time here keeps the comparison local and honest.
//! The trade is that these stamps measure when the frame *arrived*, not when the
//! aircraft sampled it — which is the right quantity for "is this reading still
//! worth trusting".

use std::time::{Duration, Instant};

use ados_protocol::mavlink::parse_any;
use serde_json::{json, Value};

use crate::aux_tee::mavlink_message_id;
use crate::state::{VehicleSource, VehicleState};

/// How long a relayed snapshot stays usable after the last frame that fed it.
///
/// Deliberately tighter than the four-second window the on-box cockpit allows a
/// vehicle timestamp to sit unchanged before it stops calling the reading live.
/// The node going stale *before* its own consumer does means a dead lane
/// surfaces as an explicit stale reading here rather than as a horizon quietly
/// frozen at its last value in the browser.
pub const RELAYED_STALE_AFTER: Duration = Duration::from_secs(3);

/// The MAVLink message ids [`VehicleState::update_from_message`] actually
/// consumes.
///
/// Frames are filtered against this before being parsed, for two reasons. The
/// cheap one is cost: the dialect parse allocates, and the overwhelming majority
/// of a telemetry stream is messages this state does not read. The load-bearing
/// one is honesty — a full parse fails on any message outside this build's
/// dialect, so counting those as undecodable would report a healthy lane as
/// corrupt. Filtering first means [`RelayedVehicle::frames_undecodable`] counts
/// only frames that should have parsed and did not.
///
/// `PARAM_VALUE` (22) is excluded: relayed parameters travel over the relay
/// proxy with their own request/response accounting, not this projection.
const STATE_MESSAGE_IDS: &[u32] = &[
    0,   // HEARTBEAT
    1,   // SYS_STATUS
    24,  // GPS_RAW_INT
    30,  // ATTITUDE
    33,  // GLOBAL_POSITION_INT
    65,  // RC_CHANNELS
    74,  // VFR_HUD
    147, // BATTERY_STATUS
    253, // STATUSTEXT: the firmware banner names the mode table
];

/// Whether a frame carries a message this projection reads.
fn feeds_vehicle_state(frame: &[u8]) -> bool {
    mavlink_message_id(frame).is_some_and(|id| STATE_MESSAGE_IDS.contains(&id))
}

/// The decoded state of a vehicle reached over the radio.
///
/// Holds no clock of its own: freshness is asked as of a caller-supplied
/// [`Instant`], so the whole type is deterministic under test.
#[derive(Debug, Default)]
pub struct RelayedVehicle {
    /// The decoded vehicle fields. Same aggregator as the attached path, so a
    /// relayed reading and a direct one are scaled identically.
    state: VehicleState,
    /// When the last frame that fed [`Self::state`] arrived. `None` until the
    /// first one does, which is what distinguishes "no vehicle has ever been
    /// relayed" from "one was and has gone quiet".
    last_frame_at: Option<Instant>,
    /// Which component in the relayed stream is the vehicle. The drone's tee
    /// forwards everything its flight controller emits, including other
    /// components' broadcast traffic (a ground station on the drone's own
    /// telemetry radio, a gimbal); only the autopilot's frames feed the state.
    source: VehicleSource,
    /// Frames that fed the state.
    frames_decoded: u64,
    /// Frames whose id said they should have parsed, and did not. Non-zero here
    /// is a real signal that something upstream is corrupting the lane.
    frames_undecodable: u64,
}

impl RelayedVehicle {
    /// Apply one relayed frame, returning whether it fed the state.
    ///
    /// `now_iso` is this node's current ISO-8601 UTC timestamp and `now` the
    /// matching monotonic instant; the caller computes both once per frame.
    pub fn apply_frame(&mut self, frame: &[u8], now_iso: &str, now: Instant) -> bool {
        if !feeds_vehicle_state(frame) {
            return false;
        }
        let Ok((header, msg)) = parse_any(frame) else {
            self.frames_undecodable = self.frames_undecodable.saturating_add(1);
            return false;
        };
        if !self
            .source
            .admit(header.system_id, header.component_id, &msg)
        {
            return false;
        }
        self.state.update_from_message(&msg, now_iso);
        self.last_frame_at = Some(now);
        self.frames_decoded = self.frames_decoded.saturating_add(1);
        true
    }

    /// How long since the last frame fed this state, or `None` if none ever has.
    pub fn age(&self, now: Instant) -> Option<Duration> {
        self.last_frame_at.map(|t| now.saturating_duration_since(t))
    }

    /// Whether the relayed reading is recent enough to act on.
    pub fn is_fresh(&self, now: Instant) -> bool {
        self.age(now).is_some_and(|age| age < RELAYED_STALE_AFTER)
    }

    /// Frames that fed the state.
    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }

    /// Frames that should have parsed and did not.
    pub fn frames_undecodable(&self) -> u64 {
        self.frames_undecodable
    }

    /// The snapshot for the state socket, or `None` when no vehicle has ever
    /// been relayed (so the key is absent rather than present-and-empty, and a
    /// consumer cannot mistake a node that has never seen a vehicle for one
    /// whose vehicle reads all zeroes).
    ///
    /// The vehicle fields are nested under `vehicle` rather than merged at the
    /// top level so they can never collide with, or be mistaken for, the
    /// attached-FC fields in the same snapshot. `source` is stamped on every
    /// snapshot; `fresh` is the gate a consumer should honour before rendering
    /// the reading as current.
    pub fn to_wire(&self, now: Instant) -> Option<Value> {
        let age = self.age(now)?;
        Some(json!({
            "source": "relayed",
            "fresh": self.is_fresh(now),
            "age_s": age.as_secs_f64(),
            "stale_after_s": RELAYED_STALE_AFTER.as_secs_f64(),
            "system_id": self.source.system_id(),
            "frames_decoded": self.frames_decoded,
            "frames_undecodable": self.frames_undecodable,
            "vehicle": self.state.to_wire(),
        }))
    }
}

#[cfg(test)]
mod tests;
