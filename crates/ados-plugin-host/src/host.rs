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

/// A msgpack map the dispatcher returns to the plugin as the response `args`.
pub type HostResult = Value;

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
pub trait HostServices: Send + Sync + 'static {
    fn telemetry_subscribe(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("telemetry.subscribe")
    }
    fn telemetry_extend(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("telemetry.extend")
    }
    fn mission_read(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("mission.read")
    }
    fn mission_write(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("mission.write")
    }
    fn recording_start(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("recording.start")
    }
    fn recording_stop(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("recording.stop")
    }
    fn mavlink_subscribe(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("mavlink.subscribe")
    }
    fn mavlink_send(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("mavlink.send")
    }
    fn mavlink_register_component(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("mavlink.register_component")
    }
    fn peripheral_register_driver(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("peripheral.register_driver")
    }
    fn peripheral_unregister_driver(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("peripheral.unregister_driver")
    }
    fn camera_claim(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("camera.claim")
    }
    fn camera_release(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("camera.release")
    }
    fn camera_get_frame(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("camera.get_frame")
    }
    fn config_get(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("config.get")
    }
    fn config_set(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("config.set")
    }
    fn process_spawn(&self, _plugin_id: &str, _args: &Value) -> HostResult {
        not_implemented("process.spawn")
    }

    /// Release every per-session host resource a plugin held when its
    /// connection drops (component reservations, driver registrations, camera
    /// claims, telemetry channels). Mirrors `_release_session_resources` in the
    /// Python server. The default is a no-op; a real host releases its state.
    fn release_plugin(&self, _plugin_id: &str) {}
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
        let result = host.mission_read("p", &Value::Map(vec![]));
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
}
