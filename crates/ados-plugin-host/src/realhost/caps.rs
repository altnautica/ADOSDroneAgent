//! The capabilities this host cannot serve, and the sidecar that publishes them.

use super::*;

impl RealHost {
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
    /// `telemetry.subscribe`, `mavlink.subscribe` and `vision.subscribe_frames`
    /// are deliberately absent: the server short-circuits each to the stream
    /// method this host overrides (`telemetry_state_stream`,
    /// `mavlink_subscribe_stream`, `vision_subscribe_stream`), so they never
    /// reach the not_implemented trait default.
    ///
    /// A `#[cfg(test)]` test (`unimplemented_methods_match_reality`) asserts this
    /// list is exactly the set of methods that return `not_implemented` from a
    /// freshly-built host, so it cannot silently drift as methods are wired.
    pub const UNIMPLEMENTED_HOST_METHODS: &'static [crate::dispatch::Method] = &[
        crate::dispatch::Method::MissionRead,
        crate::dispatch::Method::MissionWrite,
        crate::dispatch::Method::RecordingStart,
        crate::dispatch::Method::RecordingStop,
        // No capture pipeline feeds this host a camera buffer, and no
        // downstream service consumes a registered driver, so these answer
        // not_implemented instead of acknowledging a claim or registration
        // nothing acts on.
        crate::dispatch::Method::CameraClaim,
        crate::dispatch::Method::CameraRelease,
        crate::dispatch::Method::CameraGetFrame,
        crate::dispatch::Method::PeripheralRegisterDriver,
        crate::dispatch::Method::PeripheralUnregisterDriver,
    ];

    /// Capabilities checked inside `peripheral.register_driver` (the driver
    /// kind decides which) rather than by the dispatch table. With that method
    /// unimplemented they gate nothing on this host.
    pub(super) const DRIVER_KIND_CAPS: &'static [&'static str] = &[
        "sensor.camera.register",
        "sensor.depth.register",
        "sensor.imu.register",
        "sensor.lidar.register",
        "sensor.payload.register",
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
        let mut unimplemented: BTreeSet<&'static str> = Self::UNIMPLEMENTED_HOST_METHODS
            .iter()
            .filter_map(|m| m.required_cap())
            .collect();
        if Self::UNIMPLEMENTED_HOST_METHODS
            .contains(&crate::dispatch::Method::PeripheralRegisterDriver)
        {
            unimplemented.extend(Self::DRIVER_KIND_CAPS);
        }
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

/// File name, under the run dir, of the ungrantable-capability list the daemon
/// publishes for the other lifecycle controller (the LAN grant path), which
/// cannot compute [`RealHost::ungrantable_caps`] itself.
pub const UNGRANTABLE_CAPS_SIDECAR: &str = "plugin-ungrantable-caps.json";

/// Write `{"caps": [...]}` to `path` via temp-then-rename, world-readable like
/// the other `/run/ados` verdict sidecars.
pub fn write_ungrantable_caps(
    path: &std::path::Path,
    caps: &BTreeSet<String>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(&serde_json::json!({ "caps": caps }))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Every dispatch-level [`Method`](crate::dispatch::Method), so
/// [`RealHost::ungrantable_caps`] can subtract the caps gated by implemented
/// methods. Kept exhaustive by the match in
/// [`Method::wire_name`](crate::dispatch::Method::wire_name): a new variant
/// forces an arm there, and the `all_dispatch_methods_is_exhaustive` test locks
/// this list to the generated table's cardinality.
pub(super) const ALL_DISPATCH_METHODS: &[crate::dispatch::Method] = {
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
        MspSubscribe,
        MspSend,
        MavlinkTunnelSend,
        MavlinkRegisterComponent,
        PeripheralRegisterDriver,
        PeripheralUnregisterDriver,
        CameraClaim,
        CameraRelease,
        CameraGetFrame,
        VideoSourceSet,
        ConfigGet,
        ConfigSet,
        ProcessSpawn,
        DisplayPageSet,
        DisplayZoneSubscribe,
        GpioOutputSet,
        GpioBuzzerBeep,
        GuidedSetpointSend,
        RateSetpointSend,
        RadioAuxStreamOpen,
        RadioAuxStreamClose,
        RadioAuxStreamSend,
        RadioAuxStreamSubscribe,
        CloudPublish,
        CloudRecordsPut,
        OffloadAdvertise,
        VisionSubscribeFrames,
        VisionRegisterModel,
        VisionReadModel,
        VisionInfer,
        VisionPublishDetection,
        VisionSubscribeDetections,
        VisionDesignateTrack,
        ButtonSubscribe,
        ComputeDatasetWrite,
        ComputeJobSubmit,
        ComputeJobRead,
        ComputeJobOutputs,
        ComputeJobCancel,
        ComputeStreamOpen,
        ComputeStreamClose,
        ComputeStreamHealth,
    ]
};
