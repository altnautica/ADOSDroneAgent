//! Hardware client impls: peripherals, cameras, GPIO, display, buttons.

use super::*;

impl PeripheralClient {
    pub async fn register_camera_driver(&self, driver_ref: &str) -> Result<Value, ClientError> {
        self.ipc
            .peripheral_register_driver("camera", driver_ref)
            .await
    }
    pub async fn register_lidar_driver(&self, driver_ref: &str) -> Result<Value, ClientError> {
        self.ipc
            .peripheral_register_driver("lidar", driver_ref)
            .await
    }
    pub async fn register_gimbal_driver(&self, driver_ref: &str) -> Result<Value, ClientError> {
        self.ipc
            .peripheral_register_driver("gimbal", driver_ref)
            .await
    }
    pub async fn register_gps_driver(&self, driver_ref: &str) -> Result<Value, ClientError> {
        self.ipc.peripheral_register_driver("gps", driver_ref).await
    }
    pub async fn register_esc_driver(&self, driver_ref: &str) -> Result<Value, ClientError> {
        self.ipc.peripheral_register_driver("esc", driver_ref).await
    }
    pub async fn register_payload_actuator_driver(
        &self,
        driver_ref: &str,
    ) -> Result<Value, ClientError> {
        self.ipc
            .peripheral_register_driver("payload-actuator", driver_ref)
            .await
    }

    /// Release a previously-registered driver by handle.
    pub async fn unregister(&self, handle_id: &str) -> Result<Value, ClientError> {
        self.ipc.peripheral_unregister_driver(handle_id).await
    }

    /// Claim a camera device path, optionally exclusive.
    pub async fn claim_camera(
        &self,
        device_path: &str,
        exclusive: bool,
    ) -> Result<Value, ClientError> {
        self.ipc.camera_claim(device_path, exclusive).await
    }
}

impl CameraClient {
    /// Claim a `/dev/videoN` path. A second exclusive claim on the same path is
    /// refused before any V4L2 handle is opened.
    pub async fn claim(&self, device_path: &str, exclusive: bool) -> Result<Value, ClientError> {
        self.ipc.camera_claim(device_path, exclusive).await
    }

    /// Release a claimed camera path.
    pub async fn release(&self, device_path: &str) -> Result<Value, ClientError> {
        self.ipc.camera_release(device_path).await
    }

    /// Pull the latest captured frame. The result map carries `frame_id`,
    /// `width`, `height`, `format`, `data`, `ts_ns`, `stale`. Repeated `stale`
    /// frames signal a stalled capture pipeline.
    pub async fn get_frame(
        &self,
        device_path: &str,
        format: &str,
        timeout_ms: i64,
    ) -> Result<Value, ClientError> {
        self.ipc
            .camera_get_frame(device_path, format, timeout_ms)
            .await
    }

    /// Set exposure, gain or auto exposure on the engine-owned camera
    /// `req.camera_id` and return the values it reads back. Needs
    /// `camera.control.write`. See [`PluginIpcClient::camera_set_controls`].
    pub async fn set_controls(
        &self,
        req: &CameraControlRequest,
    ) -> Result<CameraControlResult, ClientError> {
        self.ipc.camera_set_controls(req).await
    }
}

impl GpioClient {
    /// Drive a line high or low.
    pub async fn set(&self, chip: i64, pin: i64, high: bool) -> Result<Value, ClientError> {
        self.ipc.gpio_output_set(chip, pin, high).await
    }

    /// Play a bounded beep. `off_ms`/`freq_hz` ride through when given.
    pub async fn beep(
        &self,
        chip: i64,
        pin: i64,
        on_ms: i64,
        cycles: i64,
        off_ms: Option<i64>,
        freq_hz: Option<i64>,
    ) -> Result<Value, ClientError> {
        self.ipc
            .gpio_buzzer_beep(chip, pin, on_ms, cycles, off_ms, freq_hz)
            .await
    }
}

impl DisplayClient {
    /// Set the page content. `zones` are `(x, y, w, h, key, label)` rectangles in
    /// page-local content coordinates; a tap in one delivers `key` back to the
    /// plugin.
    pub async fn set_page(
        &self,
        title: &str,
        rows: &[(String, String)],
        zones: &[(i64, i64, i64, i64, String, String)],
    ) -> Result<Value, ClientError> {
        self.ipc.display_page_set(title, rows, zones).await
    }
}

impl ButtonClient {
    /// Receive every front-panel press.
    ///
    /// The callback gets the decoded [`ados_protocol::buttons::ButtonPress`].
    /// `action` is `None` for an unmapped button — the press is still delivered,
    /// so a plugin can bind one the operator has not assigned.
    ///
    /// A board with no front panel never fires the callback. That is the resting
    /// state, not an error, so this does not fail on a node without buttons.
    pub async fn subscribe(
        &self,
        callback: Arc<dyn Fn(ados_protocol::buttons::ButtonPress) + Send + Sync>,
    ) -> Result<(), ClientError> {
        let on_deliver = move |args: Value| {
            let Some(press) = args
                .as_map()
                .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("press")))
                .map(|(_, v)| v.clone())
            else {
                return;
            };
            // Round-trip through msgpack so the field mapping is the contract's
            // single source of truth rather than a hand-written reader here.
            let Ok(blob) = rmp_serde::to_vec_named(&press) else {
                return;
            };
            let Ok(decoded) = rmp_serde::from_slice::<ados_protocol::buttons::ButtonPress>(&blob)
            else {
                return;
            };
            callback(decoded);
        };
        self.ipc.register_button_callback(Arc::new(on_deliver));
        self.ipc.button_subscribe().await?;
        Ok(())
    }
}
