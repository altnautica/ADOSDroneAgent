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
    pub events: EventsClient,
    pub mavlink: MavlinkClient,
    pub telemetry: TelemetryClient,
    pub peripheral_manager: PeripheralClient,
    /// Alias for `peripheral_manager` (Python `ctx.peripherals`).
    pub peripherals: PeripheralClient,
    pub camera: CameraClient,
    pub config: ConfigClient,
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
        static_config: BTreeMap<String, Value>,
    ) -> Self {
        let plugin_id = ipc.plugin_id().to_string();
        let peripheral_manager = PeripheralClient { ipc: ipc.clone() };
        Self {
            plugin_id,
            plugin_version: plugin_version.into(),
            agent_id: agent_id.into(),
            events: EventsClient { ipc: ipc.clone() },
            mavlink: MavlinkClient { ipc: ipc.clone() },
            telemetry: TelemetryClient { ipc: ipc.clone() },
            peripherals: peripheral_manager.clone(),
            peripheral_manager,
            camera: CameraClient { ipc: ipc.clone() },
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
        let ctx = PluginContext::new(ipc, "1.0.0", "agent-1", cfg);
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
}
