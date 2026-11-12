//! The FC's landed state (EXTENDED_SYS_STATE) as the snapshot publishes it.

use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::MavLandedState;

use super::VehicleState;

/// MAVLink id of EXTENDED_SYS_STATE.
pub const MSG_EXTENDED_SYS_STATE: u32 = 245;

/// How long a decoded `landed_state` stays published. The router asks for
/// the message at 1–2 Hz, so this spans several missed reports; past it the
/// field reads `null` rather than holding a landed or airborne claim the FC
/// no longer makes.
pub const LANDED_STATE_MAX_AGE: Duration = Duration::from_secs(5);

/// The wire name of a MAV_LANDED_STATE value.
pub fn landed_state_name(s: MavLandedState) -> &'static str {
    match s {
        MavLandedState::MAV_LANDED_STATE_UNDEFINED => "undefined",
        MavLandedState::MAV_LANDED_STATE_ON_GROUND => "on_ground",
        MavLandedState::MAV_LANDED_STATE_IN_AIR => "in_air",
        MavLandedState::MAV_LANDED_STATE_TAKEOFF => "takeoff",
        MavLandedState::MAV_LANDED_STATE_LANDING => "landing",
    }
}

impl VehicleState {
    /// The FC's landed state while its last EXTENDED_SYS_STATE is younger
    /// than [`LANDED_STATE_MAX_AGE`] at `now`. `None` when the FC never sent
    /// one or it went quiet: "not reported" is not the FC's own `undefined`.
    pub fn landed_state_at(&self, now: Instant) -> Option<MavLandedState> {
        self.landed_state
            .filter(|(_, at)| now.saturating_duration_since(*at) < LANDED_STATE_MAX_AGE)
            .map(|(s, _)| s)
    }
}
