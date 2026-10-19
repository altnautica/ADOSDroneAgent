//! The live-video and rig relays: `video.whep.offer`, `video.whep.close` and
//! `sensors.rig.write`. The argument maps carry the field names of
//! [`ados_protocol::media_relay`]'s request types, which the host decodes.

use ados_protocol::media_relay::{
    RigWriteReply, WhepOfferReply, RIG_CAPABILITY, RIG_WRITE_METHOD, WHEP_CAPABILITY,
    WHEP_CLOSE_METHOD, WHEP_OFFER_METHOD,
};

use super::*;

/// How long a relay call waits for the host. The host bounds each upstream
/// exchange (connect plus answer) under this, so its own error arrives first.
const RELAY_TIMEOUT: Duration = Duration::from_secs(12);

/// The answer to a WHEP offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhepAnswer {
    /// The SDP answer for the viewer.
    pub sdp: String,
    /// The opaque session location to pass to [`PluginIpcClient::whep_close`];
    /// `None` when the media server named none.
    pub location: Option<String>,
}

fn args(entries: &[(&str, &str)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(k, v)| (Value::from(*k), Value::from(*v)))
            .collect(),
    )
}

impl PluginIpcClient {
    /// Open a WebRTC playback (WHEP) session on the configured camera
    /// `camera_id` (its roster id) for a viewer's `offer_sdp`, and return the
    /// answer. The host resolves the camera to the media leg publishing it.
    /// Gated on `video.whep.relay` ([`ClientError::CapabilityDenied`] without
    /// it). An id that is not a configured, enabled camera is
    /// [`ClientError::Rpc`]`("unknown_camera: ..")`; the media server refusing
    /// the offer or being down is [`ClientError::Rpc`]`("whep_unavailable: ..")`.
    pub async fn whep_offer(
        &self,
        camera_id: &str,
        offer_sdp: &str,
    ) -> Result<WhepAnswer, ClientError> {
        let args = args(&[("camera_id", camera_id), ("sdp", offer_sdp)]);
        let reply = self
            .send_request_within(WHEP_OFFER_METHOD, WHEP_CAPABILITY, args, RELAY_TIMEOUT)
            .await?;
        let reply: WhepOfferReply = decode_typed_reply(reply.args, WHEP_OFFER_METHOD)?;
        Ok(WhepAnswer {
            sdp: reply.sdp,
            location: reply.location,
        })
    }

    /// Close a session [`whep_offer`](Self::whep_offer) opened for this plugin,
    /// by the location it returned. Any other location is refused as
    /// [`ClientError::Rpc`]`("whep_unavailable: ..")`.
    pub async fn whep_close(&self, location: &str) -> Result<(), ClientError> {
        let args = args(&[("location", location)]);
        let reply = self
            .send_request_within(WHEP_CLOSE_METHOD, WHEP_CAPABILITY, args, RELAY_TIMEOUT)
            .await?;
        if let Some(error) = map_get_str(&reply.args, "error") {
            return Err(ClientError::Rpc(format!("{error}: {WHEP_CLOSE_METHOD}")));
        }
        Ok(())
    }

    /// Store `rig` through the agent's `PUT /api/v1/sensors/rig` and return
    /// the route's HTTP status and JSON body (`{"ok":true,"rig_id",
    /// "calib_version"}`, or `{"error":"E_RIG_INVALID","detail"}` with 400).
    /// Gated on `sensors.rig.write`. The route being unreachable is
    /// [`ClientError::Rpc`]`("rig_unavailable: ..")`.
    pub async fn put_rig(
        &self,
        rig: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), ClientError> {
        let args = args(&[("rig_json", rig.to_string().as_str())]);
        let reply = self
            .send_request_within(RIG_WRITE_METHOD, RIG_CAPABILITY, args, RELAY_TIMEOUT)
            .await?;
        let reply: RigWriteReply = decode_typed_reply(reply.args, RIG_WRITE_METHOD)?;
        let body = serde_json::from_str(&reply.body_json)
            .map_err(|e| ClientError::Rpc(format!("{RIG_WRITE_METHOD} answer is not JSON: {e}")))?;
        Ok((reply.status, body))
    }
}
