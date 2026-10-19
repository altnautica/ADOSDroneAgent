//! `ctx.cloud`, `ctx.node` and `ctx.mdns`: the facades that reach past the
//! plugin's sandbox to the cloud relay, the node facts and the local network.

use std::collections::BTreeMap;
use std::sync::Arc;

use rmpv::Value;

use crate::client::{ClientError, PluginIpcClient};

/// `ctx.cloud` — this plugin's own cloud stream and cloud records.
///
/// Both go through the agent's cloud relay under the plugin's verified id, so a
/// plugin only ever publishes into its own namespace. The relay being down is
/// the resting state on an unpaired or offline node: both calls then answer a
/// `not_available` map rather than an error.
#[derive(Clone)]
pub struct CloudClient {
    pub(super) ipc: Arc<PluginIpcClient>,
}

impl CloudClient {
    /// Publish one lossy message on the plugin's cloud stream `stream`. Needs
    /// `cloud.publish`. See [`PluginIpcClient::cloud_publish`].
    pub async fn publish(&self, stream: &str, payload: &[u8]) -> Result<Value, ClientError> {
        self.ipc.cloud_publish(stream, payload).await
    }

    /// Upsert the record `key` in the plugin's cloud collection `collection`,
    /// about `device_id` when given (else this node). Needs `cloud.records`.
    /// See [`PluginIpcClient::cloud_records_put`].
    pub async fn put_record(
        &self,
        collection: &str,
        key: &str,
        data: Value,
        device_id: Option<&str>,
    ) -> Result<Value, ClientError> {
        self.ipc
            .cloud_records_put(collection, key, data, device_id)
            .await
    }
}

/// `ctx.node` — facts about the node the plugin runs on.
///
/// The plugin's sandbox hides the agent's sidecars and config, so this is the
/// sanctioned read of what the agent itself decides on: the profile, the board
/// and its accelerators, the ground-station role, whether the camera pipeline
/// is up and the main stream's geometry.
#[derive(Clone)]
pub struct NodeClient {
    pub(super) ipc: Arc<PluginIpcClient>,
}

impl NodeClient {
    /// Read the node facts. Needs `node.info.read`. Each fact whose source is
    /// absent is `None`. See [`PluginIpcClient::node_info`].
    pub async fn info(&self) -> Result<ados_protocol::node_info::NodeInfo, ClientError> {
        self.ipc.node_info().await
    }
}

/// `ctx.mdns` — find and offer services on the local network.
///
/// The plugin's sandbox cannot bind the mDNS port, so the host runs the
/// responder: `advertise` publishes one of the plugin's declared listen ports
/// under the node's hostname for as long as this connection lives, and
/// `browse` collects the instances of one service type.
#[derive(Clone)]
pub struct MdnsClient {
    pub(super) ipc: Arc<PluginIpcClient>,
}

impl MdnsClient {
    /// Publish `port` as `service_type` with `txt`. Needs `network.listen`,
    /// and the port must be a declared listen port of a service that runs on
    /// this node. See [`PluginIpcClient::mdns_advertise`].
    pub async fn advertise(
        &self,
        service_type: &str,
        port: u16,
        txt: &BTreeMap<String, String>,
    ) -> Result<ados_protocol::plugin_mdns::Advertised, ClientError> {
        self.ipc.mdns_advertise(service_type, port, txt).await
    }

    /// Every instance of `service_type` answering within `window`. Needs
    /// `network.outbound`. See [`PluginIpcClient::mdns_browse`].
    pub async fn browse(
        &self,
        service_type: &str,
        window: std::time::Duration,
    ) -> Result<Vec<ados_protocol::plugin_mdns::DiscoveredService>, ClientError> {
        self.ipc.mdns_browse(service_type, window).await
    }
}
