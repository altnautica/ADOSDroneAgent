//! Plugin-facing facade over the IPC client.
//!
//! Ports `ados.plugins.ipc.context`. [`PluginContext`] is the object handed to
//! every lifecycle hook. Each field is a thin capability-gated facade backed by
//! one [`PluginIpcClient`] call: the facade shapes arguments and decodes the
//! response; the host enforces capabilities. The facades share one client
//! behind an `Arc` so the context can be passed by reference into hooks while
//! the client owns the single connection.

use std::collections::BTreeMap;
use std::sync::Arc;

use rmpv::Value;

use crate::client::{ClientError, EventCallback, PluginIpcClient};
use crate::vision::VisionClient;

/// `ctx.events` — the event bus facade.
#[derive(Clone)]
pub struct EventsClient {
    ipc: Arc<PluginIpcClient>,
}

impl EventsClient {
    /// Publish a payload on a topic. Returns the delivered count.
    pub async fn publish(&self, topic: &str, payload: Value) -> Result<i64, ClientError> {
        self.ipc.event_publish(topic, payload).await
    }

    /// Subscribe to a topic pattern; the callback fires for each matched
    /// delivery with the event's `args` map (`topic`, `payload`, `publisher`,
    /// `timestamp_ms`).
    pub async fn subscribe(
        &self,
        topic_pattern: &str,
        callback: EventCallback,
    ) -> Result<(), ClientError> {
        self.ipc.event_subscribe(topic_pattern, callback).await
    }
}

/// `ctx.mavlink` — read and write through the host's MAVLink router.
#[derive(Clone)]
pub struct MavlinkClient {
    ipc: Arc<PluginIpcClient>,
}

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
fn sticks_to_channels(roll: f32, pitch: f32, yaw: f32, throttle: f32) -> [u16; 4] {
    use ados_protocol::msp::{bipolar_to_pwm, throttle_to_pwm};
    [
        bipolar_to_pwm(roll),
        bipolar_to_pwm(pitch),
        throttle_to_pwm(throttle),
        bipolar_to_pwm(yaw),
    ]
}

/// `ctx.msp` — read and write raw MSP to a Betaflight / iNav / KISS FC through
/// the host's MSP byte plane. The sibling of [`MavlinkClient`] for an FC that
/// speaks MSP; the codec that builds/parses the bytes is
/// [`ados_protocol::msp`](ados_protocol::msp).
#[derive(Clone)]
pub struct MspClient {
    ipc: Arc<PluginIpcClient>,
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

/// `ctx.telemetry` — extend the heartbeat schema.
#[derive(Clone)]
pub struct TelemetryClient {
    ipc: Arc<PluginIpcClient>,
}

impl TelemetryClient {
    /// Add a channel of fields to the telemetry stream that ships to the GCS.
    pub async fn extend(&self, channel: &str, payload: Value) -> Result<Value, ClientError> {
        self.ipc.telemetry_extend(channel, payload).await
    }
}

/// `ctx.peripheral_manager` — register driver instances and claim cameras.
///
/// A driver is registered by an opaque reference id; the driver itself keeps
/// running in the plugin process. The host records the claim and routes the
/// driver kind through its registry. This mirrors `_driver_ref`: the host never
/// sees the live driver object.
#[derive(Clone)]
pub struct PeripheralClient {
    ipc: Arc<PluginIpcClient>,
}

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

/// `ctx.camera` — path-level claim/release plus a frame-pull primitive.
#[derive(Clone)]
pub struct CameraClient {
    ipc: Arc<PluginIpcClient>,
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
}

/// `ctx.config` — live config kv plus the manifest-supplied static config.
///
/// `ctx.gpio` — host GPIO output.
///
/// Drive a status line or play a bounded buzzer pattern. The gpio service owns
/// the safe bounds, so a plugin requests a pattern and the service clamps it —
/// there is deliberately no client-side clamp to drift from the real ceiling.
#[derive(Clone)]
pub struct GpioClient {
    ipc: Arc<PluginIpcClient>,
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

/// `ctx.display` — the reserved data-driven display page.
///
/// A plugin owns one page of title, `(label, value)` rows and touch zones
/// without recompiling the display service; the host renders it from the sidecar
/// this writes.
#[derive(Clone)]
pub struct DisplayClient {
    ipc: Arc<PluginIpcClient>,
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

/// `ctx.radio` — the additive auxiliary application stream on the link.
///
/// Opens a plugin-owned lane alongside the existing telemetry/video planes,
/// gated so it only exists while the plugin is active.
#[derive(Clone)]
pub struct RadioClient {
    ipc: Arc<PluginIpcClient>,
}

impl RadioClient {
    /// Open the auxiliary stream.
    pub async fn open_aux_stream(&self) -> Result<Value, ClientError> {
        self.ipc.radio_aux_stream_open().await
    }

    /// Close the auxiliary stream.
    pub async fn close_aux_stream(&self) -> Result<Value, ClientError> {
        self.ipc.radio_aux_stream_close().await
    }
}

/// `ctx.buttons` — front-panel presses.
///
/// The on-device input surface for a plugin whose operator has no screen and no
/// ground station. The host owns the bus and the short/long decode, so a plugin
/// never re-implements debounce or the action mapping and cannot drift from what
/// the panel's own UI thinks a press means.
///
/// Read-only and non-exclusive: several consumers watch the same bus, so
/// subscribing observes presses without consuming or remapping them.
#[derive(Clone)]
pub struct ButtonClient {
    ipc: Arc<PluginIpcClient>,
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

/// `ctx.flight` — the scoped guided-setpoint sender.
///
/// A flight-behaviour plugin commands the vehicle through this rather than raw
/// MAVLink writes, so the host gates the whole flight-command surface with one
/// capability. Single-shot by design: the host owns no flight mode and no
/// schedule, so a caller holding a velocity must re-send above the autopilot's
/// setpoint timeout or the vehicle brakes.
#[derive(Clone)]
pub struct FlightClient {
    ipc: Arc<PluginIpcClient>,
}

impl FlightClient {
    /// Send one guided-mode setpoint. `args` is the setpoint map the host
    /// validates (`kind`, `coordinate_frame`, `type_mask`, axis fields).
    pub async fn guided_setpoint(&self, args: Value) -> Result<Value, ClientError> {
        self.ipc.flight_guided_setpoint(args).await
    }
}

/// The static config is the manifest dict read at runner start; `get`/`set`
/// reach the host's live kv. Read order on the host side is drone scope (when
/// bound) -> global -> default, mirroring `_ConfigClient`.
#[derive(Clone)]
pub struct ConfigClient {
    ipc: Arc<PluginIpcClient>,
    static_config: Arc<BTreeMap<String, Value>>,
}

impl ConfigClient {
    /// Read a key from the manifest-supplied static config; synchronous.
    pub fn static_get(&self, key: &str) -> Option<&Value> {
        self.static_config.get(key)
    }

    /// Read a key from the host's live kv, falling back to `default`.
    pub async fn get(&self, key: &str, default: Value) -> Result<Value, ClientError> {
        self.ipc.config_get(key, default).await
    }

    /// Write a key to the host's live kv in the given scope (`drone`/`global`).
    pub async fn set(&self, key: &str, value: Value, scope: &str) -> Result<Value, ClientError> {
        self.ipc.config_set(key, value, scope).await
    }
}

/// `ctx.process` — sandboxed vendor-binary spawn authorization.
#[derive(Clone)]
pub struct ProcessClient {
    ipc: Arc<PluginIpcClient>,
}

impl ProcessClient {
    /// Authorize a vendor-binary spawn. The host enforces the manifest
    /// allowlist and returns the resolved install dir; the actual exec is the
    /// plugin's to perform so the child inherits the runner's cgroup slice.
    pub async fn spawn(
        &self,
        basename: &str,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Result<Value, ClientError> {
        self.ipc.process_spawn(basename, args, env).await
    }
}

/// `ctx.lifecycle` — subscribe to GCS-side mount events.
///
/// `on_pause` fires when the operator switches away from the drone hosting this
/// plugin's UI; `on_resume` fires on switch-back. Both ride the plugin's own
/// `plugin.<id>.lifecycle.*` namespace, mirroring `_LifecycleClient`.
#[derive(Clone)]
pub struct LifecycleClient {
    ipc: Arc<PluginIpcClient>,
}

impl LifecycleClient {
    pub async fn on_pause(&self, handler: EventCallback) -> Result<(), ClientError> {
        let topic = format!("plugin.{}.lifecycle.pause", self.ipc.plugin_id());
        self.ipc.event_subscribe(&topic, handler).await
    }

    pub async fn on_resume(&self, handler: EventCallback) -> Result<(), ClientError> {
        let topic = format!("plugin.{}.lifecycle.resume", self.ipc.plugin_id());
        self.ipc.event_subscribe(&topic, handler).await
    }
}

/// The object handed to every lifecycle hook. Every host-facing surface is a
/// capability-gated facade; the IPC client is an implementation detail.
///
/// Ports `ados.plugins.ipc.context.PluginContext`. The `peripherals` field is
/// an alias for `peripheral_manager`, matching the Python back-compat alias.
#[derive(Clone)]
pub struct PluginContext {
    pub plugin_id: String,
    pub plugin_version: String,
    pub agent_id: String,
    /// The plugin's per-drone data directory, when the host set one on the unit.
    /// `None` on a host that did not — a plugin must handle its absence rather
    /// than assume a path.
    pub data_dir: Option<std::path::PathBuf>,
    pub events: EventsClient,
    pub mavlink: MavlinkClient,
    /// `ctx.msp` — raw MSP read/write for a Betaflight / iNav / KISS FC.
    pub msp: MspClient,
    pub telemetry: TelemetryClient,
    pub peripheral_manager: PeripheralClient,
    /// Alias for `peripheral_manager` (Python `ctx.peripherals`).
    pub peripherals: PeripheralClient,
    pub camera: CameraClient,
    /// `ctx.vision` — subscribe to engine frames, register models, run
    /// inference, publish detections, and inject visual-odometry pose.
    pub vision: VisionClient,
    pub config: ConfigClient,
    /// Front-panel button presses; quiet on a board with no panel.
    pub buttons: ButtonClient,
    /// The scoped guided-setpoint sender.
    pub flight: FlightClient,
    /// Host GPIO output (status line, buzzer).
    pub gpio: GpioClient,
    /// The reserved data-driven display page.
    pub display: DisplayClient,
    /// The additive auxiliary radio stream.
    pub radio: RadioClient,
    pub process: ProcessClient,
    pub lifecycle: LifecycleClient,
    ipc: Arc<PluginIpcClient>,
}

impl PluginContext {
    /// Build a context over a connected client. `static_config` is the
    /// manifest-supplied config dict the runner read at start.
    pub fn new(
        ipc: Arc<PluginIpcClient>,
        plugin_version: impl Into<String>,
        agent_id: impl Into<String>,
        data_dir: Option<String>,
        static_config: BTreeMap<String, Value>,
    ) -> Self {
        let plugin_id = ipc.plugin_id().to_string();
        let peripheral_manager = PeripheralClient { ipc: ipc.clone() };
        Self {
            plugin_id,
            plugin_version: plugin_version.into(),
            agent_id: agent_id.into(),
            data_dir: data_dir.map(std::path::PathBuf::from),
            events: EventsClient { ipc: ipc.clone() },
            mavlink: MavlinkClient { ipc: ipc.clone() },
            msp: MspClient { ipc: ipc.clone() },
            telemetry: TelemetryClient { ipc: ipc.clone() },
            peripherals: peripheral_manager.clone(),
            peripheral_manager,
            camera: CameraClient { ipc: ipc.clone() },
            vision: VisionClient::new(ipc.clone()),
            buttons: ButtonClient { ipc: ipc.clone() },
            flight: FlightClient { ipc: ipc.clone() },
            gpio: GpioClient { ipc: ipc.clone() },
            display: DisplayClient { ipc: ipc.clone() },
            radio: RadioClient { ipc: ipc.clone() },
            config: ConfigClient {
                ipc: ipc.clone(),
                static_config: Arc::new(static_config),
            },
            process: ProcessClient { ipc: ipc.clone() },
            lifecycle: LifecycleClient { ipc: ipc.clone() },
            ipc,
        }
    }

    /// Health probe against the host. Mirrors `ping_supervisor`.
    pub async fn ping_supervisor(&self) -> Result<Value, ClientError> {
        self.ipc.ping().await
    }

    /// The shared client, for advanced callers that need a method not yet on a
    /// facade.
    pub fn client(&self) -> &Arc<PluginIpcClient> {
        &self.ipc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_config_is_read_synchronously() {
        let ipc = Arc::new(PluginIpcClient::new(
            "com.example.demo",
            "tok",
            "/tmp/x.sock",
        ));
        let mut cfg = BTreeMap::new();
        cfg.insert("palette".to_string(), Value::from("ironbow"));
        let ctx = PluginContext::new(ipc, "1.0.0", "agent-1", None, cfg);
        assert_eq!(
            ctx.config.static_get("palette"),
            Some(&Value::from("ironbow"))
        );
        assert!(ctx.config.static_get("missing").is_none());
        // peripherals is the same client as peripheral_manager.
        assert_eq!(ctx.plugin_id, "com.example.demo");
        assert_eq!(ctx.agent_id, "agent-1");
        assert_eq!(ctx.plugin_version, "1.0.0");
    }

    /// `send_sticks` must apply the codec scaling and AETR order. The bug it
    /// fixes is a control loop handing `send_raw_rc` normalized sticks, where a
    /// centre stick (`0.0`) would land at PWM `0` — off the bottom of the range —
    /// instead of `1500`. This asserts the scaling is actually applied.
    #[test]
    fn send_sticks_scales_and_orders_aetr() {
        // Centre sticks + idle throttle: NOT zeros. AETR = [roll, pitch, thr, yaw].
        assert_eq!(sticks_to_channels(0.0, 0.0, 0.0, 0.0), [1500, 1500, 1000, 1500]);
        // Full deflection each axis + full throttle, exercising the ordering.
        assert_eq!(
            sticks_to_channels(1.0, -1.0, 1.0, 1.0),
            [2000, 1000, 2000, 2000]
        );
        // Out-of-range clamps rather than wrapping.
        assert_eq!(sticks_to_channels(5.0, -5.0, 0.0, 2.0), [2000, 1000, 2000, 1500]);
    }
}
