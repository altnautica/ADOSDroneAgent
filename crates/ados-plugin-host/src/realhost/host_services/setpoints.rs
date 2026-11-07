//! Guided and rate setpoint host methods.

use super::*;

impl RealHost {
    pub(super) fn serve_guided_setpoint_send(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // Parse + validate the request into a setpoint, build the single
        // SET_POSITION_TARGET frame, and write it to the MAVLink socket. The
        // dispatch gate already enforced the guided-setpoint capability before
        // this runs. This is a single-shot send: the host owns no flight mode or
        // schedule, so a caller holding a velocity must re-send above the
        // autopilot's setpoint timeout (it brakes a few seconds after the last
        // setpoint) and must itself have the vehicle in its guided mode.
        let setpoint = parse_guided_setpoint(args)?;

        // With the pilot up, a local setpoint becomes a position goal under an
        // implicit lease, so the pilot's limits and obstacle planner apply.
        if let (Some(pilot), Some(goal)) = (self.reachable_pilot(), legacy_goal(&setpoint)) {
            let pilot = Arc::clone(pilot);
            let plugin = _plugin_id.to_owned();
            tokio::spawn(async move {
                if !pilot.legacy_goal(&plugin, goal).await {
                    tracing::debug!(plugin, "legacy_setpoint_no_lease");
                }
            });
            return Ok(Value::Map(vec![
                (Value::from("sent"), Value::Boolean(true)),
                (Value::from("via"), Value::from("pilot")),
            ]));
        }

        // Target: explicit args, else the autopilot observed on the router link.
        let (target_system, target_component) = command_target(args, self.fc_identity.get())?;

        // Build the typed message (re-validates inside) and serialize it to a v2
        // frame stamped with the companion source identity, so the bytes are
        // wire-identical to a SET_POSITION_TARGET any other agent surface emits.
        let msg = setpoint
            .build_message(target_system, target_component)
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        let header = ados_protocol::mavlink::MavHeader {
            system_id: GUIDED_SOURCE_SYSTEM_ID,
            component_id: GUIDED_SOURCE_COMPONENT_ID,
            // Fire-and-forget; the router does not require a specific sequence on
            // a client-written frame (it stamps its own on its own send path).
            sequence: 0,
        };
        let frame = ados_protocol::mavlink::serialize_v2(header, &msg)
            .map_err(|e| HostError::Rpc(format!("setpoint frame encode failed: {e}")))?;

        match &self.mavlink {
            // No router socket up: degrade to not_available, never error — the
            // same posture mavlink.send takes while its socket is absent.
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (
                    Value::from("method"),
                    Value::from("flight.guided_setpoint.send"),
                ),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&frame) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("msg_id"),
                        Value::Integer((setpoint_msg_id(&setpoint) as i64).into()),
                    ),
                    (
                        Value::from("len"),
                        Value::Integer((frame.len() as i64).into()),
                    ),
                ]))
            }
        }
    }

    pub(super) fn serve_rate_setpoint_send(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // Parse + validate into an attitude setpoint, build the single
        // SET_ATTITUDE_TARGET frame, and write it to the MAVLink socket. The
        // dispatch gate already enforced the attitude/rate-setpoint capability
        // before this runs. Single-shot like the guided sender: the host owns no
        // flight mode or schedule, so a caller holding an attitude must re-send
        // above the autopilot's setpoint timeout and must itself have the vehicle
        // in a mode that accepts offboard attitude (e.g. GUIDED).
        let setpoint = parse_rate_setpoint(args)?;

        // Target: explicit args, else the autopilot observed on the router link.
        let (target_system, target_component) = command_target(args, self.fc_identity.get())?;

        // Build the typed message (re-validates inside) and serialize it to a v2
        // frame stamped with the companion source identity, so the bytes are
        // wire-identical to a SET_ATTITUDE_TARGET any other agent surface emits.
        let msg = setpoint
            .build_message(target_system, target_component)
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        let header = ados_protocol::mavlink::MavHeader {
            system_id: GUIDED_SOURCE_SYSTEM_ID,
            component_id: GUIDED_SOURCE_COMPONENT_ID,
            sequence: 0,
        };
        let frame = ados_protocol::mavlink::serialize_v2(header, &msg)
            .map_err(|e| HostError::Rpc(format!("setpoint frame encode failed: {e}")))?;

        match &self.mavlink {
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (
                    Value::from("method"),
                    Value::from("flight.rate_setpoint.send"),
                ),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&frame) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("msg_id"),
                        Value::Integer(
                            (ados_protocol::mavlink::MSG_ID_SET_ATTITUDE_TARGET as i64).into(),
                        ),
                    ),
                    (
                        Value::from("len"),
                        Value::Integer((frame.len() as i64).into()),
                    ),
                ]))
            }
        }
    }
}

/// A `position` goal for a local-frame setpoint (`None` for a global one or a
/// velocity-only one, which keep the direct path).
fn legacy_goal(sp: &ados_protocol::mavlink::GuidedSetpoint) -> Option<Value> {
    use ados_protocol::mavlink::SetpointKind;
    if sp.kind != SetpointKind::LocalNed || sp.type_mask & 0b111 != 0 {
        return None;
    }
    let speed = (sp.vx * sp.vx + sp.vy * sp.vy + sp.vz * sp.vz).sqrt();
    let max_speed = if sp.type_mask & 0b111_000 == 0 && speed > 0.0 { speed.max(1.0) } else { 5.0 };
    let yaw = if sp.type_mask & (1 << 10) == 0 { Value::F32(sp.yaw) } else { Value::Nil };
    let f = |v: f64| Value::F32(v as f32);
    Some(Value::Map(vec![
        (Value::from("kind"), Value::from("position")),
        (Value::from("n"), f(sp.x)),
        (Value::from("e"), f(sp.y)),
        (Value::from("d"), f(sp.z)),
        (Value::from("vn"), Value::F32(sp.vx)),
        (Value::from("ve"), Value::F32(sp.vy)),
        (Value::from("vd"), Value::F32(sp.vz)),
        (Value::from("yaw"), yaw),
        (Value::from("max_speed_mps"), Value::F32(max_speed)),
    ]))
}
