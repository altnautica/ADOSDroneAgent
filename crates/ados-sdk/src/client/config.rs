//! Config, process, node-info and mDNS calls.

use super::*;

impl PluginIpcClient {
    /// Read a config key, returning the host's `value` (nil when absent).
    /// Mirrors `config_get`.
    pub async fn config_get(&self, key: &str, default: Value) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("key"), Value::from(key)),
            (Value::from("default"), default),
        ]);
        let resp = self.send_request("config.get", "", args).await?;
        Ok(map_get(&resp.args, "value").unwrap_or(Value::Nil))
    }

    /// Write a config key in the given scope (`drone` or `global`). Mirrors
    /// `config_set`.
    pub async fn config_set(
        &self,
        key: &str,
        value: Value,
        scope: &str,
    ) -> Result<Value, ClientError> {
        let args = Value::Map(vec![
            (Value::from("key"), Value::from(key)),
            (Value::from("value"), value),
            (Value::from("scope"), Value::from(scope)),
        ]);
        Ok(self.send_request("config.set", "", args).await?.args)
    }

    /// Authorize a vendor-binary spawn through the supervisor. The host enforces
    /// the manifest allowlist and audit-logs the attempt. Mirrors
    /// `process_spawn`. The required cap is `process.spawn`.
    pub async fn process_spawn(
        &self,
        basename: &str,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Result<Value, ClientError> {
        let args_val = Value::Array(args.into_iter().map(Value::from).collect());
        let env_val = Value::Map(
            env.into_iter()
                .map(|(k, v)| (Value::from(k), Value::from(v)))
                .collect(),
        );
        let payload = Value::Map(vec![
            (Value::from("basename"), Value::from(basename)),
            (Value::from("args"), args_val),
            (Value::from("env"), env_val),
        ]);
        Ok(self
            .send_request("process.spawn", "process.spawn", payload)
            .await?
            .args)
    }
}

impl PluginIpcClient {
    /// Read this node's facts: profile, board (with NPU presence), the
    /// ground-station role, camera readiness and the main stream's geometry.
    /// Each absent source is `None`, never a guess. Gated on `node.info.read`.
    ///
    /// A host that predates the method answers `not_implemented`, surfaced as
    /// [`ClientError::Rpc`]`("not_implemented: node.info")`.
    pub async fn node_info(&self) -> Result<NodeInfo, ClientError> {
        use ados_protocol::node_info::{CAPABILITY, METHOD};
        let args = self
            .send_request(METHOD, CAPABILITY, Value::Map(Vec::new()))
            .await?
            .args;
        if let Some(error) = map_get_str(&args, "error") {
            return Err(ClientError::Rpc(format!("{error}: {METHOD}")));
        }
        rmpv::ext::from_value(args)
            .map_err(|e| ClientError::Rpc(format!("{METHOD} reply did not decode: {e}")))
    }

    /// Publish one of this plugin's listen ports on the local network over
    /// mDNS, as `service_type` (`_<name>._tcp`) with `txt`.
    ///
    /// The host runs the responder: a sandboxed plugin cannot bind the
    /// multicast port. `port` must be a listen port the plugin declares for a
    /// service that runs on this node, and the agent's own service types
    /// (`_ados._tcp`, `_ados-receiver._tcp`) are refused. The SRV target is the
    /// system hostname. The record lives as long as this connection: the host
    /// withdraws it when the connection ends, and calling again with the same
    /// service type replaces it (a TXT update). Gated on `network.listen`.
    pub async fn mdns_advertise(
        &self,
        service_type: &str,
        port: u16,
        txt: &std::collections::BTreeMap<String, String>,
    ) -> Result<ados_protocol::plugin_mdns::Advertised, ClientError> {
        use ados_protocol::plugin_mdns::{ADVERTISE_CAPABILITY, ADVERTISE_METHOD};
        let txt = txt
            .iter()
            .map(|(k, v)| (Value::from(k.as_str()), Value::from(v.as_str())))
            .collect();
        let args = Value::Map(vec![
            (Value::from("service_type"), Value::from(service_type)),
            (Value::from("port"), Value::from(port)),
            (Value::from("txt"), Value::Map(txt)),
        ]);
        let args = self
            .send_request(ADVERTISE_METHOD, ADVERTISE_CAPABILITY, args)
            .await?
            .args;
        decode_typed_reply(args, ADVERTISE_METHOD)
    }

    /// Every instance of `service_type` (`_<name>._tcp` or `_<name>._udp`)
    /// that answers on the local network within `window` (clamped by the host
    /// to 0.1-5 s). The call returns once the window has closed. Gated on
    /// `network.outbound`.
    pub async fn mdns_browse(
        &self,
        service_type: &str,
        window: Duration,
    ) -> Result<Vec<ados_protocol::plugin_mdns::DiscoveredService>, ClientError> {
        use ados_protocol::plugin_mdns::{
            BrowseReply, BROWSE_CAPABILITY, BROWSE_MAX_TIMEOUT_MS, BROWSE_METHOD,
        };
        let window_ms = u32::try_from(window.as_millis()).unwrap_or(u32::MAX);
        let args = Value::Map(vec![
            (Value::from("service_type"), Value::from(service_type)),
            (Value::from("timeout_ms"), Value::from(window_ms)),
        ]);
        // The host answers after its window, so the wait must outlast the
        // longest one it will run.
        let timeout = self.request_timeout.max(
            Duration::from_millis(u64::from(window_ms.min(BROWSE_MAX_TIMEOUT_MS)))
                + MDNS_BROWSE_REPLY_SLACK,
        );
        let args = self
            .send_request_within(BROWSE_METHOD, BROWSE_CAPABILITY, args, timeout)
            .await?
            .args;
        decode_typed_reply::<BrowseReply>(args, BROWSE_METHOD).map(|r| r.services)
    }

    // Internals
}
