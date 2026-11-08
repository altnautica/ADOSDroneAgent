//! Which component on a link is the vehicle.

use ados_protocol::mavlink::ardupilotmega::{MavAutopilot, MavMessage};

/// Which component on a MAVLink link is the vehicle, and whether a frame
/// describes it.
///
/// A flight controller forwards broadcast traffic from every other component on
/// its links to this one: a ground station's HEARTBEAT arriving on a telemetry
/// radio, a gimbal's, another companion computer's. None of those is the
/// vehicle. Taking any HEARTBEAT as the vehicle's let a ground station's
/// (`MAV_TYPE_GCS`, base mode 0) overwrite armed, mode and type once a second
/// and re-point every request at the ground station's system id.
///
/// The vehicle is the component whose HEARTBEAT names an autopilot. Its
/// `(system_id, component_id)` is latched from that HEARTBEAT, and every other
/// frame is admitted only when it comes from the same component, so a PARAM_VALUE
/// from a gimbal or a status message from another companion never lands in the
/// vehicle's state or parameter cache. Nothing is admitted before the first
/// autopilot HEARTBEAT.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VehicleSource {
    vehicle: Option<(u8, u8)>,
}

impl VehicleSource {
    /// Whether a frame from `(system_id, component_id)` carrying `msg` comes
    /// from the vehicle. An autopilot HEARTBEAT re-latches the vehicle identity,
    /// so a flight controller whose system id is changed is followed.
    pub fn admit(&mut self, system_id: u8, component_id: u8, msg: &MavMessage) -> bool {
        match msg {
            MavMessage::HEARTBEAT(hb) => {
                if hb.autopilot == MavAutopilot::MAV_AUTOPILOT_INVALID {
                    return false;
                }
                self.vehicle = Some((system_id, component_id));
                true
            }
            _ => self.vehicle == Some((system_id, component_id)),
        }
    }

    /// The latched vehicle system id, `None` until an autopilot HEARTBEAT.
    pub fn system_id(&self) -> Option<u8> {
        self.vehicle.map(|(sys, _)| sys)
    }
}
