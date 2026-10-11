//! MAVLink, MSP, telemetry and flight-setpoint calls.

use super::*;

impl PluginIpcClient {
    /// Send a MAVLink frame, optionally from a registered component id.
    /// Mirrors `mavlink_send`.
    pub async fn mavlink_send(
        &self,
        msg_bytes: &[u8],
        component_id: Option<i64>,
    ) -> Result<Value, ClientError> {
        let mut entries = vec![(Value::from("msg_bytes"), Value::Binary(msg_bytes.to_vec()))];
        if let Some(comp) = component_id {
            entries.push((Value::from("component_id"), Value::from(comp)));
        }
        Ok(self
            .send_request("mavlink.send", "mavlink.write", Value::Map(entries))
            .await?
            .args)
    }

    /// Register a MAVLink component of the given kind (camera, gimbal, payload,
    /// peripheral, vio). The required cap is `mavlink.component.<kind>`. Mirrors
    /// `mavlink_register_component`.
    pub async fn mavlink_register_component(
        &self,
        comp_id: i64,
        kind: &str,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("component_id"), Value::from(comp_id)),
            (Value::from("kind"), Value::from(kind)),
        ]);
        let cap = format!("mavlink.component.{kind}");
        Ok(self
            .send_request("mavlink.register_component", &cap, args)
            .await?
            .args)
    }

    /// Subscribe to a MAVLink message name and register a callback. MAVLink
    /// deliveries arrive as `mavlink.deliver` envelopes and route by
    /// `msg_name`. Mirrors `mavlink_subscribe`.
    pub async fn mavlink_subscribe(
        &self,
        msg_name: &str,
        callback: EventCallback,
    ) -> Result<(), ClientError> {
        register_callback(&self.mavlink_callbacks, msg_name, callback);
        let args = Value::Map(vec![(Value::from("msg_name"), Value::from(msg_name))]);
        self.send_request("mavlink.subscribe", "mavlink.read", args)
            .await?;
        Ok(())
    }

    /// Write raw MSP bytes (already framed by `ados_protocol::msp`) to the FC.
    /// Gated on `msp.write`. The reply is `{sent, len}` or a `not_available`
    /// map when no MSP FC is wired.
    pub async fn msp_send(&self, msg_bytes: &[u8]) -> Result<Value, ClientError> {
        let args = Value::Map(vec![(
            Value::from("msg_bytes"),
            Value::Binary(msg_bytes.to_vec()),
        )]);
        Ok(self.send_request("msp.send", "msp.write", args).await?.args)
    }

    /// Subscribe to the raw FC->host MSP byte stream and register a callback.
    /// Deliveries arrive as `msp.deliver` events carrying `{bytes, timestamp_ms}`;
    /// MSP has no per-message topic, so the callback sees every chunk and its own
    /// codec parses it. Gated on `msp.read`. Mirrors the button subscribe shape.
    pub async fn msp_subscribe(&self, callback: EventCallback) -> Result<(), ClientError> {
        register_callback(&self.msp_callbacks, VISION_ANY_CAMERA, callback);
        self.send_request("msp.subscribe", "msp.read", Value::Map(vec![]))
            .await?;
        Ok(())
    }

    /// Extend the heartbeat schema with a new channel. Mirrors
    /// `telemetry_extend`.
    pub async fn telemetry_extend(
        &self,
        channel: &str,
        payload: Value,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("channel"), Value::from(channel)),
            (Value::from("payload"), payload),
        ]);
        Ok(self
            .send_request("telemetry.extend", "telemetry.extend", args)
            .await?
            .args)
    }

    /// Subscribe to the vehicle-state snapshot and register a callback. The
    /// host pushes the state object as an event on [`TELEMETRY_STATE_TOPIC`],
    /// at most [`TELEMETRY_STATE_MAX_HZ`](ados_protocol::plugin::TELEMETRY_STATE_MAX_HZ)
    /// times a second and always the newest; the callback receives the event's
    /// args map (`topic`, `payload`, `publisher`, `timestamp_ms`) like any event.
    /// Gated on `telemetry.read`. A node with no flight controller never fires
    /// it.
    pub async fn telemetry_subscribe(&self, callback: EventCallback) -> Result<(), ClientError> {
        register_callback(&self.event_callbacks, TELEMETRY_STATE_TOPIC, callback);
        self.send_request("telemetry.subscribe", "telemetry.read", Value::Map(vec![]))
            .await?;
        Ok(())
    }
}

impl PluginIpcClient {
    /// Send one application payload over a MAVLink TUNNEL frame.
    ///
    /// `payload_type` must be a private type (> 32767); the host validates it
    /// before anything is sent.
    pub async fn mavlink_tunnel_send(
        &self,
        payload_type: u16,
        payload: &[u8],
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (
                Value::from("payload_type"),
                Value::Integer(payload_type.into()),
            ),
            (Value::from("payload"), Value::Binary(payload.to_vec())),
        ]);
        Ok(self
            .send_request("mavlink.tunnel.send", "mavlink.tunnel", args)
            .await?
            .args)
    }

    /// Send one guided-mode setpoint through the scoped sender.
    ///
    /// `args` is the setpoint map the host validates (`kind`, `coordinate_frame`,
    /// `type_mask` and the axis fields). Single-shot by design: the host owns no
    /// flight mode and no schedule, so a caller holding a velocity re-sends above
    /// the autopilot's setpoint timeout or the vehicle brakes.
    pub async fn flight_guided_setpoint(&self, args: Value) -> Result<Value, ClientError> {
        Ok(self
            .send_request(
                "flight.guided_setpoint.send",
                "flight.guided_setpoint",
                args,
            )
            .await?
            .args)
    }

    /// Send one attitude/body-rate + thrust setpoint (SET_ATTITUDE_TARGET)
    /// through the scoped sender.
    ///
    /// `args` is the setpoint map the host validates (`type_mask`, `qw`/`qx`/
    /// `qy`/`qz`, `body_*_rate`, `thrust`, optional `target_system`/
    /// `target_component`). Single-shot by design, like the guided sender: the
    /// host owns no flight mode and no schedule, so a caller holding an attitude
    /// re-sends above the autopilot's setpoint timeout or the vehicle reverts.
    pub async fn flight_rate_setpoint(&self, args: Value) -> Result<Value, ClientError> {
        Ok(self
            .send_request("flight.rate_setpoint.send", "flight.rate_setpoint", args)
            .await?
            .args)
    }
}
