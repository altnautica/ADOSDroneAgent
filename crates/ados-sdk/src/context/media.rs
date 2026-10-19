//! `ctx.video` and `ctx.sensors`: the relays the host carries for a plugin
//! whose sandbox cannot reach the media server or the control plane.

use std::sync::Arc;

use crate::client::{ClientError, PluginIpcClient, WhepAnswer};

/// `ctx.video` — WebRTC playback (WHEP) sessions on the agent's published
/// camera legs, for viewers the plugin serves. Needs `video.whep.relay`.
#[derive(Clone)]
pub struct VideoClient {
    pub(super) ipc: Arc<PluginIpcClient>,
}

impl VideoClient {
    /// Open a playback session on the configured camera `camera_id` for
    /// `offer_sdp`. See [`PluginIpcClient::whep_offer`].
    pub async fn whep_offer(
        &self,
        camera_id: &str,
        offer_sdp: &str,
    ) -> Result<WhepAnswer, ClientError> {
        self.ipc.whep_offer(camera_id, offer_sdp).await
    }

    /// Close a session this plugin opened, by its location. See
    /// [`PluginIpcClient::whep_close`].
    pub async fn whep_close(&self, location: &str) -> Result<(), ClientError> {
        self.ipc.whep_close(location).await
    }
}

/// `ctx.sensors` — the sensor rig document. Needs `sensors.rig.write`.
#[derive(Clone)]
pub struct SensorsClient {
    pub(super) ipc: Arc<PluginIpcClient>,
}

impl SensorsClient {
    /// Relays to core `PUT /api/v1/sensors/rig`; returns the core HTTP status
    /// and JSON body (`{"ok":true,"rig_id","calib_version"}` or
    /// `{"error":"E_RIG_INVALID","detail"}`). See [`PluginIpcClient::put_rig`].
    pub async fn put_rig(
        &self,
        rig: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), ClientError> {
        self.ipc.put_rig(rig).await
    }
}
