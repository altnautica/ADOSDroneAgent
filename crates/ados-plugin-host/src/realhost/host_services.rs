//! The [`HostServices`] implementation: one wire handler per host method.

mod aux_stream;
mod devices;
mod mavlink;
mod setpoints;
mod streams;
mod telemetry_config;
pub(super) mod vision_bind;
mod vision_rpc;

use super::*;

impl HostServices for RealHost {
    fn telemetry_extend(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.serve_telemetry_extend(_plugin_id, args)
    }

    fn telemetry_state_stream(&self, _plugin_id: &str) -> Option<broadcast::Receiver<Arc<Value>>> {
        self.serve_telemetry_state_stream(_plugin_id)
    }

    fn mavlink_send(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        self.serve_mavlink_send(plugin_id, args, granted_caps)
    }

    fn msp_send(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.serve_msp_send(_plugin_id, args)
    }

    fn mavlink_tunnel_send(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.serve_mavlink_tunnel_send(_plugin_id, args)
    }

    fn mavlink_register_component(
        &self,
        plugin_id: &str,
        session: u64,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        self.serve_mavlink_register_component(plugin_id, session, args, granted_caps)
    }

    fn config_get(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.serve_config_get(plugin_id, args)
    }

    async fn config_set(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.serve_config_set(plugin_id, args).await
    }

    fn process_spawn(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.serve_process_spawn(plugin_id, args)
    }

    fn display_page_set(
        &self,
        plugin_id: &str,
        session: u64,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_display_page_set(plugin_id, session, args)
    }

    async fn gpio_output_set(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_gpio_output_set(_plugin_id, args).await
    }

    async fn gpio_buzzer_beep(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_gpio_buzzer_beep(_plugin_id, args).await
    }

    async fn video_source_set(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_video_source_set(plugin_id, args).await
    }

    async fn radio_aux_stream_open(
        &self,
        plugin_id: &str,
        session: u64,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_radio_aux_stream_open(plugin_id, session, _args)
            .await
    }

    async fn radio_aux_stream_close(
        &self,
        plugin_id: &str,
        session: u64,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_radio_aux_stream_close(plugin_id, session, _args)
            .await
    }

    async fn offload_advertise(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.advertise_offload(plugin_id, args).await
    }

    async fn node_info(&self, _plugin_id: &str, _args: &Value) -> Result<HostResult, HostError> {
        self.read_node_info().await
    }

    async fn mdns_advertise(
        &self,
        plugin_id: &str,
        session: u64,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.advertise_mdns(plugin_id, session, args).await
    }

    async fn mdns_browse(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.browse_mdns(args).await
    }

    async fn whep_offer(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.relay_whep_offer(plugin_id, args).await
    }

    async fn whep_close(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.relay_whep_close(plugin_id, args).await
    }

    async fn sensors_rig_write(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.relay_rig_write(args).await
    }

    async fn cloud_publish(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        self.publish_to_cloud(plugin_id, args, granted_caps).await
    }

    async fn cloud_records_put(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.put_cloud_record(plugin_id, args).await
    }

    async fn radio_aux_stream_send(
        &self,
        plugin_id: &str,
        session: u64,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_radio_aux_stream_send(plugin_id, session, args)
            .await
    }

    fn radio_aux_stream_subscribe_stream(
        &self,
        plugin_id: &str,
        session: u64,
    ) -> Result<Option<crate::host::AuxDatagramRx>, HostError> {
        self.serve_radio_aux_stream_subscribe_stream(plugin_id, session)
    }

    fn aux_stream_owned_by(&self, plugin_id: &str, session: u64) -> bool {
        self.owns_aux_stream(plugin_id, session)
    }

    fn display_page_owned_by(&self, plugin_id: &str) -> bool {
        self.display_page_owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|(p, _)| p == plugin_id)
    }

    async fn release_plugin(&self, plugin_id: &str) {
        self.serve_release_plugin(plugin_id).await
    }

    fn guided_setpoint_send(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_guided_setpoint_send(_plugin_id, args)
    }

    fn rate_setpoint_send(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.serve_rate_setpoint_send(_plugin_id, args)
    }

    fn begin_session(&self, _plugin_id: &str) -> u64 {
        self.session_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    async fn release_session(&self, plugin_id: &str, session: u64) {
        self.serve_release_session(plugin_id, session).await
    }

    fn mavlink_subscribe_stream(
        &self,
        _plugin_id: &str,
        _msg_name: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.serve_mavlink_subscribe_stream(_plugin_id, _msg_name)
    }

    fn msp_subscribe_stream(&self, _plugin_id: &str) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.serve_msp_subscribe_stream(_plugin_id)
    }

    fn vision_subscribe_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.serve_vision_subscribe_stream(_plugin_id, _camera_id)
    }

    fn vision_subscribe_detection_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.serve_vision_subscribe_detection_stream(_plugin_id, _camera_id)
    }

    fn button_subscribe_stream(&self, _plugin_id: &str) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.serve_button_subscribe_stream(_plugin_id)
    }

    fn display_zone_tap_stream(&self, _plugin_id: &str) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.serve_display_zone_tap_stream(_plugin_id)
    }

    async fn vision_register_model(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_vision_register_model(plugin_id, args).await
    }

    async fn vision_read_model(
        &self,
        plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_vision_read_model(plugin_id, _args).await
    }

    async fn vision_infer(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        self.serve_vision_infer(plugin_id, args).await
    }

    async fn vision_publish_detection(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &std::collections::BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        self.serve_vision_publish_detection(plugin_id, args, granted_caps)
            .await
    }

    async fn vision_designate_track(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.serve_vision_designate_track(plugin_id, args).await
    }

    async fn camera_control_set(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.relay_camera_control(args).await
    }
}
