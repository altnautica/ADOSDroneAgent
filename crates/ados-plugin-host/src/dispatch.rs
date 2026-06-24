//! Method dispatch table and the capability gate.
//!
//! Mirrors the gate from the Python server's dispatch loop
//! (`src/ados/plugins/ipc_server.py`): re-check token expiry, look up the
//! method, then refuse an ungranted caller before the handler runs.
//!
//! The `method -> required_cap` mapping is NOT carried here. It is the
//! generated [`ados_protocol::dispatch::DISPATCH_METHODS`] const, the single
//! source of truth shared with the Python host (whose generated copy is
//! `src/ados/plugins/_dispatch_generated.py`). The enum below is the typed
//! handler handle the host routes on; [`Method::required_cap`] resolves through
//! the generated table by wire name so the two can never drift, and a test in
//! this module locks the enum's wire-name coverage to the generated set.
//!
//! `None` for the required cap means the method is either ungated (the event
//! surface and `ping`) or gated inline by the handler itself (the
//! component-kind / driver-kind / pose-inject classification). The inline
//! gates that depend on the request payload are surfaced in [`crate::handlers`]
//! once the matching host service is wired; until then the host-coupled bodies
//! return the `not_implemented` shape and the dispatch-level gate is the only
//! gate exercised, exactly as in the Python stubs.

use std::collections::BTreeSet;

use ados_protocol::dispatch::required_cap_for;
use ados_protocol::framebus::methods as vision_methods;

/// One dispatchable method. The variant set is exhaustive over the surfaces the
/// generated dispatch table covers: event publish/subscribe, ping, telemetry,
/// mission, recording, mavlink, peripheral/driver/camera, config, process
/// spawn, and the four vision methods.
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
    // Send one application payload over a MAVLink TUNNEL frame (a private
    // payload_type), a transparent opaque pipe on the existing link.
    MavlinkTunnelSend,
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
    // Display: set the reserved data-driven page's content.
    DisplayPageSet,
    // GPIO output: drive a status buzzer/LED line, or play a bounded beep.
    GpioOutputSet,
    GpioBuzzerBeep,
    // Flight: send one guided-mode position/velocity setpoint to the FC.
    GuidedSetpointSend,
    // Radio: open / close an additive auxiliary application stream on the link.
    RadioAuxStreamOpen,
    RadioAuxStreamClose,
    // Vision: frame-descriptor subscribe, model register, inference, and
    // detection publish. The engine owns the cameras and the inference backend;
    // the host proxies these to it over its socket.
    VisionSubscribeFrames,
    VisionRegisterModel,
    VisionInfer,
    VisionPublishDetection,
    VisionSubscribeDetections,
    VisionDesignateTrack,
}

impl Method {
    /// Resolve the wire method name to a [`Method`], matching the exact string
    /// keys the Python dispatch table uses. Returns `None` for an unknown
    /// method, which the gate maps to `unknown method <m>`.
    pub fn from_wire(name: &str) -> Option<Self> {
        // The vision method names are the shared constants in `ados-protocol`,
        // so match them against those rather than re-spelling the strings here.
        if name == vision_methods::SUBSCRIBE_FRAMES {
            return Some(Self::VisionSubscribeFrames);
        }
        if name == vision_methods::REGISTER_MODEL {
            return Some(Self::VisionRegisterModel);
        }
        if name == vision_methods::INFER {
            return Some(Self::VisionInfer);
        }
        if name == vision_methods::PUBLISH_DETECTION {
            return Some(Self::VisionPublishDetection);
        }
        if name == vision_methods::SUBSCRIBE_DETECTIONS {
            return Some(Self::VisionSubscribeDetections);
        }
        if name == vision_methods::DESIGNATE_TRACK {
            return Some(Self::VisionDesignateTrack);
        }
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
            "mavlink.tunnel.send" => Self::MavlinkTunnelSend,
            "mavlink.register_component" => Self::MavlinkRegisterComponent,
            "peripheral.register_driver" => Self::PeripheralRegisterDriver,
            "peripheral.unregister_driver" => Self::PeripheralUnregisterDriver,
            "camera.claim" => Self::CameraClaim,
            "camera.release" => Self::CameraRelease,
            "camera.get_frame" => Self::CameraGetFrame,
            "config.get" => Self::ConfigGet,
            "config.set" => Self::ConfigSet,
            "process.spawn" => Self::ProcessSpawn,
            "display.page.set" => Self::DisplayPageSet,
            "gpio.output.set" => Self::GpioOutputSet,
            "gpio.buzzer.beep" => Self::GpioBuzzerBeep,
            "flight.guided_setpoint.send" => Self::GuidedSetpointSend,
            "radio.aux_stream.open" => Self::RadioAuxStreamOpen,
            "radio.aux_stream.close" => Self::RadioAuxStreamClose,
            _ => return None,
        })
    }

    /// The exact wire string for this method, the inverse of [`Self::from_wire`].
    /// Used to resolve the dispatch-level cap through the generated table.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::EventPublish => "event.publish",
            Self::EventSubscribe => "event.subscribe",
            Self::Ping => "ping",
            Self::TelemetrySubscribe => "telemetry.subscribe",
            Self::TelemetryExtend => "telemetry.extend",
            Self::MissionRead => "mission.read",
            Self::MissionWrite => "mission.write",
            Self::RecordingStart => "recording.start",
            Self::RecordingStop => "recording.stop",
            Self::MavlinkSubscribe => "mavlink.subscribe",
            Self::MavlinkSend => "mavlink.send",
            Self::MavlinkTunnelSend => "mavlink.tunnel.send",
            Self::MavlinkRegisterComponent => "mavlink.register_component",
            Self::PeripheralRegisterDriver => "peripheral.register_driver",
            Self::PeripheralUnregisterDriver => "peripheral.unregister_driver",
            Self::CameraClaim => "camera.claim",
            Self::CameraRelease => "camera.release",
            Self::CameraGetFrame => "camera.get_frame",
            Self::ConfigGet => "config.get",
            Self::ConfigSet => "config.set",
            Self::ProcessSpawn => "process.spawn",
            Self::DisplayPageSet => "display.page.set",
            Self::GpioOutputSet => "gpio.output.set",
            Self::GpioBuzzerBeep => "gpio.buzzer.beep",
            Self::GuidedSetpointSend => "flight.guided_setpoint.send",
            Self::RadioAuxStreamOpen => "radio.aux_stream.open",
            Self::RadioAuxStreamClose => "radio.aux_stream.close",
            Self::VisionSubscribeFrames => vision_methods::SUBSCRIBE_FRAMES,
            Self::VisionRegisterModel => vision_methods::REGISTER_MODEL,
            Self::VisionInfer => vision_methods::INFER,
            Self::VisionPublishDetection => vision_methods::PUBLISH_DETECTION,
            Self::VisionSubscribeDetections => vision_methods::SUBSCRIBE_DETECTIONS,
            Self::VisionDesignateTrack => vision_methods::DESIGNATE_TRACK,
        }
    }

    /// The capability the dispatch loop requires before routing, or `None` for
    /// the ungated / inline-gated methods. Resolved through the generated
    /// [`ados_protocol::dispatch::DISPATCH_METHODS`] table by wire name, so the
    /// Rust and Python hosts share one source of truth.
    ///
    /// Every [`Method`] is a known generated method (locked by
    /// `enum_matches_generated_table`), so the outer lookup never misses; the
    /// `.flatten()` collapses a (theoretical, test-prevented) miss to `None` —
    /// the same value the live [`gate`] reads, with no runtime panic. The gate
    /// itself rejects unknown wire methods before this is ever reached, so a
    /// `None` here only ever means "no dispatch-level gate".
    pub fn required_cap(self) -> Option<&'static str> {
        required_cap_for(self.wire_name()).flatten()
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

    /// `allowlist_violation: <basename>` — emitted when a `process.spawn` names
    /// a basename outside the plugin's manifest `subprocess_spawn` allowlist.
    pub fn allowlist_violation(basename: &str) -> String {
        format!("allowlist_violation: {basename}")
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
    fn display_page_set_gates_on_the_display_capability() {
        // Refused without the cap, allowed with it.
        assert_eq!(
            gate("display.page.set", false, &caps(&[])),
            Gate::CapabilityDenied("capability_denied: display.oled.page".to_string())
        );
        assert_eq!(
            gate("display.page.set", false, &caps(&["display.oled.page"])),
            Gate::Allow(Method::DisplayPageSet)
        );
    }

    #[test]
    fn gpio_methods_gate_on_the_gpio_output_capability() {
        // Both the digital set and the beep pattern are refused without the
        // GPIO-output cap, allowed with it.
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

    #[test]
    fn radio_aux_stream_methods_gate_on_the_aux_stream_capability() {
        // Both open and close are refused without the aux-stream cap, allowed
        // with it. A plugin can never bring up an additive radio stream without
        // the operator-granted capability.
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

    #[test]
    fn guided_setpoint_send_gates_on_the_guided_setpoint_capability() {
        // Refused without the cap, allowed with it.
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
    fn mavlink_tunnel_send_gates_on_the_tunnel_capability() {
        // Refused without the tunnel cap (and the plain mavlink.write does not
        // satisfy it), allowed with it.
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
    fn process_spawn_gate_names_process_spawn_cap() {
        let g = gate("process.spawn", false, &caps(&["mavlink.read"]));
        assert_eq!(
            g,
            Gate::CapabilityDenied("capability_denied: process.spawn".to_string())
        );
    }

    /// Every variant the enum can produce. Kept exhaustive by the match in
    /// [`Method::wire_name`] (a new variant forces an arm there), so this list
    /// plus the wire-name mapping is the enum's full surface.
    const ALL_METHODS: &[Method] = &[
        Method::EventPublish,
        Method::EventSubscribe,
        Method::Ping,
        Method::TelemetrySubscribe,
        Method::TelemetryExtend,
        Method::MissionRead,
        Method::MissionWrite,
        Method::RecordingStart,
        Method::RecordingStop,
        Method::MavlinkSubscribe,
        Method::MavlinkSend,
        Method::MavlinkTunnelSend,
        Method::MavlinkRegisterComponent,
        Method::PeripheralRegisterDriver,
        Method::PeripheralUnregisterDriver,
        Method::CameraClaim,
        Method::CameraRelease,
        Method::CameraGetFrame,
        Method::ConfigGet,
        Method::ConfigSet,
        Method::ProcessSpawn,
        Method::DisplayPageSet,
        Method::GpioOutputSet,
        Method::GpioBuzzerBeep,
        Method::GuidedSetpointSend,
        Method::RadioAuxStreamOpen,
        Method::RadioAuxStreamClose,
        Method::VisionSubscribeFrames,
        Method::VisionRegisterModel,
        Method::VisionInfer,
        Method::VisionPublishDetection,
        Method::VisionSubscribeDetections,
        Method::VisionDesignateTrack,
    ];

    #[test]
    fn enum_matches_generated_table() {
        use ados_protocol::dispatch::DISPATCH_METHODS;

        // 1. Every enum variant round-trips through the wire name and resolves
        //    to a generated row whose required_cap matches. This is the
        //    security-critical direction: a Method present here but absent from
        //    the generated table would gate on None (open) silently — caught.
        for m in ALL_METHODS {
            let name = m.wire_name();
            assert_eq!(
                Method::from_wire(name),
                Some(*m),
                "wire name {name} does not round-trip to its variant"
            );
            let row = DISPATCH_METHODS
                .iter()
                .find(|r| r.method == name)
                .unwrap_or_else(|| panic!("method {name} missing from the generated table"));
            assert_eq!(
                m.required_cap(),
                row.required_cap,
                "required cap for {name} disagrees with the generated table"
            );
        }

        // 2. Every generated method is a known enum variant. A generated row the
        //    enum does not cover would be unroutable by the Rust host.
        for row in DISPATCH_METHODS {
            assert!(
                Method::from_wire(row.method).is_some(),
                "generated method {} has no enum variant",
                row.method
            );
        }

        // 3. Same cardinality, so neither side carries an extra row.
        assert_eq!(ALL_METHODS.len(), DISPATCH_METHODS.len());
    }

    #[test]
    fn vision_methods_are_gated_in_the_generated_table() {
        use ados_protocol::dispatch::DISPATCH_METHODS;
        // The exact gap this work closes: the four vision methods must carry a
        // non-None dispatch-level cap so no host (Rust or Python) can route them
        // ungated.
        for (name, cap) in [
            (vision_methods::SUBSCRIBE_FRAMES, "vision.frame.read"),
            (vision_methods::REGISTER_MODEL, "vision.model.register"),
            (vision_methods::INFER, "vision.model.register"),
            (
                vision_methods::PUBLISH_DETECTION,
                "vision.detection.publish",
            ),
            (
                vision_methods::SUBSCRIBE_DETECTIONS,
                "vision.detection.subscribe",
            ),
            (vision_methods::DESIGNATE_TRACK, "vision.track.designate"),
        ] {
            let row = DISPATCH_METHODS
                .iter()
                .find(|r| r.method == name)
                .expect("vision method present in the generated table");
            assert_eq!(row.required_cap, Some(cap), "gate for {name}");
        }
    }

    #[test]
    fn vision_wire_names_match_the_shared_constants() {
        assert_eq!(
            Method::from_wire(vision_methods::SUBSCRIBE_FRAMES),
            Some(Method::VisionSubscribeFrames)
        );
        assert_eq!(
            Method::from_wire(vision_methods::REGISTER_MODEL),
            Some(Method::VisionRegisterModel)
        );
        assert_eq!(
            Method::from_wire(vision_methods::INFER),
            Some(Method::VisionInfer)
        );
        assert_eq!(
            Method::from_wire(vision_methods::PUBLISH_DETECTION),
            Some(Method::VisionPublishDetection)
        );
    }

    #[test]
    fn vision_methods_gate_on_their_capability() {
        // Each vision method is refused without its cap and allowed with it.
        let g = gate(vision_methods::SUBSCRIBE_FRAMES, false, &caps(&[]));
        assert_eq!(
            g,
            Gate::CapabilityDenied("capability_denied: vision.frame.read".to_string())
        );
        assert_eq!(
            gate(
                vision_methods::SUBSCRIBE_FRAMES,
                false,
                &caps(&["vision.frame.read"])
            ),
            Gate::Allow(Method::VisionSubscribeFrames)
        );

        // register_model and infer share the model-register cap.
        for m in [vision_methods::REGISTER_MODEL, vision_methods::INFER] {
            assert_eq!(
                gate(m, false, &caps(&[])),
                Gate::CapabilityDenied("capability_denied: vision.model.register".to_string())
            );
            assert!(matches!(
                gate(m, false, &caps(&["vision.model.register"])),
                Gate::Allow(_)
            ));
        }

        assert_eq!(
            gate(vision_methods::PUBLISH_DETECTION, false, &caps(&[])),
            Gate::CapabilityDenied("capability_denied: vision.detection.publish".to_string())
        );
        assert_eq!(
            gate(
                vision_methods::PUBLISH_DETECTION,
                false,
                &caps(&["vision.detection.publish"])
            ),
            Gate::Allow(Method::VisionPublishDetection)
        );
    }

    #[test]
    fn follow_me_grant_set_is_sufficient_and_bounded() {
        // The reference click-to-follow plugin's exact agent grant set. Every
        // method it actually calls must be allowed, and the set must NOT reach
        // the guided-setpoint surface: the plugin emits its position targets
        // over plain mavlink.send (the mavlink.write cap), never the
        // higher-privilege flight.guided_setpoint cap it was not granted.
        let granted = caps(&[
            "vision.detection.subscribe",
            "mavlink.read",
            "mavlink.write",
        ]);

        // Allowed with its grants: detection stream, FC telemetry, FC send.
        assert!(matches!(
            gate(vision_methods::SUBSCRIBE_DETECTIONS, false, &granted),
            Gate::Allow(Method::VisionSubscribeDetections)
        ));
        assert!(matches!(
            gate("mavlink.subscribe", false, &granted),
            Gate::Allow(Method::MavlinkSubscribe)
        ));
        assert!(matches!(
            gate("mavlink.send", false, &granted),
            Gate::Allow(Method::MavlinkSend)
        ));

        // Bounded: it cannot reach the guided-setpoint surface even though it
        // holds mavlink.write — the two are distinct caps and the grant set
        // never included the flight one.
        assert_eq!(
            gate("flight.guided_setpoint.send", false, &granted),
            Gate::CapabilityDenied("capability_denied: flight.guided_setpoint".to_string())
        );

        // And a caller with no grant is refused at the same gate.
        assert_eq!(
            gate("mavlink.send", false, &caps(&[])),
            Gate::CapabilityDenied("capability_denied: mavlink.write".to_string())
        );
        assert_eq!(
            gate(vision_methods::SUBSCRIBE_DETECTIONS, false, &caps(&[])),
            Gate::CapabilityDenied("capability_denied: vision.detection.subscribe".to_string())
        );
    }
}
