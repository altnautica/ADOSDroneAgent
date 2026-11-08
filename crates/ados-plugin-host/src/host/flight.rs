//! The flight-control slice of the host services: guided and attitude
//! setpoints, and the pilot's motion lease.

use std::future::Future;

use rmpv::Value;

use super::{not_implemented, HostError, HostResult};

/// Flight-control host services, a supertrait of
/// [`HostServices`](super::HostServices). Every method defaults to
/// [`not_implemented`], so [`NoopHost`](super::NoopHost) stays inert; a real
/// host overrides the methods it serves.
pub trait FlightServices: Send + Sync + 'static {
    /// Send one guided-mode position/velocity setpoint to the flight controller.
    /// A real host validates the request (finite numbers on every active axis, a
    /// sane type mask, a coordinate frame valid for the message kind), builds the
    /// single `SET_POSITION_TARGET` MAVLink frame, and writes it to the MAVLink
    /// socket the router forwards to the FC. Fully gated at the dispatch level on
    /// the guided-setpoint capability, so it does not see the caller's caps.
    ///
    /// This is a single-shot send: the host does not own any flight mode or
    /// schedule. To hold a commanded velocity the caller must re-send well above
    /// the autopilot's setpoint-timeout rate (the autopilot brakes a few seconds
    /// after the last setpoint), and must itself have placed the vehicle in its
    /// guided mode. The default returns `not_implemented` so
    /// [`NoopHost`](super::NoopHost) stays inert.
    fn guided_setpoint_send(
        &self,
        _plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        Ok(not_implemented("flight.guided_setpoint.send"))
    }

    /// Send one `SET_ATTITUDE_TARGET` setpoint (an attitude quaternion and/or
    /// body rates, plus collective thrust) to the flight controller via the
    /// scoped attitude sender. Like [`Self::guided_setpoint_send`] this is a
    /// single-shot send: the host owns no flight mode or schedule, so the caller
    /// must re-send above the autopilot's setpoint-timeout rate and must itself
    /// have placed the vehicle in a mode that accepts offboard attitude (e.g.
    /// GUIDED). The default returns `not_implemented` so
    /// [`NoopHost`](super::NoopHost) stays inert.
    fn rate_setpoint_send(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("flight.rate_setpoint.send"))
    }

    /// Ask the pilot for its single motion lease (`{kind, skill_id}`); the host
    /// stamps the caller as the holder. Gated on `flight.motion`. The default
    /// returns `not_implemented`.
    fn flight_lease_acquire(
        &self,
        _plugin_id: &str,
        _args: &Value,
    ) -> impl Future<Output = Result<HostResult, HostError>> + Send {
        std::future::ready(Ok(not_implemented("flight.lease.acquire")))
    }

    /// Give the motion lease back (`{lease_id}`). Gated on `flight.motion`.
    fn flight_lease_release(
        &self,
        _plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        Ok(not_implemented("flight.lease.release"))
    }

    /// Send the pilot one goal under the caller's lease (`{lease_id, goal}`).
    /// Gated on `flight.motion`.
    fn flight_goal_send(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("flight.goal.send"))
    }

    /// Ask for the operator's shaped sticks as `flight.sticks` events while the
    /// caller holds the lease. Gated on `flight.motion`.
    fn flight_sticks_subscribe(
        &self,
        _plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        Ok(not_implemented("flight.sticks.subscribe"))
    }
}
