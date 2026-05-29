//! Method dispatch table and the capability gate.
//!
//! Ports `src/ados/plugins/ipc/dispatch.py` (the `method -> (handler,
//! required_cap)` table) and the gate from the Python server's dispatch loop
//! (`src/ados/plugins/ipc_server.py`): re-check token expiry, look up the
//! method, then refuse an ungranted caller before the handler runs.
//!
//! `None` for the required cap means the method is either ungated (the event
//! surface and `ping`) or gated inline by the handler itself (the
//! component-kind / driver-kind / pose-inject classification). The inline
//! gates that depend on the request payload are surfaced in [`crate::handlers`]
//! once the matching host service is wired; until then the host-coupled bodies
//! return the `not_implemented` shape and the dispatch-level gate is the only
//! gate exercised, exactly as in the Python stubs.

use std::collections::BTreeSet;

/// One dispatchable method. The variant set is exhaustive over the 12 surfaces
/// the Python dispatch table covers (`build_dispatch_table`):
/// event publish/subscribe, ping, telemetry, mission, recording, mavlink,
/// peripheral/driver/camera, config, and process spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    // Ungated event surface (per-topic check is inline in the handler).
    EventPublish,
    EventSubscribe,
    Ping,
    // Telemetry.
    TelemetrySubscribe,
    TelemetryExtend,
    // Mission / recording.
    MissionRead,
    MissionWrite,
    RecordingStart,
    RecordingStop,
    // MAVLink.
    MavlinkSubscribe,
    MavlinkSend,
    MavlinkRegisterComponent,
    // Peripheral / driver / camera.
    PeripheralRegisterDriver,
    PeripheralUnregisterDriver,
    CameraClaim,
    CameraRelease,
    CameraGetFrame,
    // Config kv.
    ConfigGet,
    ConfigSet,
    // Vendor binary spawn.
    ProcessSpawn,
}

impl Method {
    /// Resolve the wire method name to a [`Method`], matching the exact string
    /// keys the Python dispatch table uses. Returns `None` for an unknown
    /// method, which the gate maps to `unknown method <m>`.
    pub fn from_wire(name: &str) -> Option<Self> {
        Some(match name {
            "event.publish" => Self::EventPublish,
            "event.subscribe" => Self::EventSubscribe,
            "ping" => Self::Ping,
            "telemetry.subscribe" => Self::TelemetrySubscribe,
            "telemetry.extend" => Self::TelemetryExtend,
            "mission.read" => Self::MissionRead,
            "mission.write" => Self::MissionWrite,
            "recording.start" => Self::RecordingStart,
            "recording.stop" => Self::RecordingStop,
            "mavlink.subscribe" => Self::MavlinkSubscribe,
            "mavlink.send" => Self::MavlinkSend,
            "mavlink.register_component" => Self::MavlinkRegisterComponent,
            "peripheral.register_driver" => Self::PeripheralRegisterDriver,
            "peripheral.unregister_driver" => Self::PeripheralUnregisterDriver,
            "camera.claim" => Self::CameraClaim,
            "camera.release" => Self::CameraRelease,
            "camera.get_frame" => Self::CameraGetFrame,
            "config.get" => Self::ConfigGet,
            "config.set" => Self::ConfigSet,
            "process.spawn" => Self::ProcessSpawn,
            _ => return None,
        })
    }

    /// The capability the dispatch loop requires before routing, or `None` for
    /// the ungated / inline-gated methods. Mirrors the second tuple element of
    /// every row in `build_dispatch_table`.
    pub fn required_cap(self) -> Option<&'static str> {
        match self {
            // Ungated: per-topic check is inline; ping is open.
            Self::EventPublish | Self::EventSubscribe | Self::Ping => None,
            Self::TelemetrySubscribe => Some("telemetry.read"),
            Self::TelemetryExtend => Some("telemetry.extend"),
            Self::MissionRead => Some("mission.read"),
            Self::MissionWrite => Some("mission.write"),
            Self::RecordingStart | Self::RecordingStop => Some("recording.write"),
            Self::MavlinkSubscribe => Some("mavlink.read"),
            Self::MavlinkSend => Some("mavlink.write"),
            // Component kind cap is decided inline from the requested kind.
            Self::MavlinkRegisterComponent => None,
            // Sensor.*.register cap is decided inline from the driver kind.
            Self::PeripheralRegisterDriver => None,
            Self::PeripheralUnregisterDriver => None,
            Self::CameraClaim | Self::CameraRelease | Self::CameraGetFrame => {
                Some("sensor.camera.register")
            }
            // Per-drone / global config kv is ungated at the dispatch level.
            Self::ConfigGet | Self::ConfigSet => None,
            Self::ProcessSpawn => Some("process.spawn"),
        }
    }
}

/// The outcome of gating one request, before any handler runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The request may run the named method's handler.
    Allow(Method),
    /// The token aged past `expires_at`; reply with the exact `token_expired`
    /// error so the runner can request a fresh token.
    TokenExpired,
    /// No such method. The string carries the exact `unknown method <m>` body.
    UnknownMethod(String),
    /// The caller lacks the dispatch-level capability. The string carries the
    /// exact `capability_denied: <cap>` body.
    CapabilityDenied(String),
}

/// Error-body string constants and builders. These are the exact wire strings
/// the Python server emits, kept in one place so the server and the tests
/// agree byte-for-byte.
pub mod errors {
    /// `token_expired` — emitted when the session token aged past its expiry.
    pub const TOKEN_EXPIRED: &str = "token_expired";

    /// `unknown method <m>` — emitted for a method not in the table.
    pub fn unknown_method(method: &str) -> String {
        format!("unknown method {method}")
    }

    /// `capability_denied: <cap>` — emitted when the caller's granted caps do
    /// not include the method's required capability.
    pub fn capability_denied(cap: &str) -> String {
        format!("capability_denied: {cap}")
    }
}

/// Gate one request. `token_expired` is checked first (matching the Python
/// loop, which re-checks expiry before the method lookup), then the method is
/// resolved, then the dispatch-level capability is enforced.
///
/// `token_is_expired` is computed by the caller from the verified session token
/// and the current clock, so this function stays a pure decision over the wire
/// method name and the token's granted-cap set.
pub fn gate(method_name: &str, token_is_expired: bool, granted_caps: &BTreeSet<String>) -> Gate {
    if token_is_expired {
        return Gate::TokenExpired;
    }
    let Some(method) = Method::from_wire(method_name) else {
        return Gate::UnknownMethod(errors::unknown_method(method_name));
    };
    if let Some(cap) = method.required_cap() {
        if !granted_caps.contains(cap) {
            return Gate::CapabilityDenied(errors::capability_denied(cap));
        }
    }
    Gate::Allow(method)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn expired_token_short_circuits_before_method_lookup() {
        // Even a known, granted method must report token_expired first.
        let g = gate("ping", true, &caps(&[]));
        assert_eq!(g, Gate::TokenExpired);
    }

    #[test]
    fn unknown_method_produces_the_exact_body() {
        let g = gate("does.not.exist", false, &caps(&[]));
        assert_eq!(
            g,
            Gate::UnknownMethod("unknown method does.not.exist".to_string())
        );
    }

    #[test]
    fn ungated_methods_run_without_any_cap() {
        for m in ["ping", "event.publish", "event.subscribe", "config.get"] {
            assert!(matches!(gate(m, false, &caps(&[])), Gate::Allow(_)));
        }
    }

    #[test]
    fn ungranted_cap_produces_the_exact_capability_denied_body() {
        // mission.read requires the mission.read cap.
        let g = gate("mission.read", false, &caps(&[]));
        assert_eq!(
            g,
            Gate::CapabilityDenied("capability_denied: mission.read".to_string())
        );
    }

    #[test]
    fn granted_cap_allows_the_method() {
        let g = gate("mission.read", false, &caps(&["mission.read"]));
        assert_eq!(g, Gate::Allow(Method::MissionRead));
    }

    #[test]
    fn process_spawn_gate_names_process_spawn_cap() {
        let g = gate("process.spawn", false, &caps(&["mavlink.read"]));
        assert_eq!(
            g,
            Gate::CapabilityDenied("capability_denied: process.spawn".to_string())
        );
    }

    #[test]
    fn dispatch_table_matches_the_python_required_caps() {
        // Lock the (method, required_cap) table to the Python source of truth.
        let expected: &[(&str, Option<&str>)] = &[
            ("event.publish", None),
            ("event.subscribe", None),
            ("ping", None),
            ("telemetry.subscribe", Some("telemetry.read")),
            ("telemetry.extend", Some("telemetry.extend")),
            ("mission.read", Some("mission.read")),
            ("mission.write", Some("mission.write")),
            ("recording.start", Some("recording.write")),
            ("recording.stop", Some("recording.write")),
            ("mavlink.subscribe", Some("mavlink.read")),
            ("mavlink.send", Some("mavlink.write")),
            ("mavlink.register_component", None),
            ("peripheral.register_driver", None),
            ("peripheral.unregister_driver", None),
            ("camera.claim", Some("sensor.camera.register")),
            ("camera.release", Some("sensor.camera.register")),
            ("camera.get_frame", Some("sensor.camera.register")),
            ("config.get", None),
            ("config.set", None),
            ("process.spawn", Some("process.spawn")),
        ];
        for (name, cap) in expected {
            let method = Method::from_wire(name).expect("method in table");
            assert_eq!(method.required_cap(), *cap, "required cap for {name}");
        }
    }
}
