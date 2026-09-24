//! `cloud.publish` and `cloud.records.put`: a plugin's messages and records,
//! forwarded to the cloud relay's local publish socket.
//!
//! The plugin id on every request is the caller's verified identity, never an
//! argument, so a plugin publishes only under its own stream and collection
//! namespace. The relay stamps the device identity and the account.

use super::*;
use ados_protocol::cloud_publish::{
    self, CloudPublishError, CloudPublishRequest, VISION_DETECTIONS_STREAM,
};

/// The capability the shared detection stream additionally needs: it lands on
/// the core detection topic the GCS renders, not the plugin's own namespace.
const DETECTION_PUBLISH_CAP: &str = "vision.detection.publish";

fn string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, HostError> {
    map_get(args, key)
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::Rpc(format!("{key} missing or not a string")))
}

impl RealHost {
    /// Publish `{stream, payload}` on the caller's cloud stream.
    pub(super) async fn publish_to_cloud(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        let stream = string_arg(args, "stream")?;
        cloud_publish::validate_stream_name(stream).map_err(|e| HostError::Rpc(e.to_string()))?;
        if stream == VISION_DETECTIONS_STREAM && !granted_caps.contains(DETECTION_PUBLISH_CAP) {
            return Err(HostError::CapabilityDenied(
                DETECTION_PUBLISH_CAP.to_string(),
            ));
        }
        let payload = map_get(args, "payload")
            .ok_or_else(|| HostError::Rpc("payload missing".to_string()))
            .and_then(|v| {
                coerce_msg_bytes(v).map_err(|_| HostError::Rpc("payload must be bytes".to_string()))
            })?;
        self.send_to_cloud(
            CloudPublishRequest::stream(plugin_id, stream, payload),
            "cloud.publish",
        )
        .await
    }

    /// Write `{collection, key, data, device_id?}` into the caller's cloud
    /// records.
    pub(super) async fn put_cloud_record(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let collection = string_arg(args, "collection")?;
        let key = string_arg(args, "key")?;
        let data = map_get(args, "data")
            .ok_or_else(|| HostError::Rpc("data missing".to_string()))
            .and_then(|v| {
                serde_json::to_value(v)
                    .and_then(|json| serde_json::to_vec(&json))
                    .map_err(|e| HostError::Rpc(format!("data is not JSON-representable: {e}")))
            })?;
        let mut request = CloudPublishRequest::record(plugin_id, collection, key, data);
        // The record's subject device (e.g. the drone a workstation wrote a job
        // for); absent means this node. The relay validates the id.
        match map_get(args, "device_id") {
            None | Some(Value::Nil) => {}
            Some(v) => {
                let device_id = v.as_str().ok_or_else(|| {
                    HostError::Rpc("device_id must be a string or nil".to_string())
                })?;
                request = request.with_device_id(device_id);
            }
        }
        self.send_to_cloud(request, "cloud.records.put").await
    }

    /// One request/reply on the relay socket. A request the contract refuses
    /// (a name's grammar, an oversize payload) is an error to the plugin; a
    /// relay refusal is an error carrying the relay's reason; a relay that is
    /// not up degrades to the `not_available` shape, like every other forward.
    async fn send_to_cloud(
        &self,
        request: CloudPublishRequest,
        method: &str,
    ) -> Result<HostResult, HostError> {
        match cloud_publish::send(&self.cloud_publish_path, &request).await {
            Ok(reply) if reply.ok => {
                Ok(Value::Map(vec![(Value::from("ok"), Value::Boolean(true))]))
            }
            Ok(reply) => Err(HostError::Rpc(format!(
                "cloud relay refused: {}",
                reply.error.unwrap_or_default()
            ))),
            Err(CloudPublishError::Invalid(reason)) => Err(HostError::Rpc(reason)),
            Err(CloudPublishError::Timeout) => Ok(service_unavailable(
                method,
                "cloud relay did not answer in time",
            )),
            Err(e) => Ok(service_unavailable(
                method,
                &format!("cloud relay unavailable: {e}"),
            )),
        }
    }
}
