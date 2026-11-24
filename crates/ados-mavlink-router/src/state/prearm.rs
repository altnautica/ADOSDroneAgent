//! The PreArm/Arm STATUSTEXT ring: long FC texts arrive in 50-byte chunks and
//! are joined before they enter the ring, so a surface shows one line.

use std::time::Instant;

use ados_protocol::mavlink::ardupilotmega::MavSysStatusSensor;
use ados_protocol::statustext::{StatusTextAssembler, CHUNK_LEN};

use super::{VehicleState, PREARM_MAX, PREARM_WINDOW};

/// The chunk joiner the PreArm ring is fed through, with the instant its
/// millisecond clock counts from (the first STATUSTEXT seen).
#[derive(Debug, Clone, Default)]
pub struct PrearmChunks {
    joiner: StatusTextAssembler,
    epoch: Option<Instant>,
}

impl PrearmChunks {
    fn ms(&mut self, now: Instant) -> u64 {
        let epoch = *self.epoch.get_or_insert(now);
        now.saturating_duration_since(epoch).as_millis() as u64
    }
}

impl VehicleState {
    /// Fold one STATUSTEXT chunk; every PreArm/Arm text it completes joins
    /// the ring.
    pub(super) fn on_prearm_chunk(&mut self, severity: u8, text: &[u8; CHUNK_LEN]) {
        let now = self.clock.now();
        let ms = self.prearm_chunks.ms(now);
        let mut done = self.prearm_chunks.joiner.flush_stale(ms);
        done.extend(
            self.prearm_chunks
                .joiner
                .push_untagged(0, 0, severity, text, ms),
        );
        for (_, text) in done {
            if text.starts_with("PreArm:") || text.starts_with("Arm:") {
                // A repeat refreshes its line rather than listing it twice.
                self.prearm.retain(|(_, t)| *t != text);
                self.prearm.push_back((now, text));
            }
        }
        self.prune_prearm(now);
    }

    /// SYS_STATUS: an FC that reports its pre-arm check and passes it has no
    /// PreArm failure left, so the ring empties at once rather than ageing out.
    pub(super) fn on_prearm_health(&mut self, present: u32, health: u32) {
        let bit = MavSysStatusSensor::MAV_SYS_STATUS_PREARM_CHECK.bits();
        if present & bit != 0 && health & bit != 0 {
            self.prearm.clear();
        }
    }

    /// Drop PreArm texts older than the window and bound the ring.
    pub(super) fn prune_prearm(&mut self, now: Instant) {
        while self
            .prearm
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= PREARM_WINDOW)
            || self.prearm.len() > PREARM_MAX
        {
            self.prearm.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use ados_protocol::mavlink::ardupilotmega::{
        MavAutopilot, MavModeFlag, MavSeverity, MavState, MavSysStatusSensor, MavType,
        HEARTBEAT_DATA, STATUSTEXT_DATA, SYS_STATUS_DATA,
    };
    use ados_protocol::mavlink::MavMessage;

    use super::super::VehicleState;
    use crate::clock::tests::{advance, sim_clock};

    const TS: &str = "2026-05-28T15:28:23.880948+00:00";

    fn text(s: &str) -> MavMessage {
        let mut buf = [0u8; 50];
        buf[..s.len()].copy_from_slice(s.as_bytes());
        MavMessage::STATUSTEXT(STATUSTEXT_DATA {
            severity: MavSeverity::MAV_SEVERITY_CRITICAL,
            text: buf.into(),
        })
    }

    fn prearm(s: &VehicleState) -> serde_json::Value {
        s.to_wire()["prearm"].clone()
    }

    fn sys_status(healthy: bool) -> MavMessage {
        let bit = MavSysStatusSensor::MAV_SYS_STATUS_PREARM_CHECK;
        MavMessage::SYS_STATUS(SYS_STATUS_DATA {
            onboard_control_sensors_present: bit,
            onboard_control_sensors_enabled: bit,
            onboard_control_sensors_health: if healthy {
                bit
            } else {
                MavSysStatusSensor::empty()
            },
            ..Default::default()
        })
    }

    fn sim() -> (VehicleState, std::sync::Arc<ados_time::SimFeed>) {
        let (clock, feed) = sim_clock();
        advance(&feed, 1, 1_000);
        (
            VehicleState {
                clock,
                ..VehicleState::default()
            },
            feed,
        )
    }

    /// ArduPilot repeats a failing PreArm every 30 s: the line holds through
    /// the gap as one entry, and ages out once the repeats stop.
    #[test]
    fn a_prearm_line_holds_between_thirty_second_repeats() {
        let (mut s, feed) = sim();
        s.update_from_message(&text("PreArm: Compass not calibrated"), TS);
        advance(&feed, 2, 30_000);
        assert_eq!(
            prearm(&s),
            serde_json::json!(["PreArm: Compass not calibrated"])
        );
        s.update_from_message(&text("PreArm: Compass not calibrated"), TS);
        assert_eq!(
            prearm(&s),
            serde_json::json!(["PreArm: Compass not calibrated"])
        );
        advance(&feed, 3, 66_000);
        assert_eq!(prearm(&s), serde_json::json!([]));
    }

    /// Passing checks (SYS_STATUS pre-arm bit healthy) or arming clears the
    /// ring at once; a failing pre-arm bit keeps it.
    #[test]
    fn passing_checks_or_arming_clear_the_lines() {
        let (mut s, _feed) = sim();
        s.update_from_message(&text("PreArm: Compass not calibrated"), TS);
        s.update_from_message(&sys_status(false), TS);
        assert_eq!(
            prearm(&s),
            serde_json::json!(["PreArm: Compass not calibrated"])
        );
        s.update_from_message(&sys_status(true), TS);
        assert_eq!(prearm(&s), serde_json::json!([]));

        s.update_from_message(&text("PreArm: Compass not calibrated"), TS);
        s.update_from_message(
            &MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                custom_mode: 0,
                mavtype: MavType::MAV_TYPE_QUADROTOR,
                autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
                base_mode: MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED,
                system_status: MavState::MAV_STATE_ACTIVE,
                mavlink_version: 3,
            }),
            TS,
        );
        assert_eq!(prearm(&s), serde_json::json!([]));
    }
}
