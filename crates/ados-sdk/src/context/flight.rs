//! Flight-plane client impls: MAVLink, MSP, telemetry, setpoints.

use super::*;

impl MavlinkClient {
    /// Send a raw MAVLink frame, optionally from a registered component id.
    pub async fn send(
        &self,
        msg_bytes: &[u8],
        component_id: Option<i64>,
    ) -> Result<Value, ClientError> {
        self.ipc.mavlink_send(msg_bytes, component_id).await
    }

    /// Subscribe to a MAVLink message name; the callback fires for each matched
    /// delivery with the `msg_name`, `frame`, and `timestamp_ms` map.
    pub async fn subscribe(
        &self,
        msg_name: &str,
        callback: EventCallback,
    ) -> Result<(), ClientError> {
        self.ipc.mavlink_subscribe(msg_name, callback).await
    }

    /// Register this plugin as a MAVLink component of the given kind.
    pub async fn register_component(&self, comp_id: i64, kind: &str) -> Result<Value, ClientError> {
        self.ipc.mavlink_register_component(comp_id, kind).await
    }
}

/// Pack normalized sticks into AETR PWM channels `[roll, pitch, throttle, yaw]`,
/// applying the codec's bipolar (centre 1500) and throttle (idle 1000) scaling.
/// Separated from [`MspClient::send_sticks`] so the scaling is unit-testable
/// without an IPC transport — the whole point of the fix is that the scaling is
/// actually applied.
pub(super) fn sticks_to_channels(roll: f32, pitch: f32, yaw: f32, throttle: f32) -> [u16; 4] {
    use ados_protocol::msp::{bipolar_to_pwm, throttle_to_pwm};
    [
        bipolar_to_pwm(roll),
        bipolar_to_pwm(pitch),
        throttle_to_pwm(throttle),
        bipolar_to_pwm(yaw),
    ]
}

impl MspClient {
    /// Send already-framed MSP bytes (build them with `ados_protocol::msp`).
    /// Gated on `msp.write`.
    pub async fn send(&self, msg_bytes: &[u8]) -> Result<Value, ClientError> {
        self.ipc.msp_send(msg_bytes).await
    }

    /// Convenience: send an `MSP_SET_RAW_RC` with the given PWM channel values
    /// (v2 framing). Builds the frame with the codec and sends it. Returns an
    /// error if a v1 encode were requested with too many channels (v2 never
    /// overflows). This is the command that flies the FC in a rate mode.
    pub async fn send_raw_rc(&self, channels: &[u16]) -> Result<Value, ClientError> {
        let frame = ados_protocol::msp::set_raw_rc(channels, true)
            .ok_or_else(|| ClientError::Rpc("too many RC channels for the frame".to_string()))?;
        self.ipc.msp_send(&frame).await
    }

    /// Send an `MSP_SET_RAW_RC` from NORMALIZED stick inputs, applying the codec's
    /// stick scaling. `roll`/`pitch`/`yaw` are bipolar `-1.0..=1.0` (centre `0` →
    /// `1500` µs); `throttle` is `0.0..=1.0` (idle `0` → `1000` µs, NOT centre).
    /// Channels are packed AETR `[roll, pitch, throttle, yaw]`, the codebase
    /// convention (the `ados-crsf` bank + the GCS mapping).
    ///
    /// This is the safe way to fly an MSP FC from a control loop: [`send_raw_rc`]
    /// takes RAW PWM and applies NO scaling, so handing it normalized sticks
    /// sends near-centre garbage (and a `0.0` throttle would sit at `0`, not
    /// idle). Prefer this whenever the loop produces normalized commands.
    ///
    /// [`send_raw_rc`]: Self::send_raw_rc
    pub async fn send_sticks(
        &self,
        roll: f32,
        pitch: f32,
        yaw: f32,
        throttle: f32,
    ) -> Result<Value, ClientError> {
        self.send_raw_rc(&sticks_to_channels(roll, pitch, yaw, throttle))
            .await
    }

    /// Subscribe to the raw FC->host MSP byte stream. The callback fires for each
    /// delivered chunk with the `{bytes, timestamp_ms}` map; the plugin's own
    /// codec parses `bytes`. Gated on `msp.read`.
    pub async fn subscribe(&self, callback: EventCallback) -> Result<(), ClientError> {
        self.ipc.msp_subscribe(callback).await
    }
}

impl TelemetryClient {
    /// Receive the vehicle-state snapshot (armed, mode, position, attitude,
    /// battery, ...), the newest at most ten times a second. The callback gets
    /// the state object itself, a map keyed as the agent's state wire. A node
    /// with no flight controller never fires it. Needs `telemetry.read`.
    pub async fn subscribe(&self, callback: EventCallback) -> Result<(), ClientError> {
        let on_deliver = move |args: Value| {
            let Value::Map(entries) = args else {
                return;
            };
            if let Some((_, state)) = entries
                .into_iter()
                .find(|(k, _)| k.as_str() == Some("payload"))
            {
                callback(state);
            }
        };
        self.ipc.telemetry_subscribe(Arc::new(on_deliver)).await
    }

    /// Add a channel of fields to the telemetry stream that ships to the GCS.
    pub async fn extend(&self, channel: &str, payload: Value) -> Result<Value, ClientError> {
        self.ipc.telemetry_extend(channel, payload).await
    }
}

impl FlightClient {
    /// Send one guided-mode setpoint. `args` is the setpoint map the host
    /// validates (`kind`, `coordinate_frame`, `type_mask`, axis fields).
    pub async fn guided_setpoint(&self, args: Value) -> Result<Value, ClientError> {
        self.ipc.flight_guided_setpoint(args).await
    }

    /// Send one attitude/body-rate + thrust setpoint (SET_ATTITUDE_TARGET).
    /// `args` is the setpoint map the host validates (`type_mask`, `qw`/`qx`/
    /// `qy`/`qz`, `body_*_rate`, `thrust`).
    pub async fn rate_setpoint(&self, args: Value) -> Result<Value, ClientError> {
        self.ipc.flight_rate_setpoint(args).await
    }
}
