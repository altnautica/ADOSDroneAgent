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

use crate::host::{not_implemented, HostError, HostResult, HostServices};
use crate::mavlink_client::MavlinkClient;
use crate::vision_client::VisionClient;

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

/// Per-scope config store with optional on-disk persistence. Reads consult
/// drone scope first, then global, then the request default. Mirrors the Python
/// `ConfigStore` plus its optional persistence hook. The `_MISSING` sentinel of
/// the Python store is expressed here as `Option<Value>`: a stored `nil` is
/// `Some(Value::Nil)` (a present value) and is distinct from absent (`None`),
/// so a key explicitly set to nil shadows global and default exactly as the
/// Python sentinel does.
///
/// When `persist_path` is set, every `set` flushes the whole store to a 0600
/// JSON file (atomic temp-then-rename), and [`ConfigStore::load`] reads it back
/// at startup so plugin config survives a plugin-host restart. Without a path
/// the store is purely in-memory (the test/default posture).
#[derive(Default)]
struct ConfigStore {
    drone: BTreeMap<(String, String, String), Value>,
    global: BTreeMap<(String, String), Value>,
    persist_path: Option<PathBuf>,
}

/// One persisted config record. The `value` is the msgpack encoding of the
/// stored [`Value`], base64'd, so any rmpv value (nil, ints, maps, binary)
/// round-trips losslessly through JSON. `agent_id` is `None` for global scope.
#[derive(serde::Serialize, serde::Deserialize)]
struct ConfigRecord {
    plugin_id: String,
    key: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    agent_id: Option<String>,
    /// base64(msgpack(value)).
    value: String,
}

fn encode_value(value: &Value) -> Option<String> {
    let bytes = rmp_serde::to_vec(value).ok()?;
    use base64::Engine;
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn decode_value(encoded: &str) -> Option<Value> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    rmp_serde::from_slice(&bytes).ok()
}

impl ConfigStore {
    /// An in-memory store bound to a persistence path. Loads any existing
    /// records so prior plugin config survives a restart; a missing or
    /// unparseable file starts empty (config is best-effort durable, never a
    /// startup blocker).
    fn load(path: PathBuf) -> Self {
        let mut store = ConfigStore {
            persist_path: Some(path.clone()),
            ..ConfigStore::default()
        };
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(records) = serde_json::from_str::<Vec<ConfigRecord>>(&text) {
                for r in records {
                    let Some(value) = decode_value(&r.value) else {
                        continue;
                    };
                    match r.agent_id {
                        Some(agent) => {
                            store.drone.insert((r.plugin_id, agent, r.key), value);
                        }
                        None => {
                            store.global.insert((r.plugin_id, r.key), value);
                        }
                    }
                }
            }
        }
        store
    }

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
        // Python store. With a real agent-id lookup wired (build_host reads the
        // paired device id), a drone-scoped write now isolates per drone.
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
        self.persist();
    }

    /// Flush the whole store to the persistence path (atomic temp-then-rename,
    /// 0600). A write failure is logged and swallowed: durability is
    /// best-effort and never fails a plugin's config.set. A no-op when no path
    /// is bound.
    fn persist(&self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        let mut records: Vec<ConfigRecord> = Vec::new();
        for ((plugin_id, agent_id, key), value) in &self.drone {
            if let Some(encoded) = encode_value(value) {
                records.push(ConfigRecord {
                    plugin_id: plugin_id.clone(),
                    key: key.clone(),
                    agent_id: Some(agent_id.clone()),
                    value: encoded,
                });
            }
        }
        for ((plugin_id, key), value) in &self.global {
            if let Some(encoded) = encode_value(value) {
                records.push(ConfigRecord {
                    plugin_id: plugin_id.clone(),
                    key: key.clone(),
                    agent_id: None,
                    value: encoded,
                });
            }
        }
        if let Err(e) = write_json_owner_only(path, &records) {
            tracing::warn!(path = %path.display(), error = %e, "plugin config persist failed");
        }
    }
}

/// Serialize `records` to JSON and write `path` owner-only (0600) via an atomic
/// temp-then-rename, enforcing the mode on every write (the open-time mode flag
/// only applies on creation, so a reused looser-perm inode would otherwise keep
/// its mode).
fn write_json_owner_only(path: &std::path::Path, records: &[ConfigRecord]) -> std::io::Result<()> {
    let json = serde_json::to_vec(records)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    write_bytes_owner_only(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn write_bytes_owner_only(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_bytes_owner_only(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

// ---------------------------------------------------------------------
// Display page sidecar
// ---------------------------------------------------------------------

/// One label/value row of a plugin-contributed display page. The serde shape is
/// the contract `ados_display::sidecar::LcdPluginRow` reads; the display crate
/// is not a build dependency of the host, so the shape is shared by JSON, not a
/// shared type.
#[derive(serde::Serialize)]
struct DisplayRow {
    label: String,
    value: String,
}

/// One declared touch zone on a plugin display page. Mirrors
/// `ados_display::sidecar::LcdPluginZone`.
#[derive(serde::Serialize)]
struct DisplayZone {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    key: String,
    label: String,
}

/// The full plugin display-page content. Mirrors
/// `ados_display::sidecar::LcdPluginPage`.
#[derive(serde::Serialize)]
struct DisplayPage {
    title: String,
    rows: Vec<DisplayRow>,
    zones: Vec<DisplayZone>,
}

/// Coerce a msgpack value to an i32 zone coordinate, defaulting to 0 for an
/// absent or non-numeric value (lenient, matching the display loader's
/// defaulted fields).
fn zone_i32(value: Option<&Value>) -> i32 {
    value
        .and_then(|v| v.as_i64())
        .map(|n| n.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
        .unwrap_or(0)
}

/// Read a string field from a msgpack-map element, defaulting to an empty
/// string for an absent or non-string value.
fn elem_str(value: &Value, key: &str) -> String {
    arg_str(value, key).unwrap_or("").to_string()
}

/// Parse a `display.page.set` request into the page content. Lenient by design:
/// `title`/`rows`/`zones` are all optional, a row defaults its label/value to
/// empty, and a zone defaults its coordinates to 0, so a partial payload still
/// produces a valid (possibly empty) page. A non-array `rows`/`zones` is
/// rejected so a misshaped request is a clear error rather than a silent empty.
fn parse_display_page(args: &Value) -> Result<DisplayPage, HostError> {
    let title = arg_str(args, "title").unwrap_or("").to_string();

    let rows = match map_get(args, "rows") {
        None | Some(Value::Nil) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| DisplayRow {
                label: elem_str(item, "label"),
                value: elem_str(item, "value"),
            })
            .collect(),
        Some(_) => return Err(HostError::Rpc("rows must be a list".to_string())),
    };

    let zones = match map_get(args, "zones") {
        None | Some(Value::Nil) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| DisplayZone {
                x: zone_i32(map_get(item, "x")),
                y: zone_i32(map_get(item, "y")),
                w: zone_i32(map_get(item, "w")),
                h: zone_i32(map_get(item, "h")),
                key: elem_str(item, "key"),
                label: elem_str(item, "label"),
            })
            .collect(),
        Some(_) => return Err(HostError::Rpc("zones must be a list".to_string())),
    };

    Ok(DisplayPage { title, rows, zones })
}

/// Serialize a display page to JSON and write `path` via an atomic
/// temp-then-rename, creating the parent. The sidecar is read by the display
/// service (a separate process), so it uses default file perms like the other
/// `/run/ados/lcd-*.json` sidecars, not the owner-only mode the config store
/// uses for its secret-bearing file.
fn write_display_page(path: &std::path::Path, page: &DisplayPage) -> std::io::Result<()> {
    use std::io::Write;
    let json = serde_json::to_vec(page).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// GPIO command-socket forward
// ---------------------------------------------------------------------

/// Cap on the reply read so a misbehaving service can't grow the buffer.
const GPIO_REPLY_CAP: usize = 64 * 1024;

/// Send one newline-JSON request to a unix command socket and read the one-line
/// JSON reply. Blocking (synchronous), so the synchronous host trait method can
/// call it directly, with short read/write timeouts so a wedged service surfaces
/// as an error rather than hanging the plugin connection. The forward mirrors the
/// REST layer's radio/wifi command-socket clients.
#[cfg(unix)]
fn gpio_socket_roundtrip(
    sock_path: &std::path::Path,
    request: &serde_json::Value,
) -> std::io::Result<serde_json::Value> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let mut body = serde_json::to_vec(request)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    body.push(b'\n');
    stream.write_all(&body)?;
    stream.flush()?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') || buf.len() > GPIO_REPLY_CAP {
            break;
        }
    }
    let line = match buf.iter().position(|&b| b == b'\n') {
        Some(i) => &buf[..i],
        None => &buf[..],
    };
    serde_json::from_slice(line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Non-unix dev hosts have no unix-socket command plane: report the service as
/// unreachable so the forward degrades to `not_available`.
#[cfg(not(unix))]
fn gpio_socket_roundtrip(
    _sock_path: &std::path::Path,
    _request: &serde_json::Value,
) -> std::io::Result<serde_json::Value> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no unix socket on this platform",
    ))
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

/// Read an integer field from a msgpack-map `args`, accepting a signed or
/// unsigned msgpack integer. Returns `None` for an absent or non-integer value.
fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    let v = map_get(args, key)?;
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
}

/// Read a numeric field from a msgpack-map `args` as an f64, accepting any
/// msgpack number (a float OR an integer, so `0` and `0.0` both read). A present
/// non-numeric value is an error (the caller distinguishes it from absent);
/// `Ok(None)` is an absent key, which the caller defaults to 0.0. The returned
/// f64 may be non-finite (a NaN/inf encoded by the client) — the setpoint
/// validator rejects a non-finite value on an active axis downstream, so the
/// finiteness check is one place, not scattered through the reads.
fn arg_f64_opt(args: &Value, key: &str) -> Result<Option<f64>, HostError> {
    match map_get(args, key) {
        None | Some(Value::Nil) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) => Ok(Some(n)),
            None => Err(HostError::Rpc(format!("{key} must be a number"))),
        },
    }
}

/// Read a numeric field as an f64, defaulting an absent key to 0.0 (an axis the
/// type mask ignores is conventionally left at 0). A present non-number errors.
fn arg_f64(args: &Value, key: &str) -> Result<f64, HostError> {
    Ok(arg_f64_opt(args, key)?.unwrap_or(0.0))
}

/// Read a numeric field as an f32, defaulting an absent key to 0.0. A present
/// non-number errors. The f64→f32 narrowing matches the wire field width of the
/// velocity / accel / yaw setpoint fields.
fn arg_f32(args: &Value, key: &str) -> Result<f32, HostError> {
    Ok(arg_f64(args, key)? as f32)
}

/// Convert a `serde_json::Value` (a command-socket reply) to the msgpack
/// `rmpv::Value` the plugin sees as the response `args`. Integers stay integers,
/// floats stay floats, null becomes nil, so the reply shape round-trips into the
/// plugin's response envelope unchanged.
fn json_to_mpv(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Integer(u.into())
            } else {
                Value::F64(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::from(s.as_str()),
        serde_json::Value::Array(items) => Value::Array(items.iter().map(json_to_mpv).collect()),
        serde_json::Value::Object(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (Value::from(k.as_str()), json_to_mpv(v)))
                .collect(),
        ),
    }
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
// Guided setpoint parse
// ---------------------------------------------------------------------

/// Source system id stamped on a guided-setpoint frame: the agent/companion
/// identity the router uses on its own FC send path, so a setpoint from this
/// surface is wire-identical to one the router sent. Matches the native command
/// surface's source identity.
const GUIDED_SOURCE_SYSTEM_ID: u8 = 1;
const GUIDED_SOURCE_COMPONENT_ID: u8 = 191;

/// Target identity: the single-vehicle ArduPilot defaults (1/1). A request may
/// override these with `target_system` / `target_component`.
const GUIDED_TARGET_SYSTEM: u8 = 1;
const GUIDED_TARGET_COMPONENT: u8 = 1;

/// Read an optional u8 field, defaulting an absent key to `default`. A present
/// value out of u8 range (or a non-integer) is a clear error rather than a wrap.
fn u8_arg_or(args: &Value, key: &str, default: u8) -> Result<u8, HostError> {
    match map_get(args, key) {
        None | Some(Value::Nil) => Ok(default),
        Some(_) => match arg_i64(args, key) {
            Some(n) if (0..=u8::MAX as i64).contains(&n) => Ok(n as u8),
            _ => Err(HostError::Rpc(format!("{key} out of range"))),
        },
    }
}

/// Source identity stamped on a TUNNEL frame from this surface: the same
/// agent/companion identity the guided-setpoint surface and the router use, so a
/// TUNNEL frame from a plugin is wire-consistent with the agent's other sends.
const TUNNEL_SOURCE_SYSTEM_ID: u8 = GUIDED_SOURCE_SYSTEM_ID;
const TUNNEL_SOURCE_COMPONENT_ID: u8 = GUIDED_SOURCE_COMPONENT_ID;

/// Read a required u16 field (the TUNNEL `payload_type`). An absent key, a
/// non-integer, or a value outside the u16 range is a clear error.
fn required_u16_arg(args: &Value, key: &str) -> Result<u16, HostError> {
    match map_get(args, key) {
        None | Some(Value::Nil) => Err(HostError::Rpc(format!("{key} is required"))),
        Some(_) => match arg_i64(args, key) {
            Some(n) if (0..=u16::MAX as i64).contains(&n) => Ok(n as u16),
            _ => Err(HostError::Rpc(format!("{key} out of range"))),
        },
    }
}

/// The MAVLink message id of the message this setpoint builds, for the response.
fn setpoint_msg_id(sp: &ados_protocol::mavlink::GuidedSetpoint) -> u32 {
    use ados_protocol::mavlink::SetpointKind;
    match sp.kind {
        SetpointKind::LocalNed => ados_protocol::mavlink::MSG_ID_SET_POSITION_TARGET_LOCAL_NED,
        SetpointKind::GlobalInt => ados_protocol::mavlink::MSG_ID_SET_POSITION_TARGET_GLOBAL_INT,
    }
}

/// Parse a `flight.guided_setpoint.send` request into a [`GuidedSetpoint`].
///
/// Required: `kind` (`"local_ned"` | `"global_int"`), `coordinate_frame` (an
/// integer `MAV_FRAME_*`), and `type_mask` (an integer that fits u16; a set bit
/// ignores that axis). The numeric axis fields default to 0 when absent (an
/// ignored axis is conventionally left unset); each is read as a number and a
/// present non-number is an error. The finiteness / sane-mask / valid-frame
/// checks are NOT applied here — they live in [`GuidedSetpoint::validate`],
/// called by `build_message`, so the policy lives in one place.
fn parse_guided_setpoint(
    args: &Value,
) -> Result<ados_protocol::mavlink::GuidedSetpoint, HostError> {
    use ados_protocol::mavlink::{GuidedSetpoint, SetpointKind};

    let kind = match arg_str(args, "kind") {
        Some("local_ned") => SetpointKind::LocalNed,
        Some("global_int") => SetpointKind::GlobalInt,
        Some(other) => {
            return Err(HostError::Rpc(format!(
                "kind must be \"local_ned\" or \"global_int\", got {other:?}"
            )))
        }
        None => {
            return Err(HostError::Rpc(
                "kind must be \"local_ned\" or \"global_int\"".to_string(),
            ))
        }
    };

    let coordinate_frame = match arg_i64(args, "coordinate_frame") {
        Some(n) if (0..=u8::MAX as i64).contains(&n) => n as u8,
        Some(_) => return Err(HostError::Rpc("coordinate_frame out of range".to_string())),
        None => {
            return Err(HostError::Rpc(
                "coordinate_frame must be an integer".to_string(),
            ))
        }
    };

    let type_mask = match arg_i64(args, "type_mask") {
        Some(n) if (0..=u16::MAX as i64).contains(&n) => n as u16,
        Some(_) => return Err(HostError::Rpc("type_mask out of range".to_string())),
        None => return Err(HostError::Rpc("type_mask must be an integer".to_string())),
    };

    Ok(GuidedSetpoint {
        kind,
        coordinate_frame,
        type_mask,
        x: arg_f64(args, "x")?,
        y: arg_f64(args, "y")?,
        z: arg_f64(args, "z")?,
        vx: arg_f32(args, "vx")?,
        vy: arg_f32(args, "vy")?,
        vz: arg_f32(args, "vz")?,
        afx: arg_f32(args, "afx")?,
        afy: arg_f32(args, "afy")?,
        afz: arg_f32(args, "afz")?,
        yaw: arg_f32(args, "yaw")?,
        yaw_rate: arg_f32(args, "yaw_rate")?,
    })
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
    vision: Option<Arc<VisionClient>>,
    plugin_runtime_lookup: Option<RuntimeLookup>,
    agent_id_lookup: Option<AgentIdLookup>,
    /// Sidecar path the reserved display page reads its content from. The
    /// canonical `/run/ados/lcd-plugin-page.json`; a builder overrides it in
    /// tests so the write round-trips without touching `/run`.
    display_page_path: PathBuf,
    /// Command socket the GPIO-output service serves. The host forwards each
    /// `gpio.*` method to it (the radio/wifi-cmd-socket precedent); a builder
    /// overrides it in tests so the forward round-trips against a stub.
    gpio_cmd_path: PathBuf,
    /// Command socket the radio service serves for the auxiliary application
    /// stream. The host forwards each `radio.aux_stream.*` method to it (the same
    /// command-socket precedent); a builder overrides it in tests.
    radio_aux_cmd_path: PathBuf,
    /// The plugin id that currently holds the auxiliary stream open, or `None`
    /// when the stream is closed. The aux pair is a single shared resource on the
    /// one adapter, so at most one plugin owns it at a time. Used to close the
    /// stream automatically when its owner disconnects (the SAFE-by-default
    /// invariant: a stream never outlives the plugin that opened it).
    aux_stream_owner: Mutex<Option<String>>,
}

/// Canonical sidecar the reserved data-driven display page reads. Kept in sync
/// with `ados_display::sidecar::LCD_PLUGIN_PAGE_PATH` by the cross-crate JSON
/// shape, not a build dependency (the display crate is not on the host's
/// dependency path).
const LCD_PLUGIN_PAGE_PATH: &str = "/run/ados/lcd-plugin-page.json";

/// Canonical GPIO-output command socket the host forwards `gpio.*` methods to.
/// Kept in sync with `ados_gpio::GPIO_CMD_SOCK` by the cross-crate wire string,
/// not a build dependency (the gpio crate is not on the host's dependency path).
const GPIO_CMD_SOCK: &str = "/run/ados/gpio-cmd.sock";

/// Canonical radio auxiliary-stream command socket the host forwards
/// `radio.aux_stream.*` methods to. Kept in sync with
/// `ados_radio::paths::RADIO_AUX_SOCK` by the cross-crate wire string, not a build
/// dependency (the radio crate is not on the host's dependency path).
const RADIO_AUX_CMD_SOCK: &str = "/run/ados/radio-aux.sock";

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
            vision: None,
            plugin_runtime_lookup: None,
            agent_id_lookup: None,
            display_page_path: PathBuf::from(LCD_PLUGIN_PAGE_PATH),
            gpio_cmd_path: PathBuf::from(GPIO_CMD_SOCK),
            radio_aux_cmd_path: PathBuf::from(RADIO_AUX_CMD_SOCK),
            aux_stream_owner: Mutex::new(None),
        }
    }

    /// Override the display-page sidecar path (builder style, tests). Production
    /// uses the canonical `/run/ados/lcd-plugin-page.json` from [`Self::new`].
    pub fn with_display_page_path(mut self, path: PathBuf) -> Self {
        self.display_page_path = path;
        self
    }

    /// Override the GPIO command socket path (builder style, tests). Production
    /// uses the canonical `/run/ados/gpio-cmd.sock` from [`Self::new`].
    pub fn with_gpio_cmd_path(mut self, path: PathBuf) -> Self {
        self.gpio_cmd_path = path;
        self
    }

    /// Override the radio auxiliary-stream command socket path (builder style,
    /// tests). Production uses the canonical `/run/ados/radio-aux.sock` from
    /// [`Self::new`].
    pub fn with_radio_aux_cmd_path(mut self, path: PathBuf) -> Self {
        self.radio_aux_cmd_path = path;
        self
    }

    /// Wire the MAVLink client (builder style).
    pub fn with_mavlink(mut self, mavlink: Arc<MavlinkClient>) -> Self {
        self.mavlink = Some(mavlink);
        self
    }

    /// Wire the vision-engine client (builder style). When wired, the three
    /// vision request methods proxy to the engine over its socket and
    /// `vision_subscribe_stream` hands out the engine's frame-descriptor
    /// fanout, mirroring the MAVLink wiring. When unwired the methods return the
    /// `not_implemented` shape and the stream is `None`, matching the
    /// MAVLink not-available posture.
    pub fn with_vision(mut self, vision: Arc<VisionClient>) -> Self {
        self.vision = Some(vision);
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

    /// Persist plugin config to a 0600 JSON file (builder style). Loads any
    /// existing records so config survives a plugin-host restart, then writes
    /// the whole store on every `config.set`. Without this the store is
    /// in-memory only (config is lost on restart).
    pub fn with_config_persistence(mut self, path: PathBuf) -> Self {
        self.config = Mutex::new(ConfigStore::load(path));
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

    /// Forward a built `gpio.*` request to the GPIO-output service's command
    /// socket and return its reply as the plugin's response `args`. Synchronous
    /// (a blocking unix-socket round-trip), so the host trait method stays
    /// non-async like its siblings; the service answers a `set` / `beep` request
    /// immediately (a beep schedules and returns), so the round-trip is short.
    ///
    /// A missing service / connection / IO error degrades to the `not_available`
    /// shape rather than erroring, matching the `mavlink.send` and `process.spawn`
    /// not-available paths — the GPIO service may simply not be up on this board.
    fn forward_gpio(&self, request: serde_json::Value, method: &str) -> HostResult {
        match gpio_socket_roundtrip(&self.gpio_cmd_path, &request) {
            Ok(reply) => json_to_mpv(&reply),
            Err(e) => {
                tracing::debug!(method, error = %e, "gpio command forward failed");
                Value::Map(vec![
                    (Value::from("error"), Value::from("not_available")),
                    (Value::from("method"), Value::from(method)),
                    (
                        Value::from("reason"),
                        Value::from("gpio service unavailable"),
                    ),
                ])
            }
        }
    }

    /// Forward a built `radio.aux_stream.*` request to the radio service's
    /// auxiliary command socket and return its reply. Synchronous (a blocking
    /// unix-socket round-trip over the shared [`gpio_socket_roundtrip`] newline-JSON
    /// helper), so the host trait method stays non-async; the service answers an
    /// open/close immediately. Returns `(reply, ok)` so the caller can update the
    /// owner bookkeeping only when the service confirmed the apply.
    ///
    /// A missing service / connection / IO error degrades to the `not_available`
    /// shape rather than erroring, matching the GPIO / mavlink not-available paths
    /// — the radio service may not be up on this board (e.g. a ground-station
    /// profile, or a drone with no adapter).
    fn forward_radio_aux(&self, request: serde_json::Value, method: &str) -> (HostResult, bool) {
        match gpio_socket_roundtrip(&self.radio_aux_cmd_path, &request) {
            Ok(reply) => {
                let ok = reply.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                (json_to_mpv(&reply), ok)
            }
            Err(e) => {
                tracing::debug!(method, error = %e, "radio aux command forward failed");
                let v = Value::Map(vec![
                    (Value::from("error"), Value::from("not_available")),
                    (Value::from("method"), Value::from(method)),
                    (
                        Value::from("reason"),
                        Value::from("radio service unavailable"),
                    ),
                ]);
                (v, false)
            }
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

    /// Host-coupled methods this host does NOT override, so they fall to the
    /// [`HostServices`] trait default and always return the `not_implemented`
    /// shape regardless of runtime wiring.
    ///
    /// This is distinct from a method whose backing client is merely *not up
    /// yet* (`mavlink.send`, the three vision request methods): those have real
    /// bodies that degrade to a `not_available` / `not_implemented` response
    /// only while their socket is absent, and become live the moment it is. The
    /// methods listed here have no body at all on this host, so a capability that
    /// gates only these can do nothing but error.
    ///
    /// `mavlink.subscribe` and `vision.subscribe_frames` are deliberately absent:
    /// the server short-circuits both to the stream methods this host overrides
    /// (`mavlink_subscribe_stream` / `vision_subscribe_stream`), so they never
    /// reach the not_implemented trait default.
    ///
    /// A `#[cfg(test)]` test (`unimplemented_methods_match_reality`) asserts this
    /// list is exactly the set of methods that return `not_implemented` from a
    /// freshly-built host, so it cannot silently drift as methods are wired.
    pub const UNIMPLEMENTED_HOST_METHODS: &'static [crate::dispatch::Method] = &[
        crate::dispatch::Method::TelemetrySubscribe,
        crate::dispatch::Method::MissionRead,
        crate::dispatch::Method::MissionWrite,
        crate::dispatch::Method::RecordingStart,
        crate::dispatch::Method::RecordingStop,
    ];

    /// The capabilities that gate ONLY [`UNIMPLEMENTED_HOST_METHODS`](Self::UNIMPLEMENTED_HOST_METHODS)
    /// on this host, so granting one of them buys the operator nothing but a
    /// `not_implemented` error at call time.
    ///
    /// The lifecycle controller refuses to grant these (an honest refuse-at-
    /// install rather than a surprise error-at-call). A capability that also
    /// gates a wired method (e.g. a cap shared with an implemented surface) is
    /// excluded, so a still-useful capability is never withheld.
    pub fn ungrantable_caps() -> BTreeSet<String> {
        // The caps gated by the unimplemented methods.
        let unimplemented: BTreeSet<&'static str> = Self::UNIMPLEMENTED_HOST_METHODS
            .iter()
            .filter_map(|m| m.required_cap())
            .collect();
        // The caps gated by any IMPLEMENTED dispatch-level method, so a cap
        // shared with a wired surface is never refused. A method is implemented
        // here unless it is in the unimplemented list.
        let implemented: BTreeSet<&'static str> = ALL_DISPATCH_METHODS
            .iter()
            .filter(|m| !Self::UNIMPLEMENTED_HOST_METHODS.contains(m))
            .filter_map(|m| m.required_cap())
            .collect();
        unimplemented
            .difference(&implemented)
            .map(|c| c.to_string())
            .collect()
    }
}

/// Every dispatch-level [`Method`](crate::dispatch::Method), so
/// [`RealHost::ungrantable_caps`] can subtract the caps gated by implemented
/// methods. Kept exhaustive by the match in
/// [`Method::wire_name`](crate::dispatch::Method::wire_name): a new variant
/// forces an arm there, and the `all_dispatch_methods_is_exhaustive` test locks
/// this list to the generated table's cardinality.
const ALL_DISPATCH_METHODS: &[crate::dispatch::Method] = {
    use crate::dispatch::Method::*;
    &[
        EventPublish,
        EventSubscribe,
        Ping,
        TelemetrySubscribe,
        TelemetryExtend,
        MissionRead,
        MissionWrite,
        RecordingStart,
        RecordingStop,
        MavlinkSubscribe,
        MavlinkSend,
        MavlinkTunnelSend,
        MavlinkRegisterComponent,
        PeripheralRegisterDriver,
        PeripheralUnregisterDriver,
        CameraClaim,
        CameraRelease,
        CameraGetFrame,
        ConfigGet,
        ConfigSet,
        ProcessSpawn,
        DisplayPageSet,
        GpioOutputSet,
        GpioBuzzerBeep,
        GuidedSetpointSend,
        RadioAuxStreamOpen,
        RadioAuxStreamClose,
        VisionSubscribeFrames,
        VisionRegisterModel,
        VisionInfer,
        VisionPublishDetection,
        VisionSubscribeDetections,
        VisionDesignateTrack,
    ]
};

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

    fn mavlink_tunnel_send(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        // Validate the request, build the single TUNNEL frame, and write it to
        // the MAVLink socket. The dispatch gate already enforced the tunnel
        // capability before this runs. The tunnel is a transparent opaque pipe:
        // this stamps no application semantics on the payload, so any per-payload
        // HMAC/replay lives inside the bytes the caller supplied.

        // payload_type is required and must be a private (application) type; the
        // builder re-checks the floor, so an out-of-range type is refused twice.
        let payload_type = required_u16_arg(args, "payload_type")?;

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

        // Optional target overrides; default to the single-vehicle identity.
        let target_system = u8_arg_or(args, "target_system", GUIDED_TARGET_SYSTEM)?;
        let target_component = u8_arg_or(args, "target_component", GUIDED_TARGET_COMPONENT)?;

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
                // Best-effort send (send_bytes swallows a full queue / write
                // error, matching mavlink.send), so the success shape stands.
                client.send_bytes(&frame);
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

    fn display_page_set(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        // Parse the request into the display-page shape, then atomically write
        // the sidecar the reserved page reads. The dispatch gate already
        // enforced the display capability before this runs.
        let page = parse_display_page(args)?;
        let rows = page.rows.len();
        let zones = page.zones.len();
        if let Err(e) = write_display_page(&self.display_page_path, &page) {
            tracing::warn!(
                plugin_id = %plugin_id,
                path = %self.display_page_path.display(),
                error = %e,
                "display page write failed"
            );
            // A write failure is a graceful-degrade response, not a gate
            // failure (matches the not_available shape the other host methods
            // return when their backing surface is unavailable).
            return Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("display.page.set")),
                (
                    Value::from("reason"),
                    Value::from("display page write failed"),
                ),
            ]));
        }
        Ok(Value::Map(vec![
            (Value::from("set"), Value::Boolean(true)),
            (Value::from("rows"), Value::Integer((rows as i64).into())),
            (Value::from("zones"), Value::Integer((zones as i64).into())),
        ]))
    }

    fn gpio_output_set(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        // Build the service's `set` request from the validated args, then forward
        // it to the GPIO-output command socket. The dispatch gate already enforced
        // the GPIO-output capability before this runs.
        let pin = arg_i64(args, "pin")
            .ok_or_else(|| HostError::Rpc("pin must be an integer".to_string()))?;
        let level = arg_str(args, "level").filter(|l| *l == "high" || *l == "low");
        let Some(level) = level else {
            return Err(HostError::Rpc(
                "level must be \"high\" or \"low\"".to_string(),
            ));
        };
        let chip = arg_i64(args, "chip").unwrap_or(0);
        let req = serde_json::json!({"op": "set", "chip": chip, "pin": pin, "level": level});
        Ok(self.forward_gpio(req, "gpio.output.set"))
    }

    fn gpio_buzzer_beep(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        // Build the service's `beep` request from the validated args. The service
        // clamps the pattern into the safe bounds, so the host forwards the raw
        // values verbatim and lets the single owner enforce the ceiling.
        let pin = arg_i64(args, "pin")
            .ok_or_else(|| HostError::Rpc("pin must be an integer".to_string()))?;
        let on_ms = arg_i64(args, "on_ms")
            .ok_or_else(|| HostError::Rpc("on_ms must be an integer".to_string()))?;
        let cycles = arg_i64(args, "cycles")
            .ok_or_else(|| HostError::Rpc("cycles must be an integer".to_string()))?;
        let chip = arg_i64(args, "chip").unwrap_or(0);
        let mut req = serde_json::json!({
            "op": "beep", "chip": chip, "pin": pin, "on_ms": on_ms, "cycles": cycles,
        });
        // Optional carrier/envelope fields ride through when present.
        if let Some(off_ms) = arg_i64(args, "off_ms") {
            req["off_ms"] = off_ms.into();
        }
        if let Some(freq_hz) = arg_i64(args, "freq_hz") {
            req["freq_hz"] = freq_hz.into();
        }
        if let Some(duty_pct) = arg_i64(args, "duty_pct") {
            req["duty_pct"] = duty_pct.into();
        }
        Ok(self.forward_gpio(req, "gpio.buzzer.beep"))
    }

    fn radio_aux_stream_open(
        &self,
        plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        // The dispatch gate already enforced the auxiliary-stream capability. The
        // open carries no caller-tunable parameters: the radio service resolves the
        // effective aux ports / FEC / MCS from its own config, so the host never
        // lets a plugin pick a radio-port (which could collide with the data or
        // control planes). Forward a bare open and record ownership on success so
        // the stream is closed when this plugin disconnects.
        let req = serde_json::json!({"op": "open"});
        let (reply, ok) = self.forward_radio_aux(req, "radio.aux_stream.open");
        if ok {
            *self
                .aux_stream_owner
                .lock()
                .expect("aux stream owner mutex poisoned") = Some(plugin_id.to_string());
        }
        Ok(reply)
    }

    fn radio_aux_stream_close(
        &self,
        plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        // The dispatch gate already enforced the auxiliary-stream capability.
        // Forward a bare close (idempotent on the service side) and clear the
        // ownership record when this plugin held it, so a later disconnect does not
        // forward a redundant close. A close by a plugin that does not own the
        // stream still forwards (the service no-ops a closed stream) but leaves any
        // other owner's record intact.
        let req = serde_json::json!({"op": "close"});
        let (reply, ok) = self.forward_radio_aux(req, "radio.aux_stream.close");
        if ok {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .expect("aux stream owner mutex poisoned");
            if owner.as_deref() == Some(plugin_id) {
                *owner = None;
            }
        }
        Ok(reply)
    }

    fn guided_setpoint_send(
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

        // Optional target overrides; default to the single-vehicle ArduPilot
        // identity. An out-of-range override is a clear error rather than a wrap.
        let target_system = u8_arg_or(args, "target_system", GUIDED_TARGET_SYSTEM)?;
        let target_component = u8_arg_or(args, "target_component", GUIDED_TARGET_COMPONENT)?;

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
                // Best-effort send (send_bytes swallows a full queue / write
                // error, matching mavlink.send), so the success shape stands.
                client.send_bytes(&frame);
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
        // SAFE-by-default: a radio auxiliary stream never outlives the plugin that
        // opened it. If this plugin held the stream open, forward a close so the
        // additive radio pair is torn down on disconnect (it never touches the
        // data / control planes). Take the owner slot first so the forward happens
        // without holding the lock, and only when this plugin is the owner.
        let owned = {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .expect("aux stream owner mutex poisoned");
            if owner.as_deref() == Some(plugin_id) {
                *owner = None;
                true
            } else {
                false
            }
        };
        if owned {
            let req = serde_json::json!({"op": "close"});
            let _ = self.forward_radio_aux(req, "radio.aux_stream.close");
        }
    }

    fn mavlink_subscribe_stream(
        &self,
        _plugin_id: &str,
        _msg_name: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.mavlink.as_ref().map(|c| c.subscribe())
    }

    fn vision_subscribe_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        // The engine fans every camera's descriptors out on one broadcast; the
        // per-camera filter is applied plugin-side (the SDK subscribe_frames
        // callback drops a non-matching camera). When the engine socket is not
        // up the slot is None and no stream arms, matching the MAVLink posture.
        self.vision.as_ref().map(|c| c.subscribe_frames())
    }

    fn vision_subscribe_detection_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        // The engine fans every camera's detection batches out on one broadcast;
        // the per-camera filter is applied plugin-side (the SDK callback drops a
        // non-matching camera). When the engine socket is not up the slot is None
        // and no stream arms, matching the frame-stream posture.
        self.vision.as_ref().map(|c| c.subscribe_detections())
    }

    async fn vision_register_model(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.register_model"));
        };
        // A transport / engine error surfaces as the response envelope `error`
        // (a soft failure), exactly like the engine's own reply error would.
        client
            .register_model(args)
            .await
            .map_err(|e| HostError::Rpc(e.0))
    }

    async fn vision_infer(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.infer"));
        };
        client.infer(args).await.map_err(|e| HostError::Rpc(e.0))
    }

    async fn vision_publish_detection(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.publish_detection"));
        };
        client
            .publish_detection(args)
            .await
            .map_err(|e| HostError::Rpc(e.0))
    }

    async fn vision_designate_track(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.designate_track"));
        };
        client
            .designate_track(args)
            .await
            .map_err(|e| HostError::Rpc(e.0))
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

impl RealHost {
    /// Apply a config write that originates off the per-plugin RPC path — the
    /// on-box control socket (a GCS skill toggle / per-drone settings change for
    /// a plugin the writer is not). It resolves the per-drone scope via the same
    /// `agent_id_for` lookup `config.set` uses, so an operator's `active` flip
    /// lands in the exact per-drone namespace the plugin reads, and the store
    /// flushes its 0600 JSON on the set. The trust boundary is the control socket
    /// itself (on-box, owner+group); there is no capability token here. Returns
    /// the effective scope (`drone` collapses to `global` when no device id is
    /// bound, matching `ConfigStore::set`).
    pub fn apply_config_set(
        &self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: &str,
    ) -> Result<String, String> {
        if plugin_id.is_empty() {
            return Err("plugin_id must be a non-empty string".to_string());
        }
        if key.is_empty() {
            return Err("key must be a non-empty string".to_string());
        }
        if scope != "drone" && scope != "global" {
            return Err(format!(
                "scope must be drone or global, got {}",
                py_repr(scope)
            ));
        }
        let agent_id = self.agent_id_for(plugin_id);
        self.config
            .lock()
            .expect("config mutex poisoned")
            .set(plugin_id, key, value, scope, &agent_id);
        let effective = if scope == "drone" && agent_id.is_empty() {
            "global"
        } else {
            scope
        };
        Ok(effective.to_string())
    }
}

impl crate::control::ConfigControl for RealHost {
    fn apply_config_set(
        &self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: &str,
    ) -> Result<String, String> {
        RealHost::apply_config_set(self, plugin_id, key, value, scope)
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

    // ---- unimplemented-method / ungrantable-cap invariants ----------

    /// The method name carried in a `not_implemented` result, if it is one.
    fn not_implemented_method(r: &Result<HostResult, HostError>) -> Option<String> {
        let Ok(Value::Map(m)) = r else {
            return None;
        };
        let is_ni = m
            .iter()
            .find(|(k, _)| k.as_str() == Some("error"))
            .and_then(|(_, v)| v.as_str())
            == Some("not_implemented");
        if !is_ni {
            return None;
        }
        m.iter()
            .find(|(k, _)| k.as_str() == Some("method"))
            .and_then(|(_, v)| v.as_str())
            .map(str::to_string)
    }

    #[test]
    fn all_dispatch_methods_is_exhaustive() {
        // The local ALL_DISPATCH_METHODS list must cover every generated method
        // and carry no extras, so ungrantable_caps() reasons over the full set.
        use ados_protocol::dispatch::DISPATCH_METHODS;
        assert_eq!(ALL_DISPATCH_METHODS.len(), DISPATCH_METHODS.len());
        for row in DISPATCH_METHODS {
            assert!(
                ALL_DISPATCH_METHODS
                    .iter()
                    .any(|m| m.wire_name() == row.method),
                "generated method {} missing from ALL_DISPATCH_METHODS",
                row.method
            );
        }
    }

    #[test]
    fn unimplemented_methods_match_reality() {
        // Every method declared unimplemented must actually return the
        // not_implemented shape (with its own method name) from a freshly-built
        // host, with no MAVLink / vision client wired. The async vision methods
        // are exercised in their own test (they need an executor); the five
        // synchronous methods are checked directly here.
        let host = RealHost::new();
        let empty = Value::Map(vec![]);
        for method in RealHost::UNIMPLEMENTED_HOST_METHODS {
            use crate::dispatch::Method;
            let r = match method {
                Method::TelemetrySubscribe => host.telemetry_subscribe("p", &empty),
                Method::MissionRead => host.mission_read("p", &empty),
                Method::MissionWrite => host.mission_write("p", &empty),
                Method::RecordingStart => host.recording_start("p", &empty),
                Method::RecordingStop => host.recording_stop("p", &empty),
                other => panic!("unexpected method in the unimplemented list: {other:?}"),
            };
            assert_eq!(
                not_implemented_method(&r).as_deref(),
                Some(method.wire_name()),
                "{} must return not_implemented from a fresh host",
                method.wire_name()
            );
        }
    }

    #[test]
    fn implemented_methods_are_not_not_implemented() {
        // A representative set of WIRED host methods must NOT report
        // not_implemented even on a fresh host: telemetry.extend validates and
        // succeeds, config.get reads the store, camera.claim claims, and
        // process.spawn enforces the allowlist. If one of these regressed into a
        // stub, this would catch it and the ungrantable set would need updating.
        let host = RealHost::new();
        let extend = map(&[("channel", Value::from("c")), ("data", map(&[]))]);
        assert!(not_implemented_method(&host.telemetry_extend("p", &extend)).is_none());
        let cfg = map(&[("key", Value::from("k"))]);
        assert!(not_implemented_method(&host.config_get("p", &cfg)).is_none());
        let claim = map(&[("device_path", Value::from("/dev/video0"))]);
        assert!(not_implemented_method(&host.camera_claim("p", &claim)).is_none());
    }

    #[test]
    fn ungrantable_caps_are_the_dead_capabilities() {
        // The four caps that gate only the unimplemented methods, and nothing a
        // wired surface needs.
        let ungrantable = RealHost::ungrantable_caps();
        let expected: BTreeSet<String> = [
            "telemetry.read",
            "mission.read",
            "mission.write",
            "recording.write",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(ungrantable, expected);
        // A cap that gates a wired method (config has no cap; pick a real one)
        // must never be refused: mavlink.read gates mavlink.subscribe, which is
        // served via the stream method, and sensor.camera.register gates the
        // wired camera methods.
        assert!(!ungrantable.contains("mavlink.read"));
        assert!(!ungrantable.contains("sensor.camera.register"));
        assert!(!ungrantable.contains("vision.frame.read"));
    }

    #[tokio::test]
    async fn vision_methods_are_not_in_the_unimplemented_set() {
        // The vision request methods return not_implemented only while the engine
        // socket is down (a runtime availability state, like mavlink.send), so
        // their caps are NOT permanently ungrantable. Confirm they are absent
        // from the unimplemented list even though a fresh host (no engine) does
        // return not_implemented for them.
        use crate::dispatch::Method;
        assert!(!RealHost::UNIMPLEMENTED_HOST_METHODS.contains(&Method::VisionRegisterModel));
        assert!(!RealHost::UNIMPLEMENTED_HOST_METHODS.contains(&Method::VisionInfer));
        assert!(!RealHost::UNIMPLEMENTED_HOST_METHODS.contains(&Method::VisionPublishDetection));
        // And a fresh host does degrade them to not_implemented (the availability
        // posture), which is why they must be excluded by availability, not by
        // capability.
        let host = RealHost::new();
        let empty = Value::Map(vec![]);
        assert_eq!(
            not_implemented_method(&host.vision_infer("p", &empty).await).as_deref(),
            Some("vision.infer")
        );
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
    fn drone_scoped_config_isolates_per_agent_with_a_real_lookup() {
        // With a real agent-id lookup wired, a drone-scoped write lands under
        // that agent id and a different agent sees its own (or the default),
        // instead of every drone write collapsing to one global bucket.
        let host = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "drone-A".to_string()));
        host.config_set(
            "p",
            &map(&[
                ("key", Value::from("k")),
                ("value", Value::from("for-A")),
                ("scope", Value::from("drone")),
            ]),
        )
        .unwrap();
        // The store keyed it under the resolved agent id, not global.
        {
            let cfg = host.config.lock().unwrap();
            assert!(cfg.drone.contains_key(&(
                "p".to_string(),
                "drone-A".to_string(),
                "k".to_string()
            )));
            assert!(cfg.global.is_empty());
        }
        // A host bound to a different drone falls through to its default.
        let host_b = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "drone-B".to_string()));
        let g = ok_map(host_b.config_get(
            "p",
            &map(&[("key", Value::from("k")), ("default", Value::from("dflt"))]),
        ));
        assert_eq!(field(&g, "value").and_then(Value::as_str), Some("dflt"));
    }

    #[test]
    fn config_persists_and_reloads_across_a_restart() {
        // A persisted store flushes on set; a fresh store loaded from the same
        // path sees the prior value, proving config survives a plugin-host
        // restart (the in-memory-only store lost it).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin-config.json");

        let host = RealHost::new()
            .with_agent_id_lookup(Box::new(|_pid| "drone-A".to_string()))
            .with_config_persistence(path.clone());
        host.config_set(
            "p",
            &map(&[
                ("key", Value::from("k")),
                ("value", Value::from("kept")),
                ("scope", Value::from("drone")),
            ]),
        )
        .unwrap();
        // A global write too, so both scopes round-trip.
        host.config_set(
            "p",
            &map(&[
                ("key", Value::from("g")),
                ("value", Value::from("global-kept")),
                ("scope", Value::from("global")),
            ]),
        )
        .unwrap();
        assert!(path.exists(), "config.set must flush the store to disk");

        // A brand-new host loaded from the same path (a restart) sees both.
        let reborn = RealHost::new()
            .with_agent_id_lookup(Box::new(|_pid| "drone-A".to_string()))
            .with_config_persistence(path.clone());
        let drone = ok_map(reborn.config_get("p", &map(&[("key", Value::from("k"))])));
        assert_eq!(field(&drone, "value").and_then(Value::as_str), Some("kept"));
        let global = ok_map(reborn.config_get("p", &map(&[("key", Value::from("g"))])));
        assert_eq!(
            field(&global, "value").and_then(Value::as_str),
            Some("global-kept")
        );
    }

    #[cfg(unix)]
    #[test]
    fn persisted_config_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin-config.json");
        let host = RealHost::new().with_config_persistence(path.clone());
        host.config_set(
            "p",
            &map(&[("key", Value::from("k")), ("value", Value::from("v"))]),
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "persisted plugin config must be 0600");
    }

    #[tokio::test]
    async fn vision_methods_proxy_to_a_wired_engine() {
        // With a vision client wired to a fake engine socket, the three vision
        // methods proxy to it and return its response instead of the
        // not_implemented shape; without a client they stay not_implemented.
        use ados_protocol::frame::{encode_frame, PLUGIN_MAX_FRAME};
        use ados_protocol::ipc::IpcBroadcast;
        use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};

        // Unwired: returns the not_implemented shape (the slot is None).
        let bare = RealHost::new();
        let res = bare
            .vision_register_model("p", &Value::Map(vec![]))
            .await
            .unwrap();
        let m = match res {
            Value::Map(m) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            field(&m, "error").and_then(Value::as_str),
            Some("not_implemented")
        );

        // Wired: proxy to a fake engine that answers the next request.
        let mut sock = std::env::temp_dir();
        sock.push(format!("ados-realhost-vis-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let (server, _inbound) = IpcBroadcast::bind(&sock, 256, false, None).await.unwrap();
        let client = std::sync::Arc::new(VisionClient::connect(&sock).await.unwrap());
        let host = RealHost::new().with_vision(client);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let reply = Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: "response".to_string(),
            capability: String::new(),
            args: Value::Map(vec![(Value::from("registered"), Value::Boolean(true))]),
            request_id: "vis-1".to_string(),
            token: String::new(),
            error: None,
        };
        let body = reply.to_msgpack().unwrap();
        server
            .broadcast(encode_frame(&body, PLUGIN_MAX_FRAME).unwrap())
            .await;

        let res = host
            .vision_register_model(
                "p",
                &Value::Map(vec![(Value::from("model"), Value::from("m"))]),
            )
            .await
            .unwrap();
        let m = match res {
            Value::Map(m) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(field(&m, "registered").and_then(Value::as_bool), Some(true));
        let _ = std::fs::remove_file(&sock);
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

    // ---- display.page.set ------------------------------------------------

    #[test]
    fn display_page_set_writes_the_sidecar_in_the_shared_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-plugin-page.json");
        let host = RealHost::new().with_display_page_path(path.clone());

        let row = Value::Map(vec![
            (Value::from("label"), Value::from("Temp")),
            (Value::from("value"), Value::from("42 C")),
        ]);
        let zone = Value::Map(vec![
            (Value::from("x"), Value::from(8)),
            (Value::from("y"), Value::from(40)),
            (Value::from("w"), Value::from(100)),
            (Value::from("h"), Value::from(32)),
            (Value::from("key"), Value::from("reset")),
            (Value::from("label"), Value::from("Reset")),
        ]);
        let args = map(&[
            ("title", Value::from("Sensor")),
            ("rows", Value::Array(vec![row])),
            ("zones", Value::Array(vec![zone])),
        ]);

        let m = ok_map(host.display_page_set("p", &args));
        assert_eq!(field(&m, "set").and_then(Value::as_bool), Some(true));
        assert_eq!(field(&m, "rows").and_then(Value::as_i64), Some(1));
        assert_eq!(field(&m, "zones").and_then(Value::as_i64), Some(1));

        // The written JSON matches the shape the display loader reads.
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["title"], "Sensor");
        assert_eq!(v["rows"][0]["label"], "Temp");
        assert_eq!(v["rows"][0]["value"], "42 C");
        assert_eq!(v["zones"][0]["x"], 8);
        assert_eq!(v["zones"][0]["key"], "reset");
        assert_eq!(v["zones"][0]["label"], "Reset");

        // No stray tmp left behind.
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray);
    }

    #[test]
    fn display_page_set_is_lenient_and_rejects_a_misshaped_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-plugin-page.json");
        let host = RealHost::new().with_display_page_path(path.clone());

        // An empty payload writes an empty page.
        let m = ok_map(host.display_page_set("p", &map(&[])));
        assert_eq!(field(&m, "rows").and_then(Value::as_i64), Some(0));
        assert_eq!(field(&m, "zones").and_then(Value::as_i64), Some(0));
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["title"], "");

        // A non-list rows is a clear error.
        assert_eq!(
            err_body(host.display_page_set("p", &map(&[("rows", Value::from("nope"))]))),
            "rows must be a list"
        );
        assert_eq!(
            err_body(host.display_page_set("p", &map(&[("zones", Value::from(3))]))),
            "zones must be a list"
        );
    }

    #[test]
    fn display_page_set_is_gated_on_the_display_capability() {
        use crate::dispatch::{gate, Gate, Method};
        // The handler itself does not gate; the dispatch loop does. An ungranted
        // caller never reaches the handler.
        assert_eq!(
            gate("display.page.set", false, &caps(&[])),
            Gate::CapabilityDenied("capability_denied: display.oled.page".to_string())
        );
        assert_eq!(
            gate("display.page.set", false, &caps(&["display.oled.page"])),
            Gate::Allow(Method::DisplayPageSet)
        );
    }

    // ---- gpio.output.set / gpio.buzzer.beep ------------------------------

    /// Run a one-shot stub on a unix socket that captures the request line and
    /// replies with `reply`. Returns the captured request once a client connects.
    #[cfg(unix)]
    fn gpio_stub(path: std::path::PathBuf, reply: &'static str) -> std::thread::JoinHandle<String> {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        let listener = UnixListener::bind(&path).expect("bind stub socket");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 256];
            loop {
                let n = stream.read(&mut chunk).expect("read");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
            stream.write_all(reply.as_bytes()).expect("write reply");
            stream.write_all(b"\n").expect("write nl");
            stream.flush().ok();
            let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).to_string()
        })
    }

    #[cfg(unix)]
    #[test]
    fn gpio_output_set_forwards_a_set_request_and_returns_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpio-cmd.sock");
        let stub = gpio_stub(
            path.clone(),
            r#"{"ok":true,"chip":0,"pin":17,"level":"high"}"#,
        );
        let host = RealHost::new().with_gpio_cmd_path(path);

        let args = map(&[("pin", Value::from(17)), ("level", Value::from("high"))]);
        let m = ok_map(host.gpio_output_set("p", &args));
        // The reply round-trips back to the plugin verbatim.
        assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));
        assert_eq!(field(&m, "pin").and_then(Value::as_i64), Some(17));
        assert_eq!(field(&m, "level").and_then(Value::as_str), Some("high"));

        // The forwarded request carried the op + the validated fields.
        let sent = stub.join().unwrap();
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["op"], "set");
        assert_eq!(v["pin"], 17);
        assert_eq!(v["level"], "high");
        assert_eq!(v["chip"], 0);
    }

    #[cfg(unix)]
    #[test]
    fn gpio_buzzer_beep_forwards_the_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpio-cmd.sock");
        let stub = gpio_stub(path.clone(), r#"{"ok":true,"phases":4}"#);
        let host = RealHost::new().with_gpio_cmd_path(path);

        let args = map(&[
            ("pin", Value::from(18)),
            ("on_ms", Value::from(120)),
            ("off_ms", Value::from(80)),
            ("cycles", Value::from(2)),
            ("freq_hz", Value::from(2700)),
        ]);
        let m = ok_map(host.gpio_buzzer_beep("p", &args));
        assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));
        assert_eq!(field(&m, "phases").and_then(Value::as_i64), Some(4));

        let sent = stub.join().unwrap();
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["op"], "beep");
        assert_eq!(v["pin"], 18);
        assert_eq!(v["on_ms"], 120);
        assert_eq!(v["off_ms"], 80);
        assert_eq!(v["cycles"], 2);
        assert_eq!(v["freq_hz"], 2700);
    }

    #[test]
    fn gpio_set_validates_args_before_forwarding() {
        // A bad request fails validation in the host with no socket touched.
        let host = RealHost::new();
        assert_eq!(
            err_body(host.gpio_output_set("p", &map(&[("level", Value::from("high"))]))),
            "pin must be an integer"
        );
        assert_eq!(
            err_body(host.gpio_output_set("p", &map(&[("pin", Value::from(17))]))),
            "level must be \"high\" or \"low\""
        );
        assert_eq!(
            err_body(host.gpio_output_set(
                "p",
                &map(&[("pin", Value::from(17)), ("level", Value::from("mid"))])
            )),
            "level must be \"high\" or \"low\""
        );
        assert_eq!(
            err_body(host.gpio_buzzer_beep(
                "p",
                &map(&[("pin", Value::from(18)), ("cycles", Value::from(2))])
            )),
            "on_ms must be an integer"
        );
    }

    #[test]
    fn gpio_set_degrades_to_not_available_when_the_service_is_absent() {
        // No socket bound: the forward reports not_available, never errors.
        let host = RealHost::new()
            .with_gpio_cmd_path(std::path::PathBuf::from("/nonexistent/ados-gpio-test.sock"));
        let args = map(&[("pin", Value::from(17)), ("level", Value::from("high"))]);
        let m = ok_map(host.gpio_output_set("p", &args));
        assert_eq!(
            field(&m, "error").and_then(Value::as_str),
            Some("not_available")
        );
        assert_eq!(
            field(&m, "method").and_then(Value::as_str),
            Some("gpio.output.set")
        );
    }

    #[test]
    fn gpio_methods_are_gated_on_the_gpio_output_capability() {
        use crate::dispatch::{gate, Gate, Method};
        for (method, variant) in [
            ("gpio.output.set", Method::GpioOutputSet),
            ("gpio.buzzer.beep", Method::GpioBuzzerBeep),
        ] {
            assert_eq!(
                gate(method, false, &caps(&[])),
                Gate::CapabilityDenied("capability_denied: hardware.gpio_out".to_string())
            );
            assert_eq!(
                gate(method, false, &caps(&["hardware.gpio_out"])),
                Gate::Allow(variant)
            );
        }
    }

    // ---- radio.aux_stream.open / radio.aux_stream.close ------------------

    #[cfg(unix)]
    #[test]
    fn radio_aux_open_forwards_a_bare_open_and_records_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("radio-aux.sock");
        let stub = gpio_stub(
            path.clone(),
            r#"{"ok":true,"active":true,"tx_port":5602,"rx_port":5603}"#,
        );
        let host = RealHost::new().with_radio_aux_cmd_path(path);

        let m = ok_map(host.radio_aux_stream_open("p", &map(&[])));
        // The reply round-trips back to the plugin verbatim.
        assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));
        assert_eq!(field(&m, "active").and_then(Value::as_bool), Some(true));
        assert_eq!(field(&m, "tx_port").and_then(Value::as_i64), Some(5602));

        // The forwarded request is a bare open — the plugin cannot pick a port.
        let sent = stub.join().unwrap();
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["op"], "open");
        assert_eq!(v.as_object().unwrap().len(), 1);

        // Ownership recorded so a later disconnect closes the stream.
        assert_eq!(
            *host.aux_stream_owner.lock().unwrap(),
            Some("p".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn radio_aux_close_forwards_and_clears_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("radio-aux.sock");
        let host = RealHost::new().with_radio_aux_cmd_path(path.clone());
        // Seed ownership as if this plugin had opened the stream.
        *host.aux_stream_owner.lock().unwrap() = Some("p".to_string());

        let stub = gpio_stub(path, r#"{"ok":true,"active":false}"#);
        let m = ok_map(host.radio_aux_stream_close("p", &map(&[])));
        assert_eq!(field(&m, "active").and_then(Value::as_bool), Some(false));

        let sent = stub.join().unwrap();
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["op"], "close");

        // The owner record is cleared on a confirmed close.
        assert!(host.aux_stream_owner.lock().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn radio_aux_close_by_non_owner_leaves_the_owner_record_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("radio-aux.sock");
        let host = RealHost::new().with_radio_aux_cmd_path(path.clone());
        // Another plugin owns the stream.
        *host.aux_stream_owner.lock().unwrap() = Some("owner".to_string());

        let stub = gpio_stub(path, r#"{"ok":true,"active":false}"#);
        let _ = ok_map(host.radio_aux_stream_close("intruder", &map(&[])));
        stub.join().unwrap();

        // The real owner's record survives a close from a different plugin.
        assert_eq!(
            *host.aux_stream_owner.lock().unwrap(),
            Some("owner".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_plugin_closes_an_aux_stream_the_plugin_owned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("radio-aux.sock");
        let host = RealHost::new().with_radio_aux_cmd_path(path.clone());
        *host.aux_stream_owner.lock().unwrap() = Some("p".to_string());

        let stub = gpio_stub(path, r#"{"ok":true,"active":false}"#);
        host.release_plugin("p");
        // The disconnect forwarded a close (safe-by-default: the stream never
        // outlives its owner).
        let sent = stub.join().unwrap();
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["op"], "close");
        assert!(host.aux_stream_owner.lock().unwrap().is_none());
    }

    #[test]
    fn radio_aux_open_degrades_to_not_available_when_the_service_is_absent() {
        // No socket bound: the forward reports not_available, never errors, and
        // NO ownership is recorded (so a later disconnect forwards nothing).
        let host = RealHost::new()
            .with_radio_aux_cmd_path(std::path::PathBuf::from("/nonexistent/ados-aux-test.sock"));
        let m = ok_map(host.radio_aux_stream_open("p", &map(&[])));
        assert_eq!(
            field(&m, "error").and_then(Value::as_str),
            Some("not_available")
        );
        assert_eq!(
            field(&m, "method").and_then(Value::as_str),
            Some("radio.aux_stream.open")
        );
        // A failed open never claims ownership.
        assert!(host.aux_stream_owner.lock().unwrap().is_none());
    }

    #[test]
    fn radio_aux_methods_are_gated_on_the_aux_stream_capability() {
        use crate::dispatch::{gate, Gate, Method};
        for (method, variant) in [
            ("radio.aux_stream.open", Method::RadioAuxStreamOpen),
            ("radio.aux_stream.close", Method::RadioAuxStreamClose),
        ] {
            assert_eq!(
                gate(method, false, &caps(&[])),
                Gate::CapabilityDenied("capability_denied: radio.aux_stream".to_string())
            );
            assert_eq!(
                gate(method, false, &caps(&["radio.aux_stream"])),
                Gate::Allow(variant)
            );
        }
    }

    // ---- guided setpoint sender -----------------------------------------

    /// A pure-velocity local-NED setpoint request: ignore position / accel / yaw,
    /// command vx/vy/vz, body frame (8). Mirrors the builder's velocity_setpoint.
    fn velocity_args() -> Value {
        // type_mask: X|Y|Z | AX|AY|AZ | YAW | YAW_RATE = 1+2+4 +64+128+256 +1024+2048.
        let mask = 1 + 2 + 4 + 64 + 128 + 256 + 1024 + 2048;
        map(&[
            ("kind", Value::from("local_ned")),
            ("coordinate_frame", Value::from(8)), // MAV_FRAME_BODY_NED
            ("type_mask", Value::from(mask)),
            ("vx", Value::F64(2.5)),
            ("vy", Value::F64(-1.0)),
            ("vz", Value::F64(0.5)),
        ])
    }

    #[test]
    fn guided_setpoint_is_gated_on_the_capability() {
        use crate::dispatch::{gate, Gate, Method};
        assert_eq!(
            gate("flight.guided_setpoint.send", false, &caps(&[])),
            Gate::CapabilityDenied("capability_denied: flight.guided_setpoint".to_string())
        );
        assert_eq!(
            gate(
                "flight.guided_setpoint.send",
                false,
                &caps(&["flight.guided_setpoint"])
            ),
            Gate::Allow(Method::GuidedSetpointSend)
        );
    }

    #[test]
    fn guided_setpoint_without_router_degrades_to_not_available() {
        // No mavlink client wired: degrade, never error (the mavlink.send posture).
        let host = RealHost::new();
        let m = ok_map(host.guided_setpoint_send("p", &velocity_args()));
        assert_eq!(
            field(&m, "error").and_then(Value::as_str),
            Some("not_available")
        );
        assert_eq!(
            field(&m, "method").and_then(Value::as_str),
            Some("flight.guided_setpoint.send")
        );
    }

    #[test]
    fn guided_setpoint_validates_args_before_any_send() {
        let host = RealHost::new();
        // Missing kind.
        assert_eq!(
            err_body(host.guided_setpoint_send("p", &map(&[("type_mask", Value::from(0))]))),
            "kind must be \"local_ned\" or \"global_int\""
        );
        // Missing coordinate_frame.
        assert_eq!(
            err_body(host.guided_setpoint_send(
                "p",
                &map(&[
                    ("kind", Value::from("local_ned")),
                    ("type_mask", Value::from(0))
                ]),
            )),
            "coordinate_frame must be an integer"
        );
        // A NaN on an active axis (vx is active under this mask) is rejected by
        // the builder's validation.
        let mut bad = match velocity_args() {
            Value::Map(mut m) => {
                m.retain(|(k, _)| k.as_str() != Some("vx"));
                m.push((Value::from("vx"), Value::F64(f64::NAN)));
                Value::Map(m)
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            err_body(host.guided_setpoint_send("p", &bad)),
            "vx must be a finite number"
        );
        // A frame wrong for the kind (a global frame on a local message).
        bad = map(&[
            ("kind", Value::from("local_ned")),
            ("coordinate_frame", Value::from(5)), // MAV_FRAME_GLOBAL_INT
            ("type_mask", Value::from(0)),
        ]);
        assert!(err_body(host.guided_setpoint_send("p", &bad))
            .contains("not valid for this setpoint kind"));
        // A type_mask with an unknown high bit.
        bad = map(&[
            ("kind", Value::from("local_ned")),
            ("coordinate_frame", Value::from(8)),
            ("type_mask", Value::from(0x8000)),
        ]);
        assert!(err_body(host.guided_setpoint_send("p", &bad))
            .contains("outside the defined position-target field"));
    }

    #[tokio::test]
    async fn guided_setpoint_frame_reaches_the_router_and_decodes() {
        // With a live mavlink client wired to a stub router socket, the handler
        // builds the SET_POSITION_TARGET_LOCAL_NED (84) frame and writes it; the
        // router side reads it back and it decodes to the same message + fields.
        use crate::mavlink_client::{MavlinkClient, MAVLINK_BROADCAST_DEPTH};
        use ados_protocol::ipc::IpcBroadcast;
        use ados_protocol::mavlink::{ardupilotmega, parse_v2, MavMessage};

        let mut sock = std::env::temp_dir();
        sock.push(format!("ados-realhost-sp-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let (_server, inbound) =
            IpcBroadcast::bind(&sock, MAVLINK_BROADCAST_DEPTH, false, Some(16))
                .await
                .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");

        let client = std::sync::Arc::new(MavlinkClient::connect(&sock).await.unwrap());
        let host = RealHost::new().with_mavlink(client);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let m = ok_map(host.guided_setpoint_send("p", &velocity_args()));
        assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
        assert_eq!(field(&m, "msg_id").and_then(Value::as_i64), Some(84));

        // The router reads the raw frame; decode it and check the fields.
        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .expect("a frame arrives");
        let (_h, decoded) = parse_v2(&frame).expect("decode succeeds");
        match decoded {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(d) => {
                assert_eq!(d.vx, 2.5);
                assert_eq!(d.vy, -1.0);
                assert_eq!(d.vz, 0.5);
                assert_eq!(d.target_system, 1);
                assert_eq!(d.target_component, 1);
                assert_eq!(
                    d.coordinate_frame,
                    ardupilotmega::MavFrame::MAV_FRAME_BODY_NED
                );
            }
            other => panic!("expected SET_POSITION_TARGET_LOCAL_NED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn guided_setpoint_global_int_builds_and_decodes() {
        // A global-int setpoint with a commanded position decodes back to msg 86
        // with the scaled lat/lon and altitude intact.
        use crate::mavlink_client::{MavlinkClient, MAVLINK_BROADCAST_DEPTH};
        use ados_protocol::ipc::IpcBroadcast;
        use ados_protocol::mavlink::{parse_v2, MavMessage};

        let mut sock = std::env::temp_dir();
        sock.push(format!("ados-realhost-sp-gi-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let (_server, inbound) =
            IpcBroadcast::bind(&sock, MAVLINK_BROADCAST_DEPTH, false, Some(16))
                .await
                .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");

        let client = std::sync::Arc::new(MavlinkClient::connect(&sock).await.unwrap());
        let host = RealHost::new().with_mavlink(client);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Position + velocity: clear the X/Y/Z ignore bits, keep accel/yaw ignored.
        let mask = 64 + 128 + 256 + 1024 + 2048;
        let args = map(&[
            ("kind", Value::from("global_int")),
            ("coordinate_frame", Value::from(6)), // GLOBAL_RELATIVE_ALT_INT
            ("type_mask", Value::from(mask)),
            ("x", Value::F64(374_224_080.0)), // scaled latitude (37.422408 * 1e7)
            ("y", Value::F64(-1_220_842_700.0)),
            ("z", Value::F64(30.0)),
            ("vz", Value::F64(-0.5)),
            ("target_system", Value::from(2)),
        ]);
        let m = ok_map(host.guided_setpoint_send("p", &args));
        assert_eq!(field(&m, "msg_id").and_then(Value::as_i64), Some(86));

        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .expect("a frame arrives");
        match parse_v2(&frame).expect("decode succeeds").1 {
            MavMessage::SET_POSITION_TARGET_GLOBAL_INT(d) => {
                assert_eq!(d.lat_int, 374_224_080);
                assert_eq!(d.lon_int, -1_220_842_700);
                assert_eq!(d.alt, 30.0);
                assert_eq!(d.vz, -0.5);
                assert_eq!(d.target_system, 2, "target override is honoured");
            }
            other => panic!("expected SET_POSITION_TARGET_GLOBAL_INT, got {other:?}"),
        }
    }

    // ---- mavlink.tunnel.send --------------------------------------------

    #[test]
    fn mavlink_tunnel_send_is_gated_on_the_tunnel_capability() {
        use crate::dispatch::{gate, Gate, Method};
        // The plain mavlink.write does not satisfy the tunnel cap.
        assert_eq!(
            gate("mavlink.tunnel.send", false, &caps(&["mavlink.write"])),
            Gate::CapabilityDenied("capability_denied: mavlink.tunnel".to_string())
        );
        assert_eq!(
            gate("mavlink.tunnel.send", false, &caps(&["mavlink.tunnel"])),
            Gate::Allow(Method::MavlinkTunnelSend)
        );
    }

    #[test]
    fn mavlink_tunnel_send_without_router_degrades_to_not_available() {
        // No mavlink client wired: degrade, never error (the mavlink.send posture).
        let host = RealHost::new();
        let args = map(&[
            ("payload_type", Value::from(40001)),
            ("payload", Value::Binary(vec![1, 2, 3])),
        ]);
        let m = ok_map(host.mavlink_tunnel_send("p", &args));
        assert_eq!(
            field(&m, "error").and_then(Value::as_str),
            Some("not_available")
        );
        assert_eq!(
            field(&m, "method").and_then(Value::as_str),
            Some("mavlink.tunnel.send")
        );
    }

    #[test]
    fn mavlink_tunnel_send_validates_before_any_send() {
        let host = RealHost::new();
        // Missing payload_type.
        assert_eq!(
            err_body(host.mavlink_tunnel_send("p", &map(&[("payload", Value::Binary(vec![1]))]))),
            "payload_type is required"
        );
        // A registered (non-private) payload_type is refused by the builder.
        let registered = map(&[
            ("payload_type", Value::from(200)),
            ("payload", Value::Binary(vec![1])),
        ]);
        assert!(err_body(host.mavlink_tunnel_send("p", &registered)).contains("private type"));
        // A wrong payload type (a string) is rejected, not coerced.
        let bad_payload = map(&[
            ("payload_type", Value::from(40001)),
            ("payload", Value::from("not-bytes")),
        ]);
        assert_eq!(
            err_body(host.mavlink_tunnel_send("p", &bad_payload)),
            "payload must be bytes"
        );
        // An oversized payload is refused.
        let oversize = map(&[
            ("payload_type", Value::from(40001)),
            (
                "payload",
                Value::Binary(vec![0u8; ados_protocol::mavlink::TUNNEL_MAX_PAYLOAD + 1]),
            ),
        ]);
        assert!(err_body(host.mavlink_tunnel_send("p", &oversize)).contains("exceeds"));
    }

    #[tokio::test]
    async fn mavlink_tunnel_send_frame_reaches_the_router_and_round_trips_the_payload() {
        // With a live mavlink client wired to a stub router socket, the handler
        // builds the TUNNEL (385) frame and writes it; the router side reads it
        // back, the classifier recovers the private payload_type off the wire,
        // and the application payload round-trips byte-for-byte.
        use crate::mavlink_client::{MavlinkClient, MAVLINK_BROADCAST_DEPTH};
        use ados_protocol::ipc::IpcBroadcast;
        use ados_protocol::mavlink::{tunnel_payload_type, MSG_ID_TUNNEL};

        let mut sock = std::env::temp_dir();
        sock.push(format!("ados-realhost-tun-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let (_server, inbound) =
            IpcBroadcast::bind(&sock, MAVLINK_BROADCAST_DEPTH, false, Some(16))
                .await
                .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");

        let client = std::sync::Arc::new(MavlinkClient::connect(&sock).await.unwrap());
        let host = RealHost::new().with_mavlink(client);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let payload_type = 40001;
        let app_payload = b"opaque-app-bytes".to_vec();
        let args = map(&[
            ("payload_type", Value::from(payload_type)),
            ("payload", Value::Binary(app_payload.clone())),
            ("target_system", Value::from(2)),
        ]);
        let m = ok_map(host.mavlink_tunnel_send("p", &args));
        assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
        assert_eq!(
            field(&m, "payload_type").and_then(Value::as_i64),
            Some(payload_type as i64)
        );
        assert_eq!(
            field(&m, "payload_len").and_then(Value::as_i64),
            Some(app_payload.len() as i64)
        );

        // The router reads the raw frame; it is a TUNNEL carrying the private
        // type, and the application payload bytes survive verbatim.
        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .expect("a frame arrives");
        let mut id = [0u8; 4];
        id[..3].copy_from_slice(&frame[7..10]);
        assert_eq!(u32::from_le_bytes(id), MSG_ID_TUNNEL);
        assert_eq!(tunnel_payload_type(&frame), Some(payload_type));
        // TUNNEL wire layout after the 10-byte v2 header: payload_type (bytes
        // 10..12), target_system (12), target_component (13), payload_length
        // (14), then the payload at byte 15.
        assert_eq!(frame[12], 2, "target_system override rode through");
        assert_eq!(frame[14] as usize, app_payload.len());
        assert_eq!(&frame[15..15 + app_payload.len()], &app_payload[..]);

        let _ = std::fs::remove_file(&sock);
    }
}
