//! The real [`HostServices`] implementation.
//!
//! Ports `src/ados/plugins/ipc/host_services.py` (the five in-memory facades:
//! component registrar, telemetry extender, driver registry, camera claim
//! tracker, config store) plus the twelve real handler bodies in
//! `src/ados/plugins/ipc/handlers.py`. Behaviour is reproduced exactly: the
//! same argument validation, the same inline capability gates, the same error
//! strings, and the same success-map shapes.
//!
//! Raise-vs-return mapping (read carefully — it is load-bearing):
//! * A Python `raise _RpcError(m)` becomes `Err(HostError::Rpc(m))`.
//! * A Python `raise CapabilityDenied(pid, cap)` becomes
//!   `Err(HostError::CapabilityDenied(cap))`.
//! * A Python `raise AllowlistViolation(pid, basename)` becomes
//!   `Err(HostError::AllowlistViolation(basename))`.
//! * A Python handler that *returns* a dict with an `"error"` key (the
//!   `not_available` / `send_failed` paths) becomes `Ok(<that map>)`, NOT an
//!   `Err`. Those are graceful-degrade responses, not gate failures.
//!
//! One [`Arc<RealHost>`] is shared across every per-plugin accept task, so every
//! facade is behind a [`std::sync::Mutex`]. The host methods are synchronous
//! (they never `.await`), so a std mutex is correct and is never held across an
//! await point.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmpv::Value;
use tokio::sync::broadcast;

use crate::host::{HostError, HostResult, HostServices};
use crate::mavlink_client::MavlinkClient;

// ---------------------------------------------------------------------
// MAVLink classification constants (host_services.py)
// ---------------------------------------------------------------------

/// Message ids the pose-injection path covers. A send whose msg id is in this
/// set demands `estimator.pose.inject` on top of `mavlink.write`. Mirrors
/// `POSE_INJECT_MSG_IDS`.
pub const POSE_INJECT_MSG_IDS: &[u32] = &[
    331,   // ODOMETRY
    102,   // VISION_POSITION_ESTIMATE
    11011, // VISION_POSITION_DELTA
    104,   // VICON_POSITION_ESTIMATE
    138,   // ATT_POS_MOCAP (vicon-equivalent attitude path)
];

/// Component ids the VIO permission covers. Registering one of these requires
/// `mavlink.component.vio` on top of the matching component kind. Mirrors
/// `VIO_COMPONENT_IDS`.
pub const VIO_COMPONENT_IDS: &[i64] = &[197, 198];

/// Driver kind -> required capability. Mirrors `_DRIVER_KIND_TO_CAP`.
fn driver_kind_to_cap(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "camera" => "sensor.camera.register",
        "depth" => "sensor.depth.register",
        "lidar" => "sensor.lidar.register",
        "imu" => "sensor.imu.register",
        "payload" => "sensor.payload.register",
        // gimbal / gps / esc / payload-actuator reuse the payload register cap
        // (no dedicated sensor.* permission in the current catalog).
        "gimbal" | "gps" | "esc" | "payload-actuator" => "sensor.payload.register",
        _ => return None,
    })
}

/// Supported camera frame formats. Mirrors `_SUPPORTED_CAMERA_FORMATS`.
const SUPPORTED_CAMERA_FORMATS: &[&str] = &["nv12", "rgb888", "yuv420p"];

// ---------------------------------------------------------------------
// Component registrar
// ---------------------------------------------------------------------

/// One component-id reservation. Mirrors `ComponentRegistration`.
#[derive(Debug, Clone)]
struct ComponentRegistration {
    plugin_id: String,
    component_id: i64,
    kind: String,
}

/// Tracks per-plugin MAVLink component-id reservations. Mirrors
/// `ComponentRegistrar`.
#[derive(Default)]
struct ComponentRegistrar {
    by_plugin: BTreeMap<String, BTreeMap<i64, ComponentRegistration>>,
    by_component_id: BTreeMap<i64, ComponentRegistration>,
}

impl ComponentRegistrar {
    /// Reserve `comp_id` for `plugin_id`. Refuses a reservation another plugin
    /// already holds, with the exact cross-plugin collision message.
    fn register(
        &mut self,
        plugin_id: &str,
        comp_id: i64,
        kind: &str,
    ) -> Result<ComponentRegistration, String> {
        if let Some(existing) = self.by_component_id.get(&comp_id) {
            if existing.plugin_id != plugin_id {
                return Err(format!(
                    "component_id {comp_id} already reserved by {}",
                    existing.plugin_id
                ));
            }
        }
        let reg = ComponentRegistration {
            plugin_id: plugin_id.to_string(),
            component_id: comp_id,
            kind: kind.to_string(),
        };
        self.by_plugin
            .entry(plugin_id.to_string())
            .or_default()
            .insert(comp_id, reg.clone());
        self.by_component_id.insert(comp_id, reg.clone());
        Ok(reg)
    }

    fn is_registered(&self, plugin_id: &str, comp_id: i64) -> bool {
        self.by_plugin
            .get(plugin_id)
            .is_some_and(|m| m.contains_key(&comp_id))
    }

    fn release_plugin(&mut self, plugin_id: &str) {
        if let Some(comps) = self.by_plugin.remove(plugin_id) {
            for comp_id in comps.keys() {
                self.by_component_id.remove(comp_id);
            }
        }
    }
}

// ---------------------------------------------------------------------
// Telemetry extender
// ---------------------------------------------------------------------

/// Stores per-plugin telemetry channel payloads. Channel keys are namespaced
/// `<plugin_id>/<channel>`. Mirrors `TelemetryExtender`.
#[derive(Default)]
struct TelemetryExtender {
    channels: BTreeMap<String, Value>,
}

impl TelemetryExtender {
    /// Store a defensive copy of `payload` under `<plugin_id>/<channel>`. The
    /// channel must be a non-empty string (validated upstream, re-checked here
    /// to match the facade contract).
    fn extend(&mut self, plugin_id: &str, channel: &str, payload: Value) -> Result<(), String> {
        if channel.is_empty() {
            return Err("channel must be a non-empty string".to_string());
        }
        let key = format!("{plugin_id}/{channel}");
        // `payload` is already an owned clone of the request arg, so storing it
        // is the defensive copy the Python facade makes with `dict(payload)`.
        self.channels.insert(key, payload);
        Ok(())
    }

    fn clear_plugin(&mut self, plugin_id: &str) {
        let prefix = format!("{plugin_id}/");
        self.channels.retain(|k, _| !k.starts_with(&prefix));
    }

    /// A defensive copy of the channel map. Mirrors `snapshot()`.
    fn snapshot(&self) -> BTreeMap<String, Value> {
        self.channels.clone()
    }
}

// ---------------------------------------------------------------------
// Driver registry
// ---------------------------------------------------------------------

/// One driver registration handle. Mirrors `DriverHandle`.
#[derive(Debug, Clone)]
struct DriverHandle {
    plugin_id: String,
    handle_id: String,
}

/// Generic driver registry for every driver kind. Mirrors `DriverRegistry`
/// (the installer/uninstaller callables are not wired in this in-memory host;
/// the production agent hands the driver to the owning manager out of band).
#[derive(Default)]
struct DriverRegistry {
    handles: BTreeMap<String, DriverHandle>,
    counter: u64,
}

impl DriverRegistry {
    /// Register a driver and return its handle id, formatted exactly as the
    /// Python facade: `<kind>-<plugin_id>-<counter>`.
    fn register(&mut self, plugin_id: &str, kind: &str) -> DriverHandle {
        self.counter += 1;
        let handle_id = format!("{kind}-{plugin_id}-{}", self.counter);
        let h = DriverHandle {
            plugin_id: plugin_id.to_string(),
            handle_id: handle_id.clone(),
        };
        self.handles.insert(handle_id, h.clone());
        h
    }

    fn unregister(&mut self, handle_id: &str) {
        self.handles.remove(handle_id);
    }

    fn release_plugin(&mut self, plugin_id: &str) {
        self.handles.retain(|_, h| h.plugin_id != plugin_id);
    }
}

// ---------------------------------------------------------------------
// Camera claim tracker
// ---------------------------------------------------------------------

/// One camera claim. Mirrors `CameraClaim`.
#[derive(Debug, Clone)]
struct CameraClaim {
    plugin_id: String,
    device_path: String,
    exclusive: bool,
}

/// The latest captured frame for a claimed path. Mirrors `CameraFrame`; only a
/// test harness populates it in this in-memory host.
#[derive(Debug, Clone)]
pub struct CameraFrame {
    pub frame_id: i64,
    pub width: i64,
    pub height: i64,
    pub format: String,
    pub data: Vec<u8>,
    pub ts_ns: i64,
}

/// Records per-plugin exclusive holds on a `/dev/videoN` path plus the cached
/// latest frame. Mirrors `CameraClaimTracker`.
#[derive(Default)]
struct CameraClaimTracker {
    claims: BTreeMap<String, CameraClaim>,
    frames: BTreeMap<String, CameraFrame>,
}

impl CameraClaimTracker {
    /// Claim `device_path`. Refuses when another plugin holds it exclusively,
    /// with the exact exclusive-hold message.
    fn claim(
        &mut self,
        plugin_id: &str,
        device_path: &str,
        exclusive: bool,
    ) -> Result<CameraClaim, String> {
        if let Some(prior) = self.claims.get(device_path) {
            if prior.exclusive && prior.plugin_id != plugin_id {
                return Err(format!(
                    "camera {device_path} is exclusively held by {}",
                    prior.plugin_id
                ));
            }
        }
        let claim = CameraClaim {
            plugin_id: plugin_id.to_string(),
            device_path: device_path.to_string(),
            exclusive,
        };
        self.claims.insert(device_path.to_string(), claim.clone());
        Ok(claim)
    }

    /// Release a single path. No-op when the plugin does not hold the path;
    /// errors only when another plugin holds it. Drops the cached frame so a
    /// stale buffer can't leak to the next claimant.
    fn release(&mut self, plugin_id: &str, device_path: &str) -> Result<(), String> {
        let prior = match self.claims.get(device_path) {
            Some(p) => p,
            None => return Ok(()),
        };
        if prior.plugin_id != plugin_id {
            return Err(format!(
                "camera {device_path} is held by {}, not {plugin_id}",
                prior.plugin_id
            ));
        }
        self.claims.remove(device_path);
        self.frames.remove(device_path);
        Ok(())
    }

    fn release_plugin(&mut self, plugin_id: &str) {
        let paths: Vec<String> = self
            .claims
            .iter()
            .filter(|(_, c)| c.plugin_id == plugin_id)
            .map(|(p, _)| p.clone())
            .collect();
        for path in paths {
            self.claims.remove(&path);
            self.frames.remove(&path);
        }
    }

    fn holder(&self, device_path: &str) -> Option<String> {
        self.claims.get(device_path).map(|c| c.plugin_id.clone())
    }

    /// Stash the latest frame for a path (test/capture-pipeline only).
    fn publish_frame(&mut self, device_path: &str, frame: CameraFrame) {
        self.frames.insert(device_path.to_string(), frame);
    }

    fn latest_frame(&self, device_path: &str) -> Option<CameraFrame> {
        self.frames.get(device_path).cloned()
    }
}

// ---------------------------------------------------------------------
// Config store
// ---------------------------------------------------------------------

/// In-memory per-scope config store. Reads consult drone scope first, then
/// global, then the request default. Mirrors `ConfigStore`. The `_MISSING`
/// sentinel of the Python store is expressed here as `Option<Value>`: a stored
/// `nil` is `Some(Value::Nil)` (a present value) and is distinct from absent
/// (`None`), so a key explicitly set to nil shadows global and default exactly
/// as the Python sentinel does.
#[derive(Default)]
struct ConfigStore {
    drone: BTreeMap<(String, String, String), Value>,
    global: BTreeMap<(String, String), Value>,
}

impl ConfigStore {
    fn get(&self, plugin_id: &str, key: &str, agent_id: &str, default: Value) -> Value {
        if !agent_id.is_empty() {
            if let Some(v) =
                self.drone
                    .get(&(plugin_id.to_string(), agent_id.to_string(), key.to_string()))
            {
                return v.clone();
            }
        }
        if let Some(v) = self.global.get(&(plugin_id.to_string(), key.to_string())) {
            return v.clone();
        }
        default
    }

    fn set(&mut self, plugin_id: &str, key: &str, value: Value, scope: &str, agent_id: &str) {
        // drone scope with no bound agent degrades to global, matching the
        // Python store.
        let effective_scope = if scope == "drone" && agent_id.is_empty() {
            "global"
        } else {
            scope
        };
        if effective_scope == "drone" {
            self.drone.insert(
                (plugin_id.to_string(), agent_id.to_string(), key.to_string()),
                value,
            );
        } else {
            self.global
                .insert((plugin_id.to_string(), key.to_string()), value);
        }
    }
}

// ---------------------------------------------------------------------
// MAVLink frame classification
// ---------------------------------------------------------------------

/// Best-effort MAVLink message id from a raw frame. Returns `None` when the
/// frame is too short to classify. Mirrors `_mavlink_msg_id`.
fn mavlink_msg_id(frame: &[u8]) -> Option<u32> {
    let stx = *frame.first()?;
    if stx == 0xFD && frame.len() >= 10 {
        // v2: bytes 7..10 little-endian 24-bit msgid.
        let mut id = [0u8; 4];
        id[..3].copy_from_slice(&frame[7..10]);
        return Some(u32::from_le_bytes(id));
    }
    if stx == 0xFE && frame.len() >= 6 {
        // v1: byte 5 is the 8-bit msgid.
        return Some(frame[5] as u32);
    }
    None
}

// ---------------------------------------------------------------------
// rmpv arg helpers
// ---------------------------------------------------------------------

fn map_get<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

fn map_has(args: &Value, key: &str) -> bool {
    matches!(args, Value::Map(entries) if entries.iter().any(|(k, _)| k.as_str() == Some(key)))
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    map_get(args, key).and_then(Value::as_str)
}

/// `env.args.get(key)` coerced to a clone, or `Value::Nil` when absent.
fn arg_owned(args: &Value, key: &str) -> Value {
    map_get(args, key).cloned().unwrap_or(Value::Nil)
}

/// Coerce a msgpack value to raw bytes, mirroring the Python `msg_bytes`
/// handling: a binary value is taken verbatim; a list of ints is coerced to
/// bytes (msgpack may decode bytes-of-ints as a list on some configs). Any other
/// type (including a string) is rejected, matching the Python
/// `isinstance(msg_bytes, (bytes, bytearray))` check. Returns the coerced bytes,
/// or an `Err` string when the type is wrong / the list coercion fails. The
/// error text is the fixed wire string the Python handler emits.
fn coerce_msg_bytes(value: &Value) -> Result<Vec<u8>, String> {
    match value {
        Value::Binary(b) => Ok(b.clone()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_u64() {
                    Some(n) if n <= 255 => out.push(n as u8),
                    _ => return Err("msg_bytes coercion failed".to_string()),
                }
            }
            Ok(out)
        }
        _ => Err("msg_bytes must be bytes".to_string()),
    }
}

/// Coerce a msgpack value to an i64 component id, mirroring the Python
/// `int(component_id)`: an integer is taken directly, and a numeric string is
/// trimmed and parsed (`int("197")`). Any other value yields the fixed
/// `"component_id not integer"` wire string the Python handler emits.
fn coerce_component_id(value: &Value) -> Result<i64, String> {
    if let Some(n) = value.as_i64() {
        return Ok(n);
    }
    if let Some(n) = value.as_u64() {
        if let Ok(n) = i64::try_from(n) {
            return Ok(n);
        }
    }
    // Python `int("197")` parses a numeric string (after stripping surrounding
    // whitespace); a non-numeric string raises ValueError -> the fixed message.
    if let Some(s) = value.as_str() {
        if let Ok(n) = s.trim().parse::<i64>() {
            return Ok(n);
        }
    }
    Err("component_id not integer".to_string())
}

// ---------------------------------------------------------------------
// RealHost
// ---------------------------------------------------------------------

/// Resolves a plugin id to its `(install_dir, subprocess_spawn allowlist)`.
pub type RuntimeLookup = Box<dyn Fn(&str) -> Option<(PathBuf, BTreeSet<String>)> + Send + Sync>;

/// Resolves a plugin id to its bound agent identity (empty when unbound).
pub type AgentIdLookup = Box<dyn Fn(&str) -> String + Send + Sync>;

/// The real host: the five in-memory facades, the optional MAVLink client, and
/// the two runtime lookups. Mirrors the `HostServices` dataclass and
/// `default_host_services()` (every external slot starts `None`).
pub struct RealHost {
    components: Mutex<ComponentRegistrar>,
    telemetry: Mutex<TelemetryExtender>,
    drivers: Mutex<DriverRegistry>,
    cameras: Mutex<CameraClaimTracker>,
    config: Mutex<ConfigStore>,
    mavlink: Option<Arc<MavlinkClient>>,
    plugin_runtime_lookup: Option<RuntimeLookup>,
    agent_id_lookup: Option<AgentIdLookup>,
}

impl RealHost {
    /// A host with empty facades and every external slot unwired, matching
    /// `default_host_services()`.
    pub fn new() -> Self {
        Self {
            components: Mutex::new(ComponentRegistrar::default()),
            telemetry: Mutex::new(TelemetryExtender::default()),
            drivers: Mutex::new(DriverRegistry::default()),
            cameras: Mutex::new(CameraClaimTracker::default()),
            config: Mutex::new(ConfigStore::default()),
            mavlink: None,
            plugin_runtime_lookup: None,
            agent_id_lookup: None,
        }
    }

    /// Wire the MAVLink client (builder style).
    pub fn with_mavlink(mut self, mavlink: Arc<MavlinkClient>) -> Self {
        self.mavlink = Some(mavlink);
        self
    }

    /// Wire the plugin runtime lookup (builder style).
    pub fn with_runtime_lookup(mut self, lookup: RuntimeLookup) -> Self {
        self.plugin_runtime_lookup = Some(lookup);
        self
    }

    /// Wire the agent-id lookup (builder style).
    pub fn with_agent_id_lookup(mut self, lookup: AgentIdLookup) -> Self {
        self.agent_id_lookup = Some(lookup);
        self
    }

    /// Resolve the agent id for a plugin, swallowing lookup errors to "" exactly
    /// as `_agent_id_for`. (The Rust lookup cannot raise, so a `None` host slot
    /// is the only "" path; an empty string returned by the closure stays "".)
    fn agent_id_for(&self, plugin_id: &str) -> String {
        match &self.agent_id_lookup {
            Some(lookup) => lookup(plugin_id),
            None => String::new(),
        }
    }

    /// A snapshot of the telemetry channels the heartbeat builder reads. Mirrors
    /// `TelemetryExtender.snapshot()` reached through `host.telemetry`.
    pub fn telemetry_snapshot(&self) -> BTreeMap<String, Value> {
        self.telemetry
            .lock()
            .expect("telemetry mutex poisoned")
            .snapshot()
    }

    /// Stash a camera frame for a path (the capture pipeline / test harness
    /// surface). Mirrors `CameraClaimTracker.publish_frame`.
    pub fn publish_camera_frame(&self, device_path: &str, frame: CameraFrame) {
        self.cameras
            .lock()
            .expect("cameras mutex poisoned")
            .publish_frame(device_path, frame);
    }
}

impl Default for RealHost {
    fn default() -> Self {
        Self::new()
    }
}

impl HostServices for RealHost {
    fn telemetry_extend(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let channel = arg_str(args, "channel").filter(|c| !c.is_empty());
        let Some(channel) = channel else {
            return Err(HostError::Rpc(
                "channel must be a non-empty string".to_string(),
            ));
        };
        // payload defaults to an empty map; a non-map payload is an _RpcError.
        let payload = match map_get(args, "payload") {
            None | Some(Value::Nil) => Value::Map(vec![]),
            Some(v @ Value::Map(_)) => v.clone(),
            Some(_) => return Err(HostError::Rpc("payload must be a mapping".to_string())),
        };
        self.telemetry
            .lock()
            .expect("telemetry mutex poisoned")
            .extend(plugin_id, channel, payload)
            .map_err(HostError::Rpc)?;
        Ok(Value::Map(vec![
            (Value::from("merged"), Value::Boolean(true)),
            (Value::from("channel"), Value::from(channel)),
        ]))
    }

    fn mavlink_send(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        // Mirrors `handle_mavlink_send` in source order: argument validation,
        // then the pose-inject capability gate, then the component-id VIO gate +
        // reservation check, then the router-None / send. The inline gates run
        // INSIDE the handler, after validation, so a malformed request fails
        // validation before any capability check.

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

        // 2. Pose-inject gate: rejects ungranted callers regardless of the
        //    dispatch-level mavlink.write.
        if let Some(id) = mavlink_msg_id(&msg_bytes) {
            if POSE_INJECT_MSG_IDS.contains(&id) && !granted_caps.contains("estimator.pose.inject")
            {
                return Err(HostError::CapabilityDenied(
                    "estimator.pose.inject".to_string(),
                ));
            }
        }

        // 3. Component-id: VIO cap gate, then reservation check.
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
            }
        }

        match &self.mavlink {
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("mavlink.send")),
            ])),
            Some(client) => {
                // Best-effort send; failures are swallowed by send_bytes, so the
                // success shape stands, matching the Python slice.
                client.send_bytes(&msg_bytes);
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

    fn mavlink_register_component(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        // Mirrors `handle_mavlink_register_component` in source order: validate
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
            .register(plugin_id, comp_id, kind)
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

    fn peripheral_register_driver(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        // Mirrors `handle_peripheral_register_driver` in source order: validate
        // kind, validate driver_ref, resolve the kind's required cap (unknown
        // kind is an _RpcError), then the cap gate, then register.
        let kind = arg_str(args, "kind").filter(|k| !k.is_empty());
        let Some(kind) = kind else {
            return Err(HostError::Rpc(
                "kind must be a non-empty string".to_string(),
            ));
        };
        let driver_ref = arg_str(args, "driver_ref").filter(|r| !r.is_empty());
        if driver_ref.is_none() {
            return Err(HostError::Rpc(
                "driver_ref must be a non-empty string".to_string(),
            ));
        }
        let Some(required) = driver_kind_to_cap(kind) else {
            return Err(HostError::Rpc(format!("unknown driver kind: {kind}")));
        };
        if !granted_caps.contains(required) {
            return Err(HostError::CapabilityDenied(required.to_string()));
        }
        let handle = self
            .drivers
            .lock()
            .expect("drivers mutex poisoned")
            .register(plugin_id, kind);
        Ok(Value::Map(vec![
            (Value::from("registered"), Value::Boolean(true)),
            (Value::from("kind"), Value::from(kind)),
            (
                Value::from("handle_id"),
                Value::from(handle.handle_id.as_str()),
            ),
        ]))
    }

    fn peripheral_unregister_driver(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let handle_id = arg_str(args, "handle_id").filter(|h| !h.is_empty());
        let Some(handle_id) = handle_id else {
            return Err(HostError::Rpc(
                "handle_id must be a non-empty string".to_string(),
            ));
        };
        self.drivers
            .lock()
            .expect("drivers mutex poisoned")
            .unregister(handle_id);
        Ok(Value::Map(vec![(
            Value::from("unregistered"),
            Value::Boolean(true),
        )]))
    }

    fn camera_claim(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let device_path = arg_str(args, "device_path").filter(|p| !p.is_empty());
        let Some(device_path) = device_path else {
            return Err(HostError::Rpc(
                "device_path must be a non-empty string".to_string(),
            ));
        };
        // exclusive defaults to True; bool(...) coerces a present value.
        let exclusive = match map_get(args, "exclusive") {
            None | Some(Value::Nil) => true,
            Some(v) => python_bool(v),
        };
        let claim = self
            .cameras
            .lock()
            .expect("cameras mutex poisoned")
            .claim(plugin_id, device_path, exclusive)
            .map_err(HostError::Rpc)?;
        Ok(Value::Map(vec![
            (Value::from("claimed"), Value::Boolean(true)),
            (
                Value::from("device_path"),
                Value::from(claim.device_path.as_str()),
            ),
            (Value::from("exclusive"), Value::Boolean(claim.exclusive)),
        ]))
    }

    fn camera_release(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let device_path = arg_str(args, "device_path").filter(|p| !p.is_empty());
        let Some(device_path) = device_path else {
            return Err(HostError::Rpc(
                "device_path must be a non-empty string".to_string(),
            ));
        };
        self.cameras
            .lock()
            .expect("cameras mutex poisoned")
            .release(plugin_id, device_path)
            .map_err(HostError::Rpc)?;
        Ok(Value::Map(vec![
            (Value::from("released"), Value::Boolean(true)),
            (Value::from("device_path"), Value::from(device_path)),
        ]))
    }

    fn camera_get_frame(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let device_path = arg_str(args, "device_path").filter(|p| !p.is_empty());
        let Some(device_path) = device_path else {
            return Err(HostError::Rpc(
                "device_path must be a non-empty string".to_string(),
            ));
        };
        // format defaults to "nv12".
        let fmt = match map_get(args, "format") {
            None | Some(Value::Nil) => "nv12",
            Some(Value::String(s)) => s.as_str().unwrap_or(""),
            Some(_) => "",
        };
        if !SUPPORTED_CAMERA_FORMATS.contains(&fmt) {
            return Err(HostError::Rpc(format!(
                "format {} not supported; pick one of {:?}",
                py_repr(fmt),
                sorted_formats()
            )));
        }
        // timeout_ms defaults to 1000; must coerce to an int >= 0.
        let timeout_value = match map_get(args, "timeout_ms") {
            None | Some(Value::Nil) => Value::Integer(1000.into()),
            Some(v) => v.clone(),
        };
        let timeout_ms = match coerce_timeout_ms(&timeout_value) {
            Some(n) => n,
            None => return Err(HostError::Rpc("timeout_ms must be an integer".to_string())),
        };
        if timeout_ms < 0 {
            return Err(HostError::Rpc("timeout_ms must be >= 0".to_string()));
        }

        let cameras = self.cameras.lock().expect("cameras mutex poisoned");
        let holder = cameras.holder(device_path);
        match holder {
            None => {
                return Err(HostError::Rpc(format!(
                    "camera {device_path} is not claimed; call camera.claim first"
                )))
            }
            Some(h) if h != plugin_id => {
                return Err(HostError::Rpc(format!(
                    "camera {device_path} is held by another plugin ({h})"
                )))
            }
            Some(_) => {}
        }

        let frame = cameras.latest_frame(device_path);
        let Some(frame) = frame else {
            return Err(HostError::Rpc(format!(
                "no frame available for {device_path}; capture pipeline has not \
                 produced a buffer yet"
            )));
        };
        if frame.format != fmt {
            return Err(HostError::Rpc(format!(
                "frame format mismatch: pipeline produced {}, plugin requested {}",
                py_repr(&frame.format),
                py_repr(fmt)
            )));
        }
        Ok(Value::Map(vec![
            (
                Value::from("frame_id"),
                Value::Integer(frame.frame_id.into()),
            ),
            (Value::from("width"), Value::Integer(frame.width.into())),
            (Value::from("height"), Value::Integer(frame.height.into())),
            (Value::from("format"), Value::from(frame.format.as_str())),
            (Value::from("data"), Value::Binary(frame.data.clone())),
            (Value::from("ts_ns"), Value::Integer(frame.ts_ns.into())),
            (Value::from("stale"), Value::Boolean(false)),
        ]))
    }

    fn config_get(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let key = arg_str(args, "key").filter(|k| !k.is_empty());
        let Some(key) = key else {
            return Err(HostError::Rpc("key must be a non-empty string".to_string()));
        };
        let default = arg_owned(args, "default");
        let agent_id = self.agent_id_for(plugin_id);
        let value = self
            .config
            .lock()
            .expect("config mutex poisoned")
            .get(plugin_id, key, &agent_id, default);
        Ok(Value::Map(vec![(Value::from("value"), value)]))
    }

    fn config_set(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let key = arg_str(args, "key").filter(|k| !k.is_empty());
        let Some(key) = key else {
            return Err(HostError::Rpc("key must be a non-empty string".to_string()));
        };
        if !map_has(args, "value") {
            return Err(HostError::Rpc("value missing".to_string()));
        }
        // Mirrors Python `scope = args.get("scope") or "drone"`: any falsy value
        // (nil, empty string, 0, 0.0, false, empty array/map, absent) coerces to
        // "drone". Only a truthy value that is neither drone nor global errors.
        let scope_arg = map_get(args, "scope");
        let scope = match scope_arg {
            None => "drone",
            Some(v) if !python_bool(v) => "drone",
            Some(Value::String(s)) => s.as_str().unwrap_or("drone"),
            // A truthy non-string scope (e.g. a non-empty array) is neither
            // drone nor global, so it errors with the repr of the arg.
            Some(other) => {
                return Err(HostError::Rpc(format!(
                    "scope must be drone or global, got {}",
                    py_repr_value(other)
                )))
            }
        };
        if scope != "drone" && scope != "global" {
            return Err(HostError::Rpc(format!(
                "scope must be drone or global, got {}",
                py_repr(scope)
            )));
        }
        let value = arg_owned(args, "value");
        let agent_id = self.agent_id_for(plugin_id);
        self.config
            .lock()
            .expect("config mutex poisoned")
            .set(plugin_id, key, value, scope, &agent_id);
        Ok(Value::Map(vec![
            (Value::from("set"), Value::Boolean(true)),
            (Value::from("scope"), Value::from(scope)),
        ]))
    }

    fn process_spawn(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let basename = arg_str(args, "basename").filter(|b| !b.is_empty());
        let Some(basename) = basename else {
            return Err(HostError::Rpc(
                "basename must be a non-empty string".to_string(),
            ));
        };
        // args defaults to []; must be a list. env defaults to {}; must be a map.
        let spawn_args = match map_get(args, "args") {
            None | Some(Value::Nil) => Value::Array(vec![]),
            Some(v @ Value::Array(_)) => v.clone(),
            Some(_) => return Err(HostError::Rpc("args must be a list of strings".to_string())),
        };
        let spawn_env = match map_get(args, "env") {
            None | Some(Value::Nil) => Value::Map(vec![]),
            Some(v @ Value::Map(_)) => v.clone(),
            Some(_) => return Err(HostError::Rpc("env must be a mapping".to_string())),
        };

        let lookup = match &self.plugin_runtime_lookup {
            None => {
                return Ok(Value::Map(vec![
                    (Value::from("error"), Value::from("not_available")),
                    (Value::from("method"), Value::from("process.spawn")),
                ]))
            }
            Some(lookup) => lookup,
        };
        // A missing registration mirrors the Python KeyError path:
        // {"error": "not_available", "method": "process.spawn",
        //  "reason": "plugin runtime not registered"}.
        let Some((install_dir, allowlist)) = lookup(plugin_id) else {
            return Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("process.spawn")),
                (
                    Value::from("reason"),
                    Value::from("plugin runtime not registered"),
                ),
            ]));
        };

        if !allowlist.contains(basename) {
            tracing::warn!(
                plugin_id = %plugin_id,
                basename = %basename,
                allowlist_size = allowlist.len(),
                "plugin process spawn denied"
            );
            return Err(HostError::AllowlistViolation(basename.to_string()));
        }

        tracing::info!(
            plugin_id = %plugin_id,
            basename = %basename,
            "plugin process spawn authorized"
        );

        Ok(Value::Map(vec![
            (Value::from("authorized"), Value::Boolean(true)),
            (
                Value::from("install_dir"),
                Value::from(install_dir.to_string_lossy().as_ref()),
            ),
            (Value::from("basename"), Value::from(basename)),
            (Value::from("args"), spawn_args),
            (Value::from("env"), spawn_env),
        ]))
    }

    fn release_plugin(&self, plugin_id: &str) {
        // Mirror _release_session_resources: components + drivers + cameras +
        // telemetry are cleared. The config store is deliberately NOT cleared
        // (it persists across reconnects in the Python facade too).
        self.components
            .lock()
            .expect("components mutex poisoned")
            .release_plugin(plugin_id);
        self.drivers
            .lock()
            .expect("drivers mutex poisoned")
            .release_plugin(plugin_id);
        self.cameras
            .lock()
            .expect("cameras mutex poisoned")
            .release_plugin(plugin_id);
        self.telemetry
            .lock()
            .expect("telemetry mutex poisoned")
            .clear_plugin(plugin_id);
    }

    fn mavlink_subscribe_stream(
        &self,
        _plugin_id: &str,
        _msg_name: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.mavlink.as_ref().map(|c| c.subscribe())
    }
}

// ---------------------------------------------------------------------
// Python-compat formatting helpers
// ---------------------------------------------------------------------

/// `bool(value)` truthiness, matching Python's coercion of the camera
/// `exclusive` arg. Empty containers / zero / nil / false are falsy.
fn python_bool(value: &Value) -> bool {
    match value {
        Value::Nil => false,
        Value::Boolean(b) => *b,
        Value::Integer(i) => i.as_i64().map(|n| n != 0).unwrap_or(true),
        Value::F32(f) => *f != 0.0,
        Value::F64(f) => *f != 0.0,
        Value::String(s) => !s.as_str().map(str::is_empty).unwrap_or(true),
        Value::Binary(b) => !b.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Map(m) => !m.is_empty(),
        Value::Ext(_, b) => !b.is_empty(),
    }
}

/// `int(value)` coercion for timeout_ms: an integer (or integral float) yields
/// `Some`; anything else yields `None` (the Python `(TypeError, ValueError)`
/// branch). A float truncates toward zero like Python `int(float)`.
fn coerce_timeout_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(i) => i
            .as_i64()
            .or_else(|| i.as_u64().and_then(|n| i64::try_from(n).ok())),
        Value::F32(f) => Some(*f as i64),
        Value::F64(f) => Some(*f as i64),
        Value::Boolean(b) => Some(*b as i64),
        _ => None,
    }
}

/// Python `repr()` of a string: single-quoted. Used in the few error strings
/// that interpolate a value with `{x!r}` so the wire body matches byte-for-byte.
fn py_repr(s: &str) -> String {
    format!("'{s}'")
}

/// Python `repr()` of an rmpv value where the handler used `{scope!r}` on a
/// non-string scope arg. Strings are single-quoted; containers recurse so inner
/// strings are single-quoted too (`repr(['x'])` == `['x']`, `repr({'a': 1})` ==
/// `{'a': 1}`). Only reached for a truthy non-string scope, which is an exotic
/// error path; the common case is a plain string.
fn py_repr_value(value: &Value) -> String {
    match value {
        Value::String(s) => py_repr(s.as_str().unwrap_or("")),
        Value::Nil => "None".to_string(),
        Value::Boolean(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::F32(f) => f.to_string(),
        Value::F64(f) => f.to_string(),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_repr_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Map(entries) => {
            let inner: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr_value(k), py_repr_value(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        other => format!("{other}"),
    }
}

/// `sorted(_SUPPORTED_CAMERA_FORMATS)` — the formats in sorted order for the
/// not-supported error string.
fn sorted_formats() -> Vec<&'static str> {
    let mut v = SUPPORTED_CAMERA_FORMATS.to_vec();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| (Value::from(*k), v.clone()))
                .collect(),
        )
    }

    fn err_body(r: Result<HostResult, HostError>) -> String {
        match r {
            Err(e) => e.body(),
            Ok(v) => panic!("expected Err, got Ok({v:?})"),
        }
    }

    fn ok_map(r: Result<HostResult, HostError>) -> Vec<(Value, Value)> {
        match r {
            Ok(Value::Map(m)) => m,
            other => panic!("expected Ok(map), got {other:?}"),
        }
    }

    fn field<'a>(m: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
        m.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    }

    // ---- ComponentRegistrar -----------------------------------------

    #[test]
    fn component_cross_plugin_collision_uses_exact_message() {
        let mut reg = ComponentRegistrar::default();
        reg.register("a", 5, "vio").unwrap();
        let err = reg.register("b", 5, "vio").unwrap_err();
        assert_eq!(err, "component_id 5 already reserved by a");
        // Same plugin re-registering its own id is fine.
        assert!(reg.register("a", 5, "vio").is_ok());
    }

    #[test]
    fn component_release_drops_both_indexes() {
        let mut reg = ComponentRegistrar::default();
        reg.register("a", 5, "vio").unwrap();
        assert!(reg.is_registered("a", 5));
        reg.release_plugin("a");
        assert!(!reg.is_registered("a", 5));
        // The component id is freed for another plugin.
        assert!(reg.register("b", 5, "vio").is_ok());
    }

    // ---- TelemetryExtender ------------------------------------------

    #[test]
    fn telemetry_namespaces_and_snapshots() {
        let mut t = TelemetryExtender::default();
        t.extend("p1", "ch", map(&[("v", Value::Integer(1.into()))]))
            .unwrap();
        let snap = t.snapshot();
        assert!(snap.contains_key("p1/ch"));
        // empty channel rejected.
        assert_eq!(
            t.extend("p1", "", Value::Map(vec![])).unwrap_err(),
            "channel must be a non-empty string"
        );
    }

    #[test]
    fn telemetry_clear_plugin_only_drops_that_prefix() {
        let mut t = TelemetryExtender::default();
        t.extend("p1", "a", Value::Map(vec![])).unwrap();
        t.extend("p2", "a", Value::Map(vec![])).unwrap();
        t.clear_plugin("p1");
        let snap = t.snapshot();
        assert!(!snap.contains_key("p1/a"));
        assert!(snap.contains_key("p2/a"));
    }

    // ---- DriverRegistry ---------------------------------------------

    #[test]
    fn driver_handle_id_format_and_release() {
        let mut d = DriverRegistry::default();
        let h1 = d.register("p", "camera");
        assert_eq!(h1.handle_id, "camera-p-1");
        let h2 = d.register("p", "lidar");
        assert_eq!(h2.handle_id, "lidar-p-2");
        d.release_plugin("p");
        // After release, the handles are gone; unregister of a stale id is a
        // no-op (does not panic).
        d.unregister(&h1.handle_id);
    }

    // ---- CameraClaimTracker -----------------------------------------

    #[test]
    fn camera_exclusive_hold_message_and_frame_drop_on_release() {
        let mut c = CameraClaimTracker::default();
        c.claim("a", "/dev/video0", true).unwrap();
        let err = c.claim("b", "/dev/video0", true).unwrap_err();
        assert_eq!(err, "camera /dev/video0 is exclusively held by a");

        // Frame cached for the holder, dropped on release.
        c.publish_frame(
            "/dev/video0",
            CameraFrame {
                frame_id: 1,
                width: 2,
                height: 2,
                format: "nv12".into(),
                data: vec![0, 1, 2, 3],
                ts_ns: 9,
            },
        );
        assert!(c.latest_frame("/dev/video0").is_some());
        c.release("a", "/dev/video0").unwrap();
        assert!(c.latest_frame("/dev/video0").is_none());
    }

    #[test]
    fn camera_release_of_unheld_path_is_noop_and_wrong_holder_errors() {
        let mut c = CameraClaimTracker::default();
        // No claim: release is a no-op.
        assert!(c.release("a", "/dev/video9").is_ok());
        c.claim("a", "/dev/video0", true).unwrap();
        // Wrong holder errors with the exact message.
        let err = c.release("b", "/dev/video0").unwrap_err();
        assert_eq!(err, "camera /dev/video0 is held by a, not b");
    }

    // ---- ConfigStore ------------------------------------------------

    #[test]
    fn config_drone_scope_shadows_global_and_degrades_when_unbound() {
        let mut cfg = ConfigStore::default();
        cfg.set("p", "k", Value::from("global-v"), "global", "");
        cfg.set("p", "k", Value::from("drone-v"), "drone", "agent-1");
        // With agent bound, drone scope wins.
        assert_eq!(
            cfg.get("p", "k", "agent-1", Value::Nil).as_str(),
            Some("drone-v")
        );
        // Without an agent, falls back to global.
        assert_eq!(cfg.get("p", "k", "", Value::Nil).as_str(), Some("global-v"));
        // drone scope with no agent degrades to global.
        cfg.set("p", "g", Value::from("via-degrade"), "drone", "");
        assert_eq!(
            cfg.get("p", "g", "", Value::Nil).as_str(),
            Some("via-degrade")
        );
    }

    #[test]
    fn config_missing_returns_default() {
        let cfg = ConfigStore::default();
        assert_eq!(
            cfg.get("p", "absent", "agent-1", Value::from("fallback"))
                .as_str(),
            Some("fallback")
        );
    }

    #[test]
    fn config_stored_nil_shadows_global_like_the_sentinel() {
        // A value explicitly set to nil at drone scope must shadow a global
        // value and the request default — matching the _MISSING sentinel, which
        // treats a stored None as present.
        let mut cfg = ConfigStore::default();
        cfg.set("p", "k", Value::from("global-v"), "global", "");
        cfg.set("p", "k", Value::Nil, "drone", "agent-1");
        let got = cfg.get("p", "k", "agent-1", Value::from("default-v"));
        assert!(matches!(got, Value::Nil));
    }

    // ---- mavlink_msg_id ---------------------------------------------

    #[test]
    fn mavlink_msg_id_v2_and_v1_and_short() {
        // v2: STX 0xFD, msgid little-endian 24-bit at bytes 7..10. id = 331.
        let mut v2 = vec![0xFD, 0, 0, 0, 0, 0, 0];
        v2.extend_from_slice(&[331u32.to_le_bytes()[0], 331u32.to_le_bytes()[1], 0]);
        assert_eq!(mavlink_msg_id(&v2), Some(331));
        // v1: STX 0xFE, msgid byte 5.
        let v1 = vec![0xFE, 0, 0, 0, 0, 102];
        assert_eq!(mavlink_msg_id(&v1), Some(102));
        // too short -> None.
        assert_eq!(mavlink_msg_id(&[0xFD, 0]), None);
        assert_eq!(mavlink_msg_id(&[]), None);
    }

    // ---- inline cap gates (now applied inside the handlers) ----------

    #[test]
    fn pose_inject_gate_demands_estimator_cap() {
        // A v2 ODOMETRY (331) send without estimator.pose.inject is denied. The
        // gate runs inside the handler, after msg_bytes validation.
        let host = RealHost::new();
        let mut frame = vec![0xFD, 0, 0, 0, 0, 0, 0];
        frame.extend_from_slice(&[331u32.to_le_bytes()[0], 331u32.to_le_bytes()[1], 0]);
        let args = map(&[("msg_bytes", Value::Binary(frame))]);
        assert_eq!(
            err_body(host.mavlink_send("p", &args, &caps(&[]))),
            "capability_denied: estimator.pose.inject"
        );
    }

    #[test]
    fn vio_component_gate_demands_vio_cap() {
        let host = RealHost::new();
        let args = map(&[
            ("msg_bytes", Value::Binary(vec![0xFE, 0, 0, 0, 0, 0])),
            ("component_id", Value::Integer(197.into())),
        ]);
        assert_eq!(
            err_body(host.mavlink_send("p", &args, &caps(&[]))),
            "capability_denied: mavlink.component.vio"
        );
    }

    #[test]
    fn string_component_id_participates_in_vio_gate() {
        // Python int("197") parses a numeric string, so a string component id in
        // the VIO set still triggers the cap gate instead of being skipped.
        let host = RealHost::new();
        let args = map(&[
            ("msg_bytes", Value::Binary(vec![0xFE, 0, 0, 0, 0, 0])),
            ("component_id", Value::from("197")),
        ]);
        assert_eq!(
            err_body(host.mavlink_send("p", &args, &caps(&[]))),
            "capability_denied: mavlink.component.vio"
        );
        // A whitespace-padded numeric string parses too (Python int(" 197 ")).
        let padded = map(&[
            ("msg_bytes", Value::Binary(vec![0xFE, 0, 0, 0, 0, 0])),
            ("component_id", Value::from(" 197 ")),
        ]);
        assert_eq!(
            err_body(host.mavlink_send("p", &padded, &caps(&[]))),
            "capability_denied: mavlink.component.vio"
        );
        // A non-numeric string is the "component_id not integer" error.
        let nonnum = map(&[
            ("msg_bytes", Value::Binary(vec![0xFE, 0, 0, 0, 0, 0])),
            ("component_id", Value::from("abc")),
        ]);
        assert_eq!(
            err_body(host.mavlink_send("p", &nonnum, &caps(&[]))),
            "component_id not integer"
        );
    }

    #[test]
    fn mavlink_send_validates_before_capability_gate() {
        // Ordering parity: a non-bytes msg_bytes AND a VIO component_id without
        // the cap fails validation FIRST (msg_bytes must be bytes), not on the
        // capability gate. Mirrors the Python source order.
        let host = RealHost::new();
        let args = map(&[
            ("msg_bytes", Value::Integer(7.into())),
            ("component_id", Value::Integer(197.into())),
        ]);
        assert_eq!(
            err_body(host.mavlink_send("p", &args, &caps(&[]))),
            "msg_bytes must be bytes"
        );
    }

    #[test]
    fn register_component_gate_uses_kind_cap() {
        let host = RealHost::new();
        let args = map(&[
            ("kind", Value::from("vio")),
            ("component_id", Value::Integer(197.into())),
        ]);
        assert_eq!(
            err_body(host.mavlink_register_component("p", &args, &caps(&[]))),
            "capability_denied: mavlink.component.vio"
        );
        // Granted -> registers.
        let m =
            ok_map(host.mavlink_register_component("p", &args, &caps(&["mavlink.component.vio"])));
        assert_eq!(field(&m, "registered").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn register_driver_gate_uses_kind_cap() {
        let host = RealHost::new();
        let args = map(&[
            ("kind", Value::from("lidar")),
            ("driver_ref", Value::from("ref")),
        ]);
        assert_eq!(
            err_body(host.peripheral_register_driver("p", &args, &caps(&[]))),
            "capability_denied: sensor.lidar.register"
        );
    }

    // ---- host method bodies -----------------------------------------

    #[test]
    fn telemetry_extend_merges_and_validates() {
        let host = RealHost::new();
        let args = map(&[
            ("channel", Value::from("metrics")),
            ("payload", map(&[("x", Value::Integer(1.into()))])),
        ]);
        let m = ok_map(host.telemetry_extend("p", &args));
        assert_eq!(field(&m, "merged").and_then(Value::as_bool), Some(true));
        assert_eq!(
            field(&m, "channel").and_then(Value::as_str),
            Some("metrics")
        );
        assert!(host.telemetry_snapshot().contains_key("p/metrics"));

        // non-map payload errors.
        let bad = map(&[
            ("channel", Value::from("m")),
            ("payload", Value::from("not-a-map")),
        ]);
        assert_eq!(
            err_body(host.telemetry_extend("p", &bad)),
            "payload must be a mapping"
        );
        // empty channel errors.
        let empty = map(&[("channel", Value::from(""))]);
        assert_eq!(
            err_body(host.telemetry_extend("p", &empty)),
            "channel must be a non-empty string"
        );
    }

    #[test]
    fn mavlink_send_without_router_returns_not_available() {
        let host = RealHost::new();
        let args = map(&[("msg_bytes", Value::Binary(vec![0xFE, 0, 0, 0, 0, 0]))]);
        let m = ok_map(host.mavlink_send("p", &args, &caps(&[])));
        assert_eq!(
            field(&m, "error").and_then(Value::as_str),
            Some("not_available")
        );
        assert_eq!(
            field(&m, "method").and_then(Value::as_str),
            Some("mavlink.send")
        );
    }

    #[test]
    fn mavlink_send_empty_and_wrong_type_error() {
        let host = RealHost::new();
        let empty = map(&[("msg_bytes", Value::Binary(vec![]))]);
        assert_eq!(
            err_body(host.mavlink_send("p", &empty, &caps(&[]))),
            "msg_bytes must be non-empty"
        );
        let wrong = map(&[("msg_bytes", Value::Integer(7.into()))]);
        assert_eq!(
            err_body(host.mavlink_send("p", &wrong, &caps(&[]))),
            "msg_bytes must be bytes"
        );
        // A msgpack string is also rejected (Python accepts only bytes/list).
        let as_str = map(&[("msg_bytes", Value::from("not-bytes"))]);
        assert_eq!(
            err_body(host.mavlink_send("p", &as_str, &caps(&[]))),
            "msg_bytes must be bytes"
        );
    }

    #[test]
    fn mavlink_send_unreserved_component_errors() {
        let host = RealHost::new();
        let args = map(&[
            ("msg_bytes", Value::Binary(vec![0xFE, 0, 0, 0, 0, 0])),
            ("component_id", Value::Integer(42.into())),
        ]);
        assert_eq!(
            err_body(host.mavlink_send("p", &args, &caps(&[]))),
            "component_id 42 not reserved by p; call mavlink.register_component first"
        );
    }

    #[test]
    fn register_component_vio_reservation_rule() {
        let host = RealHost::new();
        // A VIO id with a non-vio kind is refused (with the camera cap granted so
        // the reservation rule, not the cap gate, is what fires).
        let args = map(&[
            ("kind", Value::from("camera")),
            ("component_id", Value::Integer(197.into())),
        ]);
        assert_eq!(
            err_body(host.mavlink_register_component(
                "p",
                &args,
                &caps(&["mavlink.component.camera"])
            )),
            "component_id 197 is reserved for kind=vio"
        );
        // The right kind registers and returns the shape.
        let ok = map(&[
            ("kind", Value::from("vio")),
            ("component_id", Value::Integer(197.into())),
        ]);
        let m =
            ok_map(host.mavlink_register_component("p", &ok, &caps(&["mavlink.component.vio"])));
        assert_eq!(field(&m, "registered").and_then(Value::as_bool), Some(true));
        assert_eq!(field(&m, "component_id").and_then(Value::as_i64), Some(197));
        assert_eq!(field(&m, "kind").and_then(Value::as_str), Some("vio"));
    }

    #[test]
    fn driver_register_unknown_kind_errors() {
        let host = RealHost::new();
        let args = map(&[
            ("kind", Value::from("teleporter")),
            ("driver_ref", Value::from("r")),
        ]);
        assert_eq!(
            err_body(host.peripheral_register_driver("p", &args, &caps(&[]))),
            "unknown driver kind: teleporter"
        );
    }

    #[test]
    fn driver_register_and_unregister_round_trip() {
        let host = RealHost::new();
        let args = map(&[
            ("kind", Value::from("lidar")),
            ("driver_ref", Value::from("r")),
        ]);
        let m =
            ok_map(host.peripheral_register_driver("p", &args, &caps(&["sensor.lidar.register"])));
        let handle = field(&m, "handle_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        assert_eq!(handle, "lidar-p-1");
        let u = ok_map(host.peripheral_unregister_driver(
            "p",
            &map(&[("handle_id", Value::from(handle.as_str()))]),
        ));
        assert_eq!(
            field(&u, "unregistered").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn camera_claim_release_and_exclusive_collision() {
        let host = RealHost::new();
        let claim = map(&[("device_path", Value::from("/dev/video0"))]);
        let m = ok_map(host.camera_claim("a", &claim));
        assert_eq!(field(&m, "claimed").and_then(Value::as_bool), Some(true));
        assert_eq!(field(&m, "exclusive").and_then(Value::as_bool), Some(true));
        // A second plugin's exclusive claim is refused with the exact body.
        assert_eq!(
            err_body(host.camera_claim("b", &claim)),
            "camera /dev/video0 is exclusively held by a"
        );
        // Release by the holder.
        let r = ok_map(host.camera_release("a", &claim));
        assert_eq!(field(&r, "released").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn camera_get_frame_format_and_claim_checks() {
        let host = RealHost::new();
        // Unsupported format errors before any claim check.
        let bad_fmt = map(&[
            ("device_path", Value::from("/dev/video0")),
            ("format", Value::from("jpeg")),
        ]);
        assert_eq!(
            err_body(host.camera_get_frame("a", &bad_fmt)),
            "format 'jpeg' not supported; pick one of [\"nv12\", \"rgb888\", \"yuv420p\"]"
        );
        // Unclaimed path errors.
        let unclaimed = map(&[("device_path", Value::from("/dev/video0"))]);
        assert_eq!(
            err_body(host.camera_get_frame("a", &unclaimed)),
            "camera /dev/video0 is not claimed; call camera.claim first"
        );
        // Claim, then no-frame error.
        host.camera_claim("a", &unclaimed).unwrap();
        assert_eq!(
            err_body(host.camera_get_frame("a", &unclaimed)),
            "no frame available for /dev/video0; capture pipeline has not produced a buffer yet"
        );
        // Publish a frame, then a format mismatch.
        host.publish_camera_frame(
            "/dev/video0",
            CameraFrame {
                frame_id: 7,
                width: 4,
                height: 4,
                format: "rgb888".into(),
                data: vec![9, 9],
                ts_ns: 100,
            },
        );
        assert_eq!(
            err_body(host.camera_get_frame("a", &unclaimed)),
            "frame format mismatch: pipeline produced 'rgb888', plugin requested 'nv12'"
        );
        // Matching format returns the frame.
        let match_fmt = map(&[
            ("device_path", Value::from("/dev/video0")),
            ("format", Value::from("rgb888")),
        ]);
        let m = ok_map(host.camera_get_frame("a", &match_fmt));
        assert_eq!(field(&m, "frame_id").and_then(Value::as_i64), Some(7));
        assert_eq!(field(&m, "stale").and_then(Value::as_bool), Some(false));
        assert!(matches!(field(&m, "data"), Some(Value::Binary(_))));
    }

    #[test]
    fn camera_get_frame_held_by_another_plugin_errors() {
        let host = RealHost::new();
        let path = map(&[("device_path", Value::from("/dev/video0"))]);
        host.camera_claim("a", &path).unwrap();
        assert_eq!(
            err_body(host.camera_get_frame("b", &path)),
            "camera /dev/video0 is held by another plugin (a)"
        );
    }

    #[test]
    fn config_get_set_round_trip_with_agent_lookup() {
        let host = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "agent-1".to_string()));
        let set_args = map(&[("key", Value::from("k")), ("value", Value::from("v"))]);
        let s = ok_map(host.config_set("p", &set_args));
        assert_eq!(field(&s, "set").and_then(Value::as_bool), Some(true));
        assert_eq!(field(&s, "scope").and_then(Value::as_str), Some("drone"));
        let get_args = map(&[("key", Value::from("k"))]);
        let g = ok_map(host.config_get("p", &get_args));
        assert_eq!(field(&g, "value").and_then(Value::as_str), Some("v"));
    }

    #[test]
    fn config_set_validation() {
        let host = RealHost::new();
        // missing value.
        assert_eq!(
            err_body(host.config_set("p", &map(&[("key", Value::from("k"))]))),
            "value missing"
        );
        // bad scope: a truthy value that is neither drone nor global errors.
        let bad = map(&[
            ("key", Value::from("k")),
            ("value", Value::from("v")),
            ("scope", Value::from("fleet")),
        ]);
        assert_eq!(
            err_body(host.config_set("p", &bad)),
            "scope must be drone or global, got 'fleet'"
        );
        // empty key.
        let nokey = map(&[("key", Value::from("")), ("value", Value::from("v"))]);
        assert_eq!(
            err_body(host.config_set("p", &nokey)),
            "key must be a non-empty string"
        );
    }

    #[test]
    fn config_set_falsy_scope_coerces_to_drone() {
        // Mirrors Python `scope = args.get("scope") or "drone"`: any falsy scope
        // value is accepted and treated as "drone".
        let host = RealHost::new();
        for falsy in [
            Value::Nil,
            Value::from(""),
            Value::Integer(0.into()),
            Value::Boolean(false),
            Value::Array(vec![]),
            Value::Map(vec![]),
        ] {
            let args = map(&[
                ("key", Value::from("k")),
                ("value", Value::from("v")),
                ("scope", falsy.clone()),
            ]);
            let m = ok_map(host.config_set("p", &args));
            assert_eq!(
                field(&m, "scope").and_then(Value::as_str),
                Some("drone"),
                "falsy scope {falsy:?} should coerce to drone"
            );
        }
    }

    #[test]
    fn config_set_truthy_non_string_scope_errors_with_repr() {
        // A truthy non-string scope (a non-empty array of strings) is neither
        // drone nor global; the error reprs it with single-quoted inner strings.
        let host = RealHost::new();
        let args = map(&[
            ("key", Value::from("k")),
            ("value", Value::from("v")),
            ("scope", Value::Array(vec![Value::from("x")])),
        ]);
        assert_eq!(
            err_body(host.config_set("p", &args)),
            "scope must be drone or global, got ['x']"
        );
    }

    #[test]
    fn process_spawn_no_lookup_returns_not_available() {
        let host = RealHost::new();
        let args = map(&[("basename", Value::from("ffmpeg"))]);
        let m = ok_map(host.process_spawn("p", &args));
        assert_eq!(
            field(&m, "error").and_then(Value::as_str),
            Some("not_available")
        );
        assert_eq!(
            field(&m, "method").and_then(Value::as_str),
            Some("process.spawn")
        );
    }

    #[test]
    fn process_spawn_unregistered_runtime_returns_reason() {
        let host = RealHost::new().with_runtime_lookup(Box::new(|_pid| None));
        let args = map(&[("basename", Value::from("ffmpeg"))]);
        let m = ok_map(host.process_spawn("p", &args));
        assert_eq!(
            field(&m, "reason").and_then(Value::as_str),
            Some("plugin runtime not registered")
        );
    }

    #[test]
    fn process_spawn_allowlist_hit_and_miss() {
        let host = RealHost::new().with_runtime_lookup(Box::new(|_pid| {
            let mut allow = BTreeSet::new();
            allow.insert("ffmpeg".to_string());
            Some((PathBuf::from("/opt/ados/plugins/p"), allow))
        }));
        // Hit: authorized shape.
        let hit = map(&[
            ("basename", Value::from("ffmpeg")),
            ("args", Value::Array(vec![Value::from("-i")])),
            ("env", map(&[("K", Value::from("V"))])),
        ]);
        let m = ok_map(host.process_spawn("p", &hit));
        assert_eq!(field(&m, "authorized").and_then(Value::as_bool), Some(true));
        assert_eq!(
            field(&m, "install_dir").and_then(Value::as_str),
            Some("/opt/ados/plugins/p")
        );
        assert_eq!(
            field(&m, "basename").and_then(Value::as_str),
            Some("ffmpeg")
        );
        assert!(matches!(field(&m, "args"), Some(Value::Array(_))));
        // Miss: allowlist_violation error.
        let miss = map(&[("basename", Value::from("rm"))]);
        assert_eq!(
            err_body(host.process_spawn("p", &miss)),
            "allowlist_violation: rm"
        );
    }

    #[test]
    fn release_plugin_clears_all_but_config() {
        let host = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "agent-1".to_string()));
        // Seed every facade for plugin "p".
        host.mavlink_register_component(
            "p",
            &map(&[
                ("kind", Value::from("vio")),
                ("component_id", Value::Integer(197.into())),
            ]),
            &caps(&["mavlink.component.vio"]),
        )
        .unwrap();
        host.peripheral_register_driver(
            "p",
            &map(&[
                ("kind", Value::from("lidar")),
                ("driver_ref", Value::from("r")),
            ]),
            &caps(&["sensor.lidar.register"]),
        )
        .unwrap();
        host.camera_claim("p", &map(&[("device_path", Value::from("/dev/video0"))]))
            .unwrap();
        host.telemetry_extend("p", &map(&[("channel", Value::from("c"))]))
            .unwrap();
        host.config_set(
            "p",
            &map(&[("key", Value::from("k")), ("value", Value::from("v"))]),
        )
        .unwrap();

        host.release_plugin("p");

        // Components / drivers / cameras / telemetry cleared.
        assert!(!host.components.lock().unwrap().is_registered("p", 197));
        assert!(host.drivers.lock().unwrap().handles.is_empty());
        assert!(host.cameras.lock().unwrap().holder("/dev/video0").is_none());
        assert!(host.telemetry_snapshot().is_empty());
        // Config survives the release.
        let g = ok_map(host.config_get("p", &map(&[("key", Value::from("k"))])));
        assert_eq!(field(&g, "value").and_then(Value::as_str), Some("v"));
    }

    #[test]
    fn stubbed_methods_inherit_not_implemented() {
        let host = RealHost::new();
        for (got, name) in [
            (
                host.telemetry_subscribe("p", &Value::Map(vec![])),
                "telemetry.subscribe",
            ),
            (host.mission_read("p", &Value::Map(vec![])), "mission.read"),
            (
                host.mission_write("p", &Value::Map(vec![])),
                "mission.write",
            ),
            (
                host.recording_start("p", &Value::Map(vec![])),
                "recording.start",
            ),
            (
                host.recording_stop("p", &Value::Map(vec![])),
                "recording.stop",
            ),
        ] {
            let m = ok_map(got);
            assert_eq!(
                field(&m, "error").and_then(Value::as_str),
                Some("not_implemented")
            );
            assert_eq!(field(&m, "method").and_then(Value::as_str), Some(name));
        }
    }
}
