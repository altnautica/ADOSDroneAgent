//! The host's own publisher for the public `vehicle.*` and `agent.*` topics.
//!
//! Plugins may subscribe to these topics without a namespace grant and may not
//! publish into them, so the host is their only possible source. This module
//! derives the vehicle events from the flight controller's own MAVLink stream,
//! read off the router link, and publishes them with publisher id
//! [`HOST_PUBLISHER`]:
//!
//! * `vehicle.armed` / `vehicle.disarmed` on a change of the autopilot
//!   HEARTBEAT's armed flag;
//! * `vehicle.mode_changed` on a change of its `custom_mode`;
//! * `vehicle.battery_low` when SYS_STATUS reports the battery sensor
//!   unhealthy, which is the flight controller's own battery failsafe verdict;
//! * `vehicle.geofence_breach` when FENCE_STATUS moves into a breach.
//!
//! The two safety events also fire on the first report seen, so a host that
//! starts mid-flight still tells its plugins about a breach or a failsafe that
//! is already under way. Arm and mode fire only on an observed change.

use std::sync::Arc;

use ados_protocol::flight_modes::{ArduPilotFirmware, MAV_AUTOPILOT_PX4};
use ados_protocol::mavlink::ardupilotmega::{MavAutopilot, MavModeFlag, MavSysStatusSensor};
use ados_protocol::mavlink::{parse_any, MavMessage};
use rmpv::Value;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::handlers::{Event, EventBus};

/// Publisher id stamped on every event the host itself emits.
pub const HOST_PUBLISHER: &str = "host";

/// Published once the plugin host is serving.
pub const TOPIC_AGENT_READY: &str = "agent.ready";
/// Published when the plugin host is shutting down.
pub const TOPIC_AGENT_SHUTDOWN: &str = "agent.shutdown";

/// Folds the FC's MAVLink messages into the vehicle events they imply.
#[derive(Debug, Default)]
pub struct VehicleEventDeriver {
    armed: Option<bool>,
    custom_mode: Option<u32>,
    battery_healthy: Option<bool>,
    fence_breached: Option<bool>,
}

impl VehicleEventDeriver {
    /// The `(topic, payload)` pairs `msg` implies, in publish order.
    pub fn observe(&mut self, msg: &MavMessage) -> Vec<(&'static str, Value)> {
        let mut out = Vec::new();
        match msg {
            MavMessage::HEARTBEAT(hb) => {
                // Gimbals, cameras and ground stations heartbeat too; only the
                // autopilot's heartbeat speaks for the vehicle.
                if hb.autopilot == MavAutopilot::MAV_AUTOPILOT_INVALID {
                    return out;
                }
                let armed = hb
                    .base_mode
                    .contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED);
                if let Some(was) = self.armed.replace(armed) {
                    if was != armed {
                        let topic = if armed {
                            "vehicle.armed"
                        } else {
                            "vehicle.disarmed"
                        };
                        out.push((topic, Value::Map(vec![])));
                    }
                }
                if let Some(was) = self.custom_mode.replace(hb.custom_mode) {
                    if was != hb.custom_mode {
                        let name = if hb.autopilot as i64 == MAV_AUTOPILOT_PX4 {
                            None
                        } else {
                            ArduPilotFirmware::from_mav_type(hb.mavtype as i64)
                                .and_then(|fw| fw.mode_name(hb.custom_mode))
                        };
                        out.push((
                            "vehicle.mode_changed",
                            Value::Map(vec![
                                (Value::from("mode"), name.map_or(Value::Nil, Value::from)),
                                (Value::from("custom_mode"), Value::from(hb.custom_mode)),
                            ]),
                        ));
                    }
                }
            }
            MavMessage::SYS_STATUS(s) => {
                let battery = MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_BATTERY;
                if !s.onboard_control_sensors_present.contains(battery) {
                    return out;
                }
                let healthy = s.onboard_control_sensors_health.contains(battery);
                let was = self.battery_healthy.replace(healthy);
                if !healthy && was != Some(false) {
                    let pct = if s.battery_remaining >= 0 {
                        Value::from(s.battery_remaining)
                    } else {
                        Value::Nil
                    };
                    let volts = if s.voltage_battery == u16::MAX {
                        Value::Nil
                    } else {
                        Value::F64(f64::from(s.voltage_battery) / 1000.0)
                    };
                    out.push((
                        "vehicle.battery_low",
                        Value::Map(vec![
                            (Value::from("battery_pct"), pct),
                            (Value::from("voltage_v"), volts),
                        ]),
                    ));
                }
            }
            MavMessage::FENCE_STATUS(f) => {
                let breached = f.breach_status != 0;
                let was = self.fence_breached.replace(breached);
                if breached && was != Some(true) {
                    out.push((
                        "vehicle.geofence_breach",
                        Value::Map(vec![
                            (
                                Value::from("breach_type"),
                                Value::from(f.breach_type as i64),
                            ),
                            (Value::from("breach_count"), Value::from(f.breach_count)),
                        ]),
                    ));
                }
            }
            _ => {}
        }
        out
    }
}

/// Publish one host event on `bus`.
pub fn publish_host_event(bus: &EventBus, topic: &str, payload: Value) -> usize {
    bus.publish(Event {
        topic: topic.to_string(),
        timestamp_ms: now_ms(),
        publisher_plugin_id: HOST_PUBLISHER.to_string(),
        payload,
    })
}

/// Drain the FC frame fanout forever, publishing the vehicle events it implies.
pub fn spawn_vehicle_events(
    bus: Arc<EventBus>,
    mut frames: broadcast::Receiver<Vec<u8>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut deriver = VehicleEventDeriver::default();
        loop {
            let chunk = match frames.recv().await {
                Ok(chunk) => chunk,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            };
            for frame in ados_protocol::aux_mux::split_frames(&chunk) {
                let Ok((_, msg)) = parse_any(frame) else {
                    continue;
                };
                for (topic, payload) in deriver.observe(&msg) {
                    publish_host_event(&bus, topic, payload);
                }
            }
        }
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::mavlink::ardupilotmega::{
        MavType, FENCE_STATUS_DATA, HEARTBEAT_DATA, SYS_STATUS_DATA,
    };
    use ados_protocol::mavlink::Message;

    fn heartbeat(armed: bool, custom_mode: u32) -> MavMessage {
        let MavMessage::HEARTBEAT(mut hb) = MavMessage::default_message_from_id(0).unwrap() else {
            unreachable!()
        };
        hb.autopilot = MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA;
        hb.mavtype = MavType::MAV_TYPE_QUADROTOR;
        hb.custom_mode = custom_mode;
        hb.base_mode = if armed {
            MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED
        } else {
            MavModeFlag::empty()
        };
        MavMessage::HEARTBEAT(hb)
    }

    fn topics(events: Vec<(&'static str, Value)>) -> Vec<&'static str> {
        events.into_iter().map(|(t, _)| t).collect()
    }

    #[test]
    fn arm_and_mode_changes_publish_but_the_first_heartbeat_does_not() {
        let mut d = VehicleEventDeriver::default();
        assert!(d.observe(&heartbeat(false, 0)).is_empty());
        assert!(d.observe(&heartbeat(false, 0)).is_empty());
        assert_eq!(topics(d.observe(&heartbeat(true, 0))), ["vehicle.armed"]);
        let events = d.observe(&heartbeat(true, 6));
        assert_eq!(events.len(), 1);
        let (topic, payload) = &events[0];
        assert_eq!(*topic, "vehicle.mode_changed");
        assert_eq!(
            payload,
            &Value::Map(vec![
                (Value::from("mode"), Value::from("RTL")),
                (Value::from("custom_mode"), Value::from(6u32)),
            ])
        );
        assert_eq!(
            topics(d.observe(&heartbeat(false, 6))),
            ["vehicle.disarmed"]
        );
    }

    #[test]
    fn a_non_autopilot_heartbeat_is_ignored() {
        let mut d = VehicleEventDeriver::default();
        d.observe(&heartbeat(false, 0));
        let MavMessage::HEARTBEAT(mut gimbal) = heartbeat(true, 9) else {
            unreachable!()
        };
        gimbal.autopilot = MavAutopilot::MAV_AUTOPILOT_INVALID;
        assert!(d.observe(&MavMessage::HEARTBEAT(gimbal)).is_empty());
    }

    fn sys_status(battery_healthy: bool, remaining: i8) -> MavMessage {
        let battery = MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_BATTERY;
        let MavMessage::SYS_STATUS(mut s) = MavMessage::default_message_from_id(1).unwrap() else {
            unreachable!()
        };
        s.onboard_control_sensors_present = battery;
        s.onboard_control_sensors_enabled = battery;
        s.onboard_control_sensors_health = if battery_healthy {
            battery
        } else {
            MavSysStatusSensor::empty()
        };
        s.battery_remaining = remaining;
        s.voltage_battery = 14_800;
        let _: &SYS_STATUS_DATA = &s;
        MavMessage::SYS_STATUS(s)
    }

    #[test]
    fn the_battery_failsafe_publishes_once_per_episode() {
        let mut d = VehicleEventDeriver::default();
        assert!(d.observe(&sys_status(true, 40)).is_empty());
        let events = d.observe(&sys_status(false, 18));
        assert_eq!(topics(events.clone()), ["vehicle.battery_low"]);
        assert_eq!(
            events[0].1,
            Value::Map(vec![
                (Value::from("battery_pct"), Value::from(18i8)),
                (Value::from("voltage_v"), Value::F64(14.8)),
            ])
        );
        assert!(d.observe(&sys_status(false, 17)).is_empty());
        assert!(d.observe(&sys_status(true, 60)).is_empty());
        assert_eq!(
            topics(d.observe(&sys_status(false, 15))),
            ["vehicle.battery_low"]
        );
    }

    #[test]
    fn a_fence_breach_publishes_on_entry_including_the_first_report() {
        let fence = |status: u8| {
            let MavMessage::FENCE_STATUS(mut f) = MavMessage::default_message_from_id(162).unwrap()
            else {
                unreachable!()
            };
            f.breach_status = status;
            f.breach_count = 1;
            let _: &FENCE_STATUS_DATA = &f;
            MavMessage::FENCE_STATUS(f)
        };
        let mut d = VehicleEventDeriver::default();
        assert_eq!(topics(d.observe(&fence(1))), ["vehicle.geofence_breach"]);
        assert!(d.observe(&fence(1)).is_empty());
        assert!(d.observe(&fence(0)).is_empty());
        assert_eq!(topics(d.observe(&fence(1))), ["vehicle.geofence_breach"]);
    }

    #[tokio::test]
    async fn frames_on_the_link_reach_bus_subscribers() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe();
        let (tx, rx) = broadcast::channel(8);
        let _task = spawn_vehicle_events(bus.clone(), rx);
        let header = ados_protocol::mavlink::MavHeader::default();
        for armed in [false, true] {
            let frame = ados_protocol::mavlink::serialize_v2(header, &heartbeat(armed, 0)).unwrap();
            tx.send(frame).unwrap();
        }
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.topic, "vehicle.armed");
        assert_eq!(event.publisher_plugin_id, HOST_PUBLISHER);
        let _: HEARTBEAT_DATA = match heartbeat(false, 0) {
            MavMessage::HEARTBEAT(hb) => hb,
            _ => unreachable!(),
        };
    }
}
