//! MAVLink and MSP send host methods.

use super::*;

impl RealHost {
    pub(super) fn serve_mavlink_send(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        // Order of operations: argument validation, then the frame scan, then
        // the pose-inject capability gate, then the declared component-id VIO
        // gate + reservation check, then the header component-id gates, then the
        // router-None / send. The inline gates run INSIDE the handler, after
        // validation, so a malformed request fails validation before any
        // capability check.

        // 1. Validate msg_bytes (bytes/bytearray or list-of-ints; a string is
        //    rejected). Only Binary/Array are accepted as bytes-bearing.
        let msg_value = arg_owned(args, "msg_bytes");
        let msg_bytes = match &msg_value {
            Value::Array(_) | Value::Binary(_) => {
                coerce_msg_bytes(&msg_value).map_err(HostError::Rpc)?
            }
            _ => return Err(HostError::Rpc("msg_bytes must be bytes".to_string())),
        };
        if msg_bytes.is_empty() {
            return Err(HostError::Rpc("msg_bytes must be non-empty".to_string()));
        }

        // 2. Walk EVERY frame in the buffer: the router forwards it whole, so a
        //    benign leading frame would otherwise speak for the frames behind it.
        //    A buffer the walk cannot classify is refused whatever the caller
        //    holds, because an unparseable frame can hide a whole frame inside.
        let Some(frames) = scan_outbound(&msg_bytes) else {
            return Err(HostError::Rpc(
                "msg_bytes is not a run of whole, valid MAVLink frames".to_string(),
            ));
        };

        // 3. Pose-inject gate: rejects ungranted callers regardless of the
        //    dispatch-level mavlink.write.
        if frames.requires_pose_cap && !granted_caps.contains("estimator.pose.inject") {
            return Err(HostError::CapabilityDenied(
                "estimator.pose.inject".to_string(),
            ));
        }

        // 4. Declared component-id: VIO cap gate, then reservation check, then
        //    every frame header must carry that id.
        if map_has(args, "component_id") {
            let comp = arg_owned(args, "component_id");
            if !matches!(comp, Value::Nil) {
                let comp_id = coerce_component_id(&comp).map_err(HostError::Rpc)?;
                if VIO_COMPONENT_IDS.contains(&comp_id)
                    && !granted_caps.contains("mavlink.component.vio")
                {
                    return Err(HostError::CapabilityDenied(
                        "mavlink.component.vio".to_string(),
                    ));
                }
                if !self
                    .components
                    .lock()
                    .expect("components mutex poisoned")
                    .is_registered(plugin_id, comp_id)
                {
                    return Err(HostError::Rpc(format!(
                        "component_id {comp_id} not reserved by {plugin_id}; \
                         call mavlink.register_component first"
                    )));
                }
                if let Some(header) = frames
                    .component_ids
                    .iter()
                    .find(|&&h| i64::from(h) != comp_id)
                {
                    return Err(HostError::Rpc(format!(
                        "component_id {comp_id} does not match the frame header component id {header}"
                    )));
                }
            }
        }

        // 5. Header component-ids: what each frame claims to be is gated, not
        //    what the caller declared. A VIO id needs the VIO cap and this
        //    plugin's reservation; an id another plugin reserved is refused.
        self.check_frame_components(plugin_id, &frames.component_ids, granted_caps)?;

        match &self.mavlink {
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("mavlink.send")),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&msg_bytes) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("len"),
                        Value::Integer((msg_bytes.len() as i64).into()),
                    ),
                ]))
            }
        }
    }

    pub(super) fn serve_msp_send(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // The MSP byte plane is opaque: validate that msg_bytes are bytes and
        // non-empty, then forward raw. No pose-inject scan or component gate (MSP
        // carries no such frames); the dispatch-level msp.write cap is the whole
        // gate. Failures are reported, matching mavlink.send.
        let msg_value = arg_owned(args, "msg_bytes");
        let msg_bytes = match &msg_value {
            Value::Array(_) | Value::Binary(_) => {
                coerce_msg_bytes(&msg_value).map_err(HostError::Rpc)?
            }
            _ => return Err(HostError::Rpc("msg_bytes must be bytes".to_string())),
        };
        if msg_bytes.is_empty() {
            return Err(HostError::Rpc("msg_bytes must be non-empty".to_string()));
        }
        match self.msp.as_ref() {
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("msp.send")),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&msg_bytes) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("len"),
                        Value::Integer((msg_bytes.len() as i64).into()),
                    ),
                ]))
            }
        }
    }

    pub(super) fn serve_mavlink_tunnel_send(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // Validate the request, build the single TUNNEL frame, and write it to
        // the MAVLink socket. The dispatch gate already enforced the tunnel
        // capability before this runs. The tunnel is a transparent opaque pipe:
        // this stamps no application semantics on the payload, so any per-payload
        // HMAC/replay lives inside the bytes the caller supplied.

        // payload_type is required and must be a private (application) type; the
        // builder re-checks the floor, so an out-of-range type is refused twice.
        let payload_type = arg_int_required::<u16>(args, "payload_type")?;

        // payload accepts the same shapes as mavlink.send's msg_bytes (a binary
        // value or a list of byte-ints). It may be empty (a zero-byte tunnel
        // ping). A wrong type is rejected, never silently coerced.
        let payload_value = arg_owned(args, "payload");
        let payload = match &payload_value {
            Value::Binary(_) | Value::Array(_) => {
                coerce_msg_bytes(&payload_value).map_err(HostError::Rpc)?
            }
            Value::Nil => Vec::new(),
            _ => return Err(HostError::Rpc("payload must be bytes".to_string())),
        };
        if payload.len() > ados_protocol::mavlink::TUNNEL_MAX_PAYLOAD {
            return Err(HostError::Rpc(format!(
                "payload is {} bytes, exceeds the {}-byte TUNNEL limit",
                payload.len(),
                ados_protocol::mavlink::TUNNEL_MAX_PAYLOAD
            )));
        }

        // Target: explicit args, else the autopilot observed on the router link.
        let (target_system, target_component) = command_target(args, self.fc_identity.get())?;

        let header = ados_protocol::mavlink::MavHeader {
            system_id: TUNNEL_SOURCE_SYSTEM_ID,
            component_id: TUNNEL_SOURCE_COMPONENT_ID,
            // Fire-and-forget; the router stamps its own sequence on its own send
            // path, and a client-written frame does not require a specific one.
            sequence: 0,
        };
        // The builder enforces the private-type floor and the payload width, so a
        // bad request is a clean Rpc error rather than a malformed frame.
        let frame = ados_protocol::mavlink::build_tunnel_v2(
            header,
            payload_type,
            target_system,
            target_component,
            &payload,
        )
        .map_err(|e| HostError::Rpc(e.to_string()))?;

        match &self.mavlink {
            // No router socket up: degrade to not_available, never error — the
            // same posture mavlink.send takes while its socket is absent.
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("mavlink.tunnel.send")),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&frame) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("payload_type"),
                        Value::Integer((payload_type as i64).into()),
                    ),
                    (
                        Value::from("payload_len"),
                        Value::Integer((payload.len() as i64).into()),
                    ),
                    (
                        Value::from("len"),
                        Value::Integer((frame.len() as i64).into()),
                    ),
                ]))
            }
        }
    }

    pub(super) fn serve_mavlink_register_component(
        &self,
        plugin_id: &str,
        session: u64,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        // Order of operations: validate
        // kind, coerce component_id, then the `mavlink.component.<kind>` cap gate,
        // then the VIO-kind reservation rule, then register.
        let kind = arg_str(args, "kind").filter(|k| !k.is_empty());
        let Some(kind) = kind else {
            return Err(HostError::Rpc(
                "kind must be a non-empty string".to_string(),
            ));
        };
        let comp = arg_owned(args, "component_id");
        let comp_id = coerce_component_id(&comp).map_err(HostError::Rpc)?;
        // Required cap is decided from the requested kind, gated after validation.
        let required = format!("mavlink.component.{kind}");
        if !granted_caps.contains(&required) {
            return Err(HostError::CapabilityDenied(required));
        }
        if VIO_COMPONENT_IDS.contains(&comp_id) && kind != "vio" {
            return Err(HostError::Rpc(format!(
                "component_id {comp_id} is reserved for kind=vio"
            )));
        }
        let reg = self
            .components
            .lock()
            .expect("components mutex poisoned")
            .register(plugin_id, comp_id, kind, session)
            .map_err(HostError::Rpc)?;
        Ok(Value::Map(vec![
            (Value::from("registered"), Value::Boolean(true)),
            (
                Value::from("component_id"),
                Value::Integer(reg.component_id.into()),
            ),
            (Value::from("kind"), Value::from(reg.kind.as_str())),
        ]))
    }
}
