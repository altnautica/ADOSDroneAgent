//! Runtime client impls: events, radio, tools, config, process, lifecycle, and context construction.

use super::*;

impl EventsClient {
    /// Publish a payload on a topic. Returns the delivered count.
    pub async fn publish(&self, topic: &str, payload: Value) -> Result<i64, ClientError> {
        self.ipc.event_publish(topic, payload).await
    }

    /// Subscribe to a topic pattern; the callback fires for each matched
    /// delivery with the event's `args` map (`topic`, `payload`, `publisher`,
    /// `timestamp_ms`).
    pub async fn subscribe(
        &self,
        topic_pattern: &str,
        callback: EventCallback,
    ) -> Result<(), ClientError> {
        self.ipc.event_subscribe(topic_pattern, callback).await
    }
}

impl RadioClient {
    /// Open the auxiliary stream.
    pub async fn open_aux_stream(&self) -> Result<Value, ClientError> {
        self.ipc.radio_aux_stream_open().await
    }

    /// Close the auxiliary stream.
    pub async fn close_aux_stream(&self) -> Result<Value, ClientError> {
        self.ipc.radio_aux_stream_close().await
    }

    /// Send one application datagram on the open auxiliary stream.
    ///
    /// `channel` must be the reserved app channel this plugin's direction uses
    /// (`AppStream` 8 or `AppCommand` 9); the host refuses any other. The payload
    /// is opaque to the host; a payload over the aux-frame maximum is refused
    /// before it is sent.
    pub async fn send_aux(&self, channel: u8, payload: &[u8]) -> Result<Value, ClientError> {
        self.ipc.radio_aux_stream_send(channel, payload).await
    }

    /// Subscribe to application datagrams received on the open auxiliary stream.
    ///
    /// The callback receives each `(channel, payload)` the host forwards; the
    /// plugin's codec routes by channel. A stream with no inbound frames never
    /// fires the callback — the resting state, not an error. Register before
    /// subscribing so no early frame is missed.
    pub async fn subscribe_aux<F: Fn(u8, Vec<u8>) + Send + Sync + 'static>(
        &self,
        cb: F,
    ) -> Result<(), ClientError> {
        let on_deliver = move |args: Value| {
            let Some(m) = args.as_map() else { return };
            let ch = m
                .iter()
                .find(|(k, _)| k.as_str() == Some("channel"))
                .and_then(|(_, v)| v.as_u64())
                .unwrap_or(0) as u8;
            let pl = match m
                .iter()
                .find(|(k, _)| k.as_str() == Some("payload"))
                .map(|(_, v)| v)
            {
                Some(Value::Binary(b)) => b.clone(),
                _ => Vec::new(),
            };
            cb(ch, pl);
        };
        self.ipc.register_aux_callback(Arc::new(on_deliver));
        self.ipc.radio_aux_stream_subscribe().await.map(|_| ())
    }
}

impl ToolsClient {
    /// Answer every `tool.invoke` with `handler`. Registration is local: no
    /// host request is made.
    pub fn serve(&self, handler: crate::client::ToolHandler) {
        self.ipc.set_tool_handler(handler);
    }
}

impl ConfigClient {
    /// Read a key from the manifest-supplied static config; synchronous.
    pub fn static_get(&self, key: &str) -> Option<&Value> {
        self.static_config.get(key)
    }

    /// Read a key from the host's live kv, falling back to `default`.
    pub async fn get(&self, key: &str, default: Value) -> Result<Value, ClientError> {
        self.ipc.config_get(key, default).await
    }

    /// Write a key to the host's live kv in the given scope (`drone`/`global`).
    pub async fn set(&self, key: &str, value: Value, scope: &str) -> Result<Value, ClientError> {
        self.ipc.config_set(key, value, scope).await
    }
}

impl ProcessClient {
    /// Authorize a vendor-binary spawn. The host enforces the manifest
    /// allowlist and returns the resolved install dir; the actual exec is the
    /// plugin's to perform so the child inherits the runner's cgroup slice.
    pub async fn spawn(
        &self,
        basename: &str,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Result<Value, ClientError> {
        self.ipc.process_spawn(basename, args, env).await
    }
}

impl LifecycleClient {
    pub async fn on_pause(&self, handler: EventCallback) -> Result<(), ClientError> {
        let topic = format!("plugin.{}.lifecycle.pause", self.ipc.plugin_id());
        self.ipc.event_subscribe(&topic, handler).await
    }

    pub async fn on_resume(&self, handler: EventCallback) -> Result<(), ClientError> {
        let topic = format!("plugin.{}.lifecycle.resume", self.ipc.plugin_id());
        self.ipc.event_subscribe(&topic, handler).await
    }
}

impl PluginContext {
    /// Build a context over a connected client. `static_config` is the
    /// manifest-supplied config dict the runner read at start.
    pub fn new(
        ipc: Arc<PluginIpcClient>,
        plugin_version: impl Into<String>,
        agent_id: impl Into<String>,
        data_dir: Option<String>,
        static_config: BTreeMap<String, Value>,
    ) -> Self {
        let plugin_id = ipc.plugin_id().to_string();
        let peripheral_manager = PeripheralClient { ipc: ipc.clone() };
        Self {
            plugin_id,
            plugin_version: plugin_version.into(),
            agent_id: agent_id.into(),
            data_dir: data_dir.map(std::path::PathBuf::from),
            events: EventsClient { ipc: ipc.clone() },
            mavlink: MavlinkClient { ipc: ipc.clone() },
            msp: MspClient { ipc: ipc.clone() },
            telemetry: TelemetryClient { ipc: ipc.clone() },
            peripherals: peripheral_manager.clone(),
            peripheral_manager,
            camera: CameraClient { ipc: ipc.clone() },
            vision: VisionClient::new(ipc.clone()),
            buttons: ButtonClient { ipc: ipc.clone() },
            flight: FlightClient { ipc: ipc.clone() },
            gpio: GpioClient { ipc: ipc.clone() },
            display: DisplayClient { ipc: ipc.clone() },
            radio: RadioClient { ipc: ipc.clone() },
            cloud: CloudClient { ipc: ipc.clone() },
            node: NodeClient { ipc: ipc.clone() },
            mdns: MdnsClient { ipc: ipc.clone() },
            video: VideoClient { ipc: ipc.clone() },
            sensors: SensorsClient { ipc: ipc.clone() },
            config: ConfigClient {
                ipc: ipc.clone(),
                static_config: Arc::new(static_config),
            },
            process: ProcessClient { ipc: ipc.clone() },
            lifecycle: LifecycleClient { ipc: ipc.clone() },
            tools: ToolsClient { ipc: ipc.clone() },
            ipc,
        }
    }

    /// Health probe against the host. Mirrors `ping_supervisor`.
    pub async fn ping_supervisor(&self) -> Result<Value, ClientError> {
        self.ipc.ping().await
    }

    /// The shared client, for advanced callers that need a method not yet on a
    /// facade.
    pub fn client(&self) -> &Arc<PluginIpcClient> {
        &self.ipc
    }
}
