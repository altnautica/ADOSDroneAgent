//! The host-coupled method router.

use super::*;

/// Route a host-coupled method to its [`HostServices`] hook. The event surface,
/// `ping`, and the streaming subscribe methods (`telemetry.subscribe`,
/// `mavlink.subscribe`, `vision.subscribe_frames`, ...) are handled in the
/// server before this is reached (they arm a push stream); this routes the
/// remaining host-coupled methods. With the [`NoopHost`](crate::host::NoopHost)
/// every one returns `Ok(not_implemented(...))`, mirroring the Python stub
/// bodies; a real host returns [`Err(HostError)`](HostError) for a soft failure,
/// which the server renders into the response envelope `error` field.
///
/// Async because the vision, command-socket (GPIO, video, radio aux), cloud
/// relay and config-write methods await a socket or file work; the
/// in-process methods complete synchronously.
///
/// `granted_caps` is the caller's verified capability set. Only the
/// payload-gated methods (`mavlink.send`, `mavlink.register_component`,
/// `peripheral.register_driver`, `cloud.publish` for its detection stream, and
/// `vision.publish_detection` for a batch's tracker fields) consume it; they
/// apply their capability gate
/// inside the handler, after argument validation, exactly where the Python
/// handlers apply it. The other methods are fully gated at the dispatch level and
/// ignore it.
///
/// `session` is the calling connection's session (see
/// [`HostServices::begin_session`]). The methods that hold per-connection
/// state (a component reservation, an mDNS record, the aux stream, the
/// display page) key it on this, since one plugin may hold several
/// connections at once (its main process and each declared service).
pub async fn route_host_method<H: HostServices + ?Sized>(
    host: &H,
    method: Method,
    plugin_id: &str,
    session: u64,
    args: &Value,
    granted_caps: &BTreeSet<String>,
) -> Result<HostResult, HostError> {
    match method {
        Method::TelemetryExtend => host.telemetry_extend(plugin_id, args),
        Method::MissionRead => host.mission_read(plugin_id, args),
        Method::MissionWrite => host.mission_write(plugin_id, args),
        Method::RecordingStart => host.recording_start(plugin_id, args),
        Method::RecordingStop => host.recording_stop(plugin_id, args),
        Method::MavlinkSubscribe => host.mavlink_subscribe(plugin_id, args),
        Method::MavlinkSend => host.mavlink_send(plugin_id, args, granted_caps),
        // msp.send forwards raw MSP bytes to the FC; the dispatch-level msp.write
        // cap is the whole gate, so no granted_caps inline check.
        Method::MspSend => host.msp_send(plugin_id, args),
        Method::MavlinkTunnelSend => host.mavlink_tunnel_send(plugin_id, args),
        Method::MavlinkRegisterComponent => {
            host.mavlink_register_component(plugin_id, session, args, granted_caps)
        }
        Method::PeripheralRegisterDriver => {
            host.peripheral_register_driver(plugin_id, args, granted_caps)
        }
        Method::PeripheralUnregisterDriver => host.peripheral_unregister_driver(plugin_id, args),
        Method::CameraClaim => host.camera_claim(plugin_id, args),
        Method::CameraRelease => host.camera_release(plugin_id, args),
        Method::CameraGetFrame => host.camera_get_frame(plugin_id, args),
        Method::VideoSourceSet => host.video_source_set(plugin_id, args).await,
        Method::ConfigGet => host.config_get(plugin_id, args),
        Method::ConfigSet => host.config_set(plugin_id, args).await,
        Method::ProcessSpawn => host.process_spawn(plugin_id, args),
        Method::DisplayPageSet => host.display_page_set(plugin_id, session, args),
        Method::GpioOutputSet => host.gpio_output_set(plugin_id, args).await,
        Method::GpioBuzzerBeep => host.gpio_buzzer_beep(plugin_id, args).await,
        Method::GuidedSetpointSend => host.guided_setpoint_send(plugin_id, args),
        Method::RateSetpointSend => host.rate_setpoint_send(plugin_id, args),
        Method::FlightLeaseAcquire => host.flight_lease_acquire(plugin_id, args).await,
        Method::FlightLeaseRelease => host.flight_lease_release(plugin_id, args),
        Method::FlightGoalSend => host.flight_goal_send(plugin_id, args),
        Method::FlightSticksSubscribe => host.flight_sticks_subscribe(plugin_id, args),
        Method::RadioAuxStreamOpen => host.radio_aux_stream_open(plugin_id, session, args).await,
        Method::RadioAuxStreamClose => {
            host.radio_aux_stream_close(plugin_id, session, args).await
        }
        Method::RadioAuxStreamSend => host.radio_aux_stream_send(plugin_id, session, args).await,
        // Subscribe is handled in the server (it arms the per-connection aux
        // push stream) and never reaches here, exactly like button.subscribe.
        Method::RadioAuxStreamSubscribe => {
            Ok(crate::host::not_implemented("radio.aux_stream.subscribe"))
        }
        // Cloud relay: forward to the relay's local publish socket. A publish
        // on the shared detection stream is checked inline against
        // `vision.detection.publish`, so this one consumes `granted_caps`.
        Method::CloudPublish => host.cloud_publish(plugin_id, args, granted_caps).await,
        Method::CloudRecordsPut => host.cloud_records_put(plugin_id, args).await,
        Method::OffloadAdvertise => host.offload_advertise(plugin_id, args).await,
        Method::NodeInfo => host.node_info(plugin_id, args).await,
        Method::MdnsAdvertise => host.mdns_advertise(plugin_id, session, args).await,
        Method::MdnsBrowse => host.mdns_browse(plugin_id, args).await,
        Method::WhepOffer => host.whep_offer(plugin_id, args).await,
        Method::WhepClose => host.whep_close(plugin_id, args).await,
        Method::RigWrite => host.sensors_rig_write(plugin_id, args).await,
        // Vision request/response methods proxy to the engine and await its
        // reply. (vision.subscribe_frames is handled in the server, where it
        // arms the frame-descriptor push stream, never reaching here.)
        Method::VisionRegisterModel => host.vision_register_model(plugin_id, args).await,
        // Reads the plugin's own resolved model status off the install record —
        // the one vision method that does not proxy to the engine.
        Method::VisionReadModel => host.vision_read_model(plugin_id, args).await,
        Method::VisionInfer => host.vision_infer(plugin_id, args).await,
        // The publish binds the batch to the plugin; whether it may carry a
        // tracker lock depends on its `vision.track.designate` grant.
        Method::VisionPublishDetection => {
            host.vision_publish_detection(plugin_id, args, granted_caps)
                .await
        }
        Method::VisionDesignateTrack => host.vision_designate_track(plugin_id, args).await,
        Method::VisionRequestDetection => host.vision_request_detection(plugin_id, args).await,
        Method::CameraControlSet => host.camera_control_set(plugin_id, args).await,
        // The event surface, ping, and the streaming subscribe methods never
        // reach here; the server short-circuits `vision.subscribe_frames`,
        // `vision.subscribe_detections` and `button.subscribe`, arming the
        // per-connection push streams before they could route to the facade.
        // Reaching this arm is a programming error guarded by a stable response.
        Method::EventPublish
        | Method::EventSubscribe
        | Method::Ping
        | Method::VisionSubscribeFrames
        | Method::VisionSubscribeDetections
        | Method::MspSubscribe
        | Method::ButtonSubscribe
        // display.zone.subscribe arms the tap push stream in the server (like
        // button.subscribe) and never reaches the facade.
        | Method::DisplayZoneSubscribe
        // telemetry.subscribe arms the paced vehicle-state push in the server.
        | Method::TelemetrySubscribe => Ok(crate::host::not_implemented("event")),
    }
}
