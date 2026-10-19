//! Peripheral drivers, cameras, buttons, GPIO, display and radio aux-stream calls.

use ados_protocol::camera_control::{
    CameraControlRequest, CameraControlResult, CAMERA_CONTROL_CAPABILITY, CAMERA_CONTROL_METHOD,
};

use super::*;

/// How long a camera control call waits: the engine applies it between two
/// frames of the camera and gives up on a stalled one after three seconds.
const CAMERA_CONTROL_TIMEOUT: Duration = Duration::from_secs(8);

impl PluginIpcClient {
    /// Register a driver of the given kind, referenced by an opaque id the host
    /// records. The required cap comes from [`driver_kind_capability`]
    /// (camera, lidar, or the payload cap for gimbal/gps/esc/payload-actuator);
    /// an unknown kind is refused here and nothing is sent. Mirrors
    /// `peripheral_register_driver`.
    pub async fn peripheral_register_driver(
        &self,
        kind: &str,
        driver_ref: &str,
    ) -> Result<Value, ClientError> {
        let cap = driver_kind_capability(kind)
            .ok_or_else(|| ClientError::Rpc(format!("unknown driver kind: {kind}")))?;
        let args = Value::Map(vec![
            (Value::from("kind"), Value::from(kind)),
            (Value::from("driver_ref"), Value::from(driver_ref)),
        ]);
        Ok(self
            .send_request("peripheral.register_driver", cap, args)
            .await?
            .args)
    }

    /// Release a previously-registered driver by handle. Mirrors
    /// `peripheral_unregister_driver`.
    pub async fn peripheral_unregister_driver(
        &self,
        handle_id: &str,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![(Value::from("handle_id"), Value::from(handle_id))]);
        Ok(self
            .send_request("peripheral.unregister_driver", "", args)
            .await?
            .args)
    }

    /// Claim a `/dev/videoN` path, optionally exclusive. Mirrors `camera_claim`.
    pub async fn camera_claim(
        &self,
        device_path: &str,
        exclusive: bool,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("device_path"), Value::from(device_path)),
            (Value::from("exclusive"), Value::Boolean(exclusive)),
        ]);
        Ok(self
            .send_request("camera.claim", "sensor.camera.register", args)
            .await?
            .args)
    }

    /// Release a previously-claimed camera path. Mirrors `camera_release`.
    pub async fn camera_release(&self, device_path: &str) -> Result<Value, ClientError> {
        let args = Value::Map(vec![(Value::from("device_path"), Value::from(device_path))]);
        Ok(self
            .send_request("camera.release", "sensor.camera.register", args)
            .await?
            .args)
    }

    /// Pull the latest captured frame from a claimed camera path. Mirrors
    /// `camera_get_frame`.
    pub async fn camera_get_frame(
        &self,
        device_path: &str,
        format: &str,
        timeout_ms: i64,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("device_path"), Value::from(device_path)),
            (Value::from("format"), Value::from(format)),
            (Value::from("timeout_ms"), Value::from(timeout_ms)),
        ]);
        Ok(self
            .send_request("camera.get_frame", "sensor.camera.register", args)
            .await?
            .args)
    }

    /// Set exposure, gain or auto exposure on a camera the vision engine owns
    /// and return what the camera reads back. Gated on `camera.control.write`
    /// ([`ClientError::CapabilityDenied`] without it). `applied` is true only
    /// when every requested control was set and reads back as asked; a
    /// simulated, network or control-less camera answers `applied: false`
    /// with a [`reason`](ados_protocol::camera_control::reason) code.
    pub async fn camera_set_controls(
        &self,
        req: &CameraControlRequest,
    ) -> Result<CameraControlResult, ClientError> {
        let reply = self
            .send_request_within(
                CAMERA_CONTROL_METHOD,
                CAMERA_CONTROL_CAPABILITY,
                req.to_args(),
                CAMERA_CONTROL_TIMEOUT,
            )
            .await?;
        decode_typed_reply(reply.args, CAMERA_CONTROL_METHOD)
    }
}

impl PluginIpcClient {
    /// Register a callback for `button.deliver` pushes.
    pub fn register_button_callback(&self, callback: EventCallback) {
        register_callback(&self.button_callbacks, VISION_ANY_CAMERA, callback);
    }
}

impl PluginIpcClient {
    /// Arm the host's front-panel button push stream for this connection.
    pub async fn button_subscribe(&self) -> Result<Value, ClientError> {
        Ok(self
            .send_request(
                ados_protocol::buttons::SUBSCRIBE,
                ados_protocol::buttons::SUBSCRIBE,
                Value::Map(vec![]),
            )
            .await?
            .args)
    }

    /// Drive a host GPIO output line high or low.
    pub async fn gpio_output_set(
        &self,
        chip: i64,
        pin: i64,
        high: bool,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("chip"), Value::Integer(chip.into())),
            (Value::from("pin"), Value::Integer(pin.into())),
            (
                Value::from("level"),
                Value::from(if high { "high" } else { "low" }),
            ),
        ]);
        Ok(self
            .send_request("gpio.output.set", "hardware.gpio_out", args)
            .await?
            .args)
    }

    /// Play a bounded beep pattern on a host GPIO line.
    ///
    /// The gpio service is the single owner of the safe bounds and clamps the
    /// pattern itself, so this sends the requested values verbatim rather than
    /// pre-clamping them to a second, drifting opinion of what is safe.
    pub async fn gpio_buzzer_beep(
        &self,
        chip: i64,
        pin: i64,
        on_ms: i64,
        cycles: i64,
        off_ms: Option<i64>,
        freq_hz: Option<i64>,
    ) -> Result<Value, ClientError> {
        let mut entries = vec![
            (Value::from("chip"), Value::Integer(chip.into())),
            (Value::from("pin"), Value::Integer(pin.into())),
            (Value::from("on_ms"), Value::Integer(on_ms.into())),
            (Value::from("cycles"), Value::Integer(cycles.into())),
        ];
        if let Some(v) = off_ms {
            entries.push((Value::from("off_ms"), Value::Integer(v.into())));
        }
        if let Some(v) = freq_hz {
            entries.push((Value::from("freq_hz"), Value::Integer(v.into())));
        }
        Ok(self
            .send_request("gpio.buzzer.beep", "hardware.gpio_out", Value::Map(entries))
            .await?
            .args)
    }

    /// Set the content of the host's reserved data-driven display page.
    ///
    /// `rows` are `(label, value)` pairs and `zones` are
    /// `(x, y, w, h, key, label)` touch rectangles in page-local content
    /// coordinates. The host writes the page sidecar the display service renders.
    pub async fn display_page_set(
        &self,
        title: &str,
        rows: &[(String, String)],
        zones: &[(i64, i64, i64, i64, String, String)],
    ) -> Result<Value, ClientError> {
        let rows_v: Vec<Value> = rows
            .iter()
            .map(|(label, value)| {
                Value::Map(vec![
                    (Value::from("label"), Value::from(label.as_str())),
                    (Value::from("value"), Value::from(value.as_str())),
                ])
            })
            .collect();
        let zones_v: Vec<Value> = zones
            .iter()
            .map(|(x, y, w, h, key, label)| {
                Value::Map(vec![
                    (Value::from("x"), Value::Integer((*x).into())),
                    (Value::from("y"), Value::Integer((*y).into())),
                    (Value::from("w"), Value::Integer((*w).into())),
                    (Value::from("h"), Value::Integer((*h).into())),
                    (Value::from("key"), Value::from(key.as_str())),
                    (Value::from("label"), Value::from(label.as_str())),
                ])
            })
            .collect();
        let args = Value::Map(vec![
            (Value::from("title"), Value::from(title)),
            (Value::from("rows"), Value::Array(rows_v)),
            (Value::from("zones"), Value::Array(zones_v)),
        ]);
        Ok(self
            .send_request("display.page.set", "display.oled.page", args)
            .await?
            .args)
    }

    /// Open the additive auxiliary application stream on the radio link.
    pub async fn radio_aux_stream_open(&self) -> Result<Value, ClientError> {
        Ok(self
            .send_request(
                "radio.aux_stream.open",
                "radio.aux_stream",
                Value::Map(vec![]),
            )
            .await?
            .args)
    }

    /// Close the auxiliary application stream.
    pub async fn radio_aux_stream_close(&self) -> Result<Value, ClientError> {
        Ok(self
            .send_request(
                "radio.aux_stream.close",
                "radio.aux_stream",
                Value::Map(vec![]),
            )
            .await?
            .args)
    }

    /// Send one application datagram on the open auxiliary stream.
    ///
    /// `channel` is the reserved aux application channel: `AppStream` (8, the
    /// stream direction) or `AppCommand` (9, the command direction). The payload
    /// is the application bytes, opaque to the host; a payload over
    /// [`ados_protocol::aux_mux::AUX_MAX_PAYLOAD`] is refused by the host before
    /// it is sent. Gated on `radio.aux_stream`.
    pub async fn radio_aux_stream_send(
        &self,
        channel: u8,
        payload: &[u8],
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("channel"), Value::Integer(channel.into())),
            (Value::from("payload"), Value::Binary(payload.to_vec())),
        ]);
        Ok(self
            .send_request("radio.aux_stream.send", "radio.aux_stream", args)
            .await?
            .args)
    }

    /// Subscribe to application datagrams received on the open auxiliary stream.
    ///
    /// The host forwards each received `(channel, payload)` as a
    /// `radio.aux_stream.deliver` event to every registered [`aux`-callback]
    /// (the plugin's codec routes by channel). Register a callback first with
    /// [`Self::register_aux_callback`] so no early frame is missed. Gated on
    /// `radio.aux_stream`. Mirrors the button subscribe shape.
    pub async fn radio_aux_stream_subscribe(&self) -> Result<Value, ClientError> {
        Ok(self
            .send_request(
                "radio.aux_stream.subscribe",
                "radio.aux_stream",
                Value::Map(vec![]),
            )
            .await?
            .args)
    }

    /// Register a callback for `radio.aux_stream.deliver` pushes. Mirrors the
    /// button callback shape: every subscriber receives every application
    /// datagram.
    pub fn register_aux_callback(&self, callback: EventCallback) {
        register_callback(&self.aux_callbacks, VISION_ANY_CAMERA, callback);
    }
}
