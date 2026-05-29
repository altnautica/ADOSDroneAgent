//! Host-service facade the dispatcher routes through.
//!
//! The dispatcher does not talk to the agent's real services directly. It
//! talks to this small facade. Mirrors the Python `HostServices` dataclass in
//! `src/ados/plugins/ipc/host_services.py`: one orchestration object between
//! the IPC handler and the real modules (MAVLink router, peripheral registry,
//! telemetry pump, driver registries, config store) so the host code is
//! testable without booting the full agent.
//!
//! Capability checks happen in the dispatcher before the handler runs. The
//! facade does not re-check; it is a thin pass-through.
//!
//! Every method on [`HostServices`] returns a structured `not_implemented`
//! result by default, exactly mirroring the Python `_handle_*` stub bodies
//! (`{"error": "not_implemented", "method": <m>}`). The real host wiring
//! arrives when the supervisor and the MAVLink router expose stable hooks; it
//! is deliberately out of scope for this core crate. The event bus is the one
//! surface that is fully wired here, because it is an in-process fanout owned
//! by the host itself (it is not coupled to any external agent service), which
//! matches how the Python supervisor wires the `EventBus` directly rather than
//! behind a host-service hook.

use rmpv::Value;

use crate::dispatch::errors;

/// A msgpack map the dispatcher returns to the plugin as the response `args`.
pub type HostResult = Value;

/// A soft host-method failure that becomes the response envelope `error` field.
///
/// Mirrors the three exception types the Python dispatch loop converts to the
/// wire `error` string (`src/ados/plugins/ipc_server.py`): `_RpcError` (the
/// message verbatim), `CapabilityDenied` (`capability_denied: <cap>`, the
/// inline pose-inject / VIO-component / driver-kind / component-kind gates),
/// and `AllowlistViolation` (`allowlist_violation: <basename>`). [`body`](Self::body)
/// renders the exact wire string for each, so a Rust host is byte-identical to
/// the Python host. A real host returns these; [`NoopHost`] never does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// An arbitrary handler failure; the body is the message verbatim.
    Rpc(String),
    /// An inline capability gate refused the call; the body renders
    /// `capability_denied: <cap>` (the stored string is the capability).
    CapabilityDenied(String),
    /// A `process.spawn` basename outside the manifest allowlist; the body
    /// renders `allowlist_violation: <basename>` (the stored string is the
    /// basename).
    AllowlistViolation(String),
}

impl HostError {
    /// The exact wire `error` body the Python server emits for this failure.
    pub fn body(&self) -> String {
        match self {
            HostError::Rpc(msg) => msg.clone(),
            HostError::CapabilityDenied(cap) => errors::capability_denied(cap),
            HostError::AllowlistViolation(basename) => errors::allowlist_violation(basename),
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.body())
    }
}

impl std::error::Error for HostError {}

/// Build the `{"error": "not_implemented", "method": <method>}` result the
/// Python `_handle_*` stub bodies return verbatim, so a plugin sees the same
/// shape from the Rust host as it did from the Python host.
pub fn not_implemented(method: &str) -> HostResult {
    Value::Map(vec![
        (Value::from("error"), Value::from("not_implemented")),
        (Value::from("method"), Value::from(method)),
    ])
}

/// The slice of host services the dispatcher needs.
///
/// Each method takes the calling `plugin_id` and the request `args` (a msgpack
/// map) and returns a msgpack-map result. The default implementations return
/// [`not_implemented`] for the matching method, mirroring the Python stub
/// bodies; a real host implements the methods as the agent's service surfaces
/// stabilize. The event surface is not on this trait — it is served in-process
/// by the host's own event bus (see [`crate::handlers`]).
///
/// Three methods (`mavlink_send`, `mavlink_register_component`,
/// `peripheral_register_driver`) additionally take the caller's
/// `granted_caps`. They are the only methods whose capability gate is decided
/// from the request payload (the pose-inject / VIO-component classification, the
/// component kind, the driver kind), so the gate must run inside the handler,
/// after argument validation, exactly where the Python handlers apply it. The
/// other 14 methods are fully gated at the dispatch level and do not see the
/// caps. The asymmetry documents which methods gate on payload.
pub trait HostServices: Send + Sync + 'static {
    fn telemetry_subscribe(
        &self,
        _plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        Ok(not_implemented("telemetry.subscribe"))
    }
    fn telemetry_extend(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("telemetry.extend"))
    }
    fn mission_read(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("mission.read"))
    }
    fn mission_write(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("mission.write"))
    }
    fn recording_start(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("recording.start"))
    }
    fn recording_stop(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("recording.stop"))
    }
    fn mavlink_subscribe(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("mavlink.subscribe"))
    }
    /// Gates on the payload (pose-inject msg ids, VIO component id) after arg
    /// validation, so it takes the caller's `granted_caps`.
    fn mavlink_send(
        &self,
        _plugin_id: &str,
        _args: &Value,
        _granted_caps: &std::collections::BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        Ok(not_implemented("mavlink.send"))
    }
    /// Gates on the requested component kind after arg validation, so it takes
    /// the caller's `granted_caps`.
    fn mavlink_register_component(
        &self,
        _plugin_id: &str,
        _args: &Value,
        _granted_caps: &std::collections::BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        Ok(not_implemented("mavlink.register_component"))
    }
    /// Gates on the requested driver kind after arg validation, so it takes the
    /// caller's `granted_caps`.
    fn peripheral_register_driver(
        &self,
        _plugin_id: &str,
        _args: &Value,
        _granted_caps: &std::collections::BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        Ok(not_implemented("peripheral.register_driver"))
    }
    fn peripheral_unregister_driver(
        &self,
        _plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        Ok(not_implemented("peripheral.unregister_driver"))
    }
    fn camera_claim(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("camera.claim"))
    }
    fn camera_release(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("camera.release"))
    }
    fn camera_get_frame(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("camera.get_frame"))
    }
    fn config_get(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("config.get"))
    }
    fn config_set(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("config.set"))
    }
    fn process_spawn(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        Ok(not_implemented("process.spawn"))
    }

    /// Release every per-session host resource a plugin held when its
    /// connection drops (component reservations, driver registrations, camera
    /// claims, telemetry channels). Mirrors `_release_session_resources` in the
    /// Python server. The default is a no-op; a real host releases its state.
    fn release_plugin(&self, _plugin_id: &str) {}

    /// A receiver for the MAVLink frame fanout, when this host has a wired
    /// MAVLink client. The server obtains one per `mavlink.subscribe` and pushes
    /// each frame to the plugin as a `mavlink.deliver` envelope. Mirrors the
    /// pump-subscription seam in `src/ados/plugins/ipc/mavlink_pump.py`, where
    /// the host's MAVLink router exposes a per-subscriber frame queue.
    ///
    /// The default returns `None`, which keeps [`NoopHost`] unaffected (no push
    /// stream). A real host returns a receiver when its MAVLink slot is wired and
    /// `None` when the router has not surfaced yet (the Python pump logs
    /// `router_missing` and does nothing in that case).
    fn mavlink_subscribe_stream(
        &self,
        _plugin_id: &str,
        _msg_name: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<Vec<u8>>> {
        None
    }
}

/// The default host: every host-coupled method returns `not_implemented`.
///
/// Mirrors `default_host_services()` in Python, which leaves the MAVLink router
/// and runtime lookups unwired until the supervisor surfaces them. The
/// handshake and the capability gate are fully exercised against this host; the
/// only thing it does not do is the real service work, which is the intended
/// boundary for this core crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHost;

impl HostServices for NoopHost {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_host_returns_not_implemented_with_the_method_name() {
        let host = NoopHost;
        let result = host
            .mission_read("p", &Value::Map(vec![]))
            .expect("NoopHost methods never error");
        let map = match result {
            Value::Map(m) => m,
            other => panic!("expected map, got {other:?}"),
        };
        // {"error": "not_implemented", "method": "mission.read"}
        let error = map
            .iter()
            .find(|(k, _)| k.as_str() == Some("error"))
            .and_then(|(_, v)| v.as_str());
        let method = map
            .iter()
            .find(|(k, _)| k.as_str() == Some("method"))
            .and_then(|(_, v)| v.as_str());
        assert_eq!(error, Some("not_implemented"));
        assert_eq!(method, Some("mission.read"));
    }

    #[test]
    fn host_error_renders_the_exact_python_wire_bodies() {
        // These strings are the contract SDK clients string-match on
        // (e.g. error.startswith("allowlist_violation:")). Lock them.
        assert_eq!(HostError::Rpc("boom".to_string()).body(), "boom");
        assert_eq!(
            HostError::CapabilityDenied("mavlink.write".to_string()).body(),
            "capability_denied: mavlink.write"
        );
        assert_eq!(
            HostError::AllowlistViolation("ffmpeg".to_string()).body(),
            "allowlist_violation: ffmpeg"
        );
    }
}
