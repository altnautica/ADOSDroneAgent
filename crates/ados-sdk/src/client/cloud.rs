//! Cloud publish, records and offload advertisement calls.

use super::*;

impl PluginIpcClient {
    /// Publish one message on this plugin's own cloud stream.
    ///
    /// The host forwards `{stream, payload}` to the cloud relay under the
    /// caller's verified plugin id, which publishes it at QoS 0 on
    /// `ados/{device_id}/plugin/{plugin_id}/{stream}`. `stream` must match
    /// `[a-z0-9][a-z0-9._-]{0,63}` and `payload` is opaque bytes of at most
    /// [`ados_protocol::cloud_publish::MAX_PAYLOAD`]. The stream
    /// [`ados_protocol::cloud_publish::VISION_DETECTIONS_STREAM`] instead lands
    /// on the core detection topic and additionally needs
    /// `vision.detection.publish`. Gated on `cloud.publish`.
    ///
    /// The reply is `{ok: true}` once the relay queued the message (the lane is
    /// lossy, so this is not broker delivery), or a `not_available` map while
    /// the relay is down.
    pub async fn cloud_publish(&self, stream: &str, payload: &[u8]) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("stream"), Value::from(stream)),
            (Value::from("payload"), Value::Binary(payload.to_vec())),
        ]);
        Ok(self
            .send_request("cloud.publish", "cloud.publish", args)
            .await?
            .args)
    }

    /// Upsert one record `{collection, key, data}` into this plugin's own cloud
    /// records in the operator's account.
    ///
    /// `collection` must match `[a-z0-9_.-]{1,64}`, `key` is 1..=256 UTF-8
    /// bytes, and `data` must be JSON-representable (the host re-encodes it as
    /// JSON, at most [`ados_protocol::cloud_publish::MAX_PAYLOAD`]). The record's
    /// subject is `device_id` when given (e.g. the drone a workstation wrote a
    /// job for), otherwise this node. Gated on `cloud.records`.
    ///
    /// The reply is `{ok: true}` only once the cloud accepted the write, so this
    /// call waits past the relay's record bound rather than the default request
    /// timeout; a `not_available` map comes back while the relay is down.
    pub async fn cloud_records_put(
        &self,
        collection: &str,
        key: &str,
        data: Value,
        device_id: Option<&str>,
    ) -> Result<Value, ClientError> {
        let mut args = vec![
            (Value::from("collection"), Value::from(collection)),
            (Value::from("key"), Value::from(key)),
            (Value::from("data"), data),
        ];
        if let Some(device_id) = device_id {
            args.push((Value::from("device_id"), Value::from(device_id)));
        }
        let args = Value::Map(args);
        let timeout = self
            .request_timeout
            .max(ados_protocol::cloud_publish::RECORD_REPLY_TIMEOUT + RECORD_REPLY_SLACK);
        Ok(self
            .send_request_within("cloud.records.put", "cloud.records", args, timeout)
            .await?
            .args)
    }

    /// Report the perception-offload link this plugin holds.
    ///
    /// The host writes it as the offload-link sidecar, which goes stale
    /// [`ados_protocol::offload_link::OFFLOAD_LINK_STALE_MS`] (20 s) after the
    /// last write, so a plugin keeps an active link counted by re-advertising at
    /// least every ~10 s, and advertises `paired: false` as soon as the link
    /// drops rather than waiting out the window. The optional strings must be
    /// 1..=256 printable bytes. The host stamps `paired` only when `target`
    /// accepts a TCP connection, and refuses the call while another plugin's
    /// link is fresh. Gated on `vision.offload.advertise`. The reply is
    /// `{ok: true, paired: <as stamped>}`.
    pub async fn offload_advertise(
        &self,
        advert: &OffloadAdvertisement,
    ) -> Result<Value, ClientError> {
        let opt = |v: &Option<String>| v.as_deref().map_or(Value::Nil, Value::from);
        let args = Value::Map(vec![
            (Value::from("paired"), Value::Boolean(advert.paired)),
            (
                Value::from("bearer_acceptable"),
                Value::Boolean(advert.bearer_acceptable),
            ),
            (Value::from("target"), opt(&advert.target)),
            (Value::from("device_id"), opt(&advert.device_id)),
            (Value::from("model_id"), opt(&advert.model_id)),
        ]);
        Ok(self
            .send_request("offload.advertise", "vision.offload.advertise", args)
            .await?
            .args)
    }
}
