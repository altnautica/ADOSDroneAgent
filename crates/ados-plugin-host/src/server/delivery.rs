//! Push deliveries and the per-connection subscription state.

use super::*;

impl<H: HostServices> Connection<H> {
    /// Push a matched event to the plugin as an `event.deliver` envelope.
    pub(super) async fn deliver_event<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        event: &Event,
    ) -> Result<(), ServerError> {
        if !Self::holds(token, Method::EventSubscribe) {
            return Ok(());
        }
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: "event.deliver".to_string(),
            capability: "event.subscribe".to_string(),
            args: handlers::event_deliver_args(event),
            request_id: format!("evt-{}", event.timestamp_ms),
            token: token.to_token_string(),
            error: None,
        };
        write_frame(write_half, &env).await
    }

    /// Push one vehicle-state snapshot to the plugin as an `event.deliver` on
    /// [`TELEMETRY_STATE_TOPIC`], in the shape every event takes (`topic`,
    /// `payload`, `publisher`, `timestamp_ms`), so the SDK routes it like any
    /// other event. The payload is the state object itself. A token rotated
    /// without `telemetry.read` stops the stream at the next snapshot.
    pub(super) async fn deliver_telemetry_state<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        snapshot: &Value,
    ) -> Result<(), ServerError> {
        const READ_CAP: &str = "telemetry.read";
        if !token.granted_caps.contains(READ_CAP) {
            return Ok(());
        }
        let ts = now_ms();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: "event.deliver".to_string(),
            capability: READ_CAP.to_string(),
            args: Value::Map(vec![
                (Value::from("topic"), Value::from(TELEMETRY_STATE_TOPIC)),
                (Value::from("payload"), snapshot.clone()),
                (
                    Value::from("publisher"),
                    Value::from(crate::vehicle_events::HOST_PUBLISHER),
                ),
                (Value::from("timestamp_ms"), Value::Integer(ts.into())),
            ]),
            request_id: format!("tel-{ts}"),
            token: token.to_token_string(),
            error: None,
        };
        write_frame(write_half, &env).await
    }

    /// Push a subscribed MAVLink frame to the plugin as a `mavlink.deliver`
    /// envelope. The shape is fixed by the SDK's frame subscriber:
    /// kind `event`, method `mavlink.deliver`, capability `mavlink.read`, args
    /// `{msg_name, frame: bytes, timestamp_ms}`, request_id `mav-<ms>`.
    pub(super) async fn deliver_mavlink<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        msg_name: &str,
        frame: &[u8],
    ) -> Result<(), ServerError> {
        if !Self::holds(token, Method::MavlinkSubscribe) {
            return Ok(());
        }
        let ts = now_ms();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: "mavlink.deliver".to_string(),
            capability: "mavlink.read".to_string(),
            args: Value::Map(vec![
                (Value::from("msg_name"), Value::from(msg_name)),
                (Value::from("frame"), Value::Binary(frame.to_vec())),
                (Value::from("timestamp_ms"), Value::Integer(ts.into())),
            ]),
            request_id: format!("mav-{ts}"),
            token: token.to_token_string(),
            error: None,
        };
        write_frame(write_half, &env).await
    }

    /// Push a vision frame descriptor to the plugin as a `vision.deliver`
    /// envelope. Mirrors the `mavlink.deliver` shape: kind `event`, method
    /// `vision.deliver`, capability `vision.frame.read`, args `{descriptor:
    /// bytes, timestamp_ms}`, request_id `vis-<ms>`. The `descriptor` bytes are
    /// an encoded `ados_protocol::framebus::FrameDescriptor`; the plugin decodes
    /// it and maps the shared-memory ring it names to read the pixels.
    pub(super) async fn deliver_vision_frame<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        descriptor: &[u8],
    ) -> Result<(), ServerError> {
        if !Self::holds(token, Method::VisionSubscribeFrames) {
            return Ok(());
        }
        let ts = now_ms();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: ados_protocol::framebus::methods::DELIVER_FRAME.to_string(),
            capability: "vision.frame.read".to_string(),
            args: Value::Map(vec![
                (
                    Value::from("descriptor"),
                    Value::Binary(descriptor.to_vec()),
                ),
                (Value::from("timestamp_ms"), Value::Integer(ts.into())),
            ]),
            request_id: format!("vis-{ts}"),
            token: token.to_token_string(),
            error: None,
        };
        write_frame(write_half, &env).await
    }

    /// Push one front-panel press to the plugin as a `button.deliver` event.
    ///
    /// The press rides under `press` as the JSON-encoded
    /// `ados_protocol::buttons::ButtonPress` the host re-encoded off the bus.
    /// Unlike the vision deliveries this carries no binary blob, so it goes as a
    /// decoded map: a press is four small scalars and making the SDKs decode a
    /// nested byte string would buy nothing.
    pub(super) async fn deliver_button_press<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        press: &[u8],
    ) -> Result<(), ServerError> {
        if !Self::holds(token, Method::ButtonSubscribe) {
            return Ok(());
        }
        let ts = now_ms();
        let decoded: ados_protocol::buttons::ButtonPress = match serde_json::from_slice(press) {
            Ok(v) => v,
            // The host re-encoded this itself, so a failure here is a host bug,
            // not bad input. Drop the press rather than kill the session.
            Err(e) => {
                tracing::debug!(error = %e, "button press re-encode was unreadable");
                return Ok(());
            }
        };
        let mut fields = vec![
            (Value::from("pin"), Value::Integer(decoded.pin.into())),
            (Value::from("kind"), Value::from(decoded.kind.as_str())),
            (
                Value::from("timestamp_ms"),
                Value::Integer(decoded.timestamp_ms.into()),
            ),
        ];
        // An unmapped button carries no action; send the key as nil rather than
        // omitting it so a decoder never has to distinguish absent from unmapped.
        fields.push((
            Value::from("action"),
            match decoded.action.as_deref() {
                Some(a) => Value::from(a),
                None => Value::Nil,
            },
        ));
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: ados_protocol::buttons::DELIVER.to_string(),
            capability: ados_protocol::buttons::SUBSCRIBE.to_string(),
            args: Value::Map(vec![
                (Value::from("press"), Value::Map(fields)),
                (Value::from("timestamp_ms"), Value::Integer(ts.into())),
            ]),
            request_id: format!("btn-{ts}"),
            token: token.to_token_string(),
            error: None,
        };
        write_frame(write_half, &env).await
    }

    /// Push raw FC->host MSP bytes to the plugin as an `msp.deliver` event: kind
    /// `event`, method `msp.deliver`, capability `msp.read`, args `{bytes,
    /// timestamp_ms}`, request_id `msp-<ms>`. The `bytes` are the raw MSP chunk
    /// the router forwarded; the plugin's codec parses them (the host never does).
    pub(super) async fn deliver_msp<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        bytes: &[u8],
    ) -> Result<(), ServerError> {
        if !Self::holds(token, Method::MspSubscribe) {
            return Ok(());
        }
        let ts = now_ms();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: "msp.deliver".to_string(),
            capability: "msp.read".to_string(),
            args: Value::Map(vec![
                (Value::from("bytes"), Value::Binary(bytes.to_vec())),
                (Value::from("timestamp_ms"), Value::Integer(ts.into())),
            ]),
            request_id: format!("msp-{ts}"),
            token: token.to_token_string(),
            error: None,
        };
        write_frame(write_half, &env).await
    }

    /// Push one decoded application datagram to the plugin as a
    /// `radio.aux_stream.deliver` event: kind `event`, method
    /// `radio.aux_stream.deliver`, capability `radio.aux_stream`, args
    /// `{channel, payload, timestamp_ms}`, request_id `aux-<ms>`. The `payload`
    /// is the application bytes the host forwarded as-is (the host never decodes
    /// the app stream — the plugin's codec owns that), and `channel` is the
    /// AppStream/AppCommand byte.
    pub(super) async fn deliver_aux_stream<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        channel: u8,
        payload: &[u8],
    ) -> Result<(), ServerError> {
        if !Self::holds(token, Method::RadioAuxStreamSubscribe) {
            return Ok(());
        }
        let ts = now_ms();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: "radio.aux_stream.deliver".to_string(),
            capability: "radio.aux_stream".to_string(),
            args: Value::Map(vec![
                (Value::from("channel"), Value::Integer(channel.into())),
                (Value::from("payload"), Value::Binary(payload.to_vec())),
                (Value::from("timestamp_ms"), Value::Integer(ts.into())),
            ]),
            request_id: format!("aux-{ts}"),
            token: token.to_token_string(),
            error: None,
        };
        write_frame(write_half, &env).await
    }

    /// Push one touch-zone tap on the plugin's reserved display page as a
    /// `display.zone.tapped` event: kind `event`, method `display.zone.tapped`,
    /// capability `display.oled.page`, args `{key, timestamp_ms}`, request_id
    /// `dz-<ms>`. The `key` is the tapped zone's key string from the display
    /// sidecar's tap record.
    pub(super) async fn deliver_display_zone_tap<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        key: &str,
    ) -> Result<(), ServerError> {
        if !Self::holds(token, Method::DisplayZoneSubscribe) {
            return Ok(());
        }
        let ts = now_ms();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: "display.zone.tapped".to_string(),
            capability: "display.oled.page".to_string(),
            args: Value::Map(vec![
                (Value::from("key"), Value::from(key.to_string())),
                (Value::from("timestamp_ms"), Value::Integer(ts.into())),
            ]),
            request_id: format!("dz-{ts}"),
            token: token.to_token_string(),
            error: None,
        };
        write_frame(write_half, &env).await
    }

    /// Push a vision detection batch to the plugin as a `vision.deliver_detection`
    /// envelope. The frame deliver shape is kind `event`, method
    /// `vision.deliver_detection`, capability `vision.detection.subscribe`, args
    /// `{batch: bytes, timestamp_ms}`, request_id `vis-det-<ms>`. The `batch`
    /// bytes are an encoded `ados_protocol::framebus::DetectionBatch`; the plugin
    /// decodes it to read the detections.
    pub(super) async fn deliver_vision_detection<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        batch: &[u8],
    ) -> Result<(), ServerError> {
        if !Self::holds(token, Method::VisionSubscribeDetections) {
            return Ok(());
        }
        let ts = now_ms();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "event".to_string(),
            method: ados_protocol::framebus::methods::DELIVER_DETECTION.to_string(),
            capability: "vision.detection.subscribe".to_string(),
            args: Value::Map(vec![
                (Value::from("batch"), Value::Binary(batch.to_vec())),
                (Value::from("timestamp_ms"), Value::Integer(ts.into())),
            ]),
            request_id: format!("vis-det-{ts}"),
            token: token.to_token_string(),
            error: None,
        };
        write_frame(write_half, &env).await
    }
}

/// One connection's subscription state, borrowed together so a token
/// adoption can prune it in one place.
pub(super) struct Subscriptions<'a> {
    pub(super) events: &'a mut Vec<String>,
    pub(super) mavlink: &'a mut Vec<String>,
    pub(super) vision: &'a mut Vec<String>,
    pub(super) detections: &'a mut Vec<String>,
    pub(super) button: &'a mut bool,
    pub(super) msp: &'a mut bool,
    pub(super) aux: &'a mut bool,
    pub(super) display_zone: &'a mut bool,
    pub(super) telemetry: &'a mut bool,
}

/// The per-subscription forwarder tasks of one connection, each tagged with
/// the subscribe method that armed it, so a revoke can stop exactly the
/// streams it no longer covers.
#[derive(Default)]
pub(super) struct Forwarders(pub(super) Vec<(Method, JoinHandle<()>)>);

impl Forwarders {
    pub(super) fn push(&mut self, method: Method, task: JoinHandle<()>) {
        self.0.push((method, task));
    }

    /// Abort and drop every forwarder whose method `revoked` selects.
    pub(super) fn abort_where(&mut self, revoked: impl Fn(Method) -> bool) {
        self.0.retain(|(method, task)| {
            if revoked(*method) {
                task.abort();
                false
            } else {
                true
            }
        });
    }

    pub(super) fn abort_all(self) {
        for (_, task) in self.0 {
            task.abort();
        }
    }
}

/// One subscribed MAVLink frame to push, tagged with the subscription's
/// msg_name. The per-subscription forwarder builds these into the merged
/// delivery channel the connection's select loop drains.
pub(super) struct MavlinkDelivery {
    pub(super) msg_name: String,
    pub(super) frame: Vec<u8>,
}

/// Extract the `msg_name` arg for `mavlink.subscribe`: a non-empty string, else
/// `None` (the `_rpc_error("msg_name must be a non-empty string")` path).
pub(super) fn mavlink_subscribe_msg_name(args: &Value) -> Option<String> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some("msg_name"))
            .and_then(|(_, v)| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

/// One subscribed vision frame descriptor to push. The per-subscription
/// forwarder builds these into the merged delivery channel the connection's
/// select loop drains.
pub(super) struct VisionDelivery {
    pub(super) descriptor: Vec<u8>,
}

/// One subscribed vision detection batch to push. The per-subscription forwarder
/// builds these into the merged delivery channel the connection's select loop
/// drains. The bytes are an encoded `ados_protocol::framebus::DetectionBatch`.
pub(super) struct DetectionDelivery {
    pub(super) batch: Vec<u8>,
}

/// Extract the `camera_id` arg for `vision.subscribe_frames`. Unlike the MAVLink
/// `msg_name`, an absent or empty `camera_id` is valid and means "every camera"
/// (the host fans out all descriptors), so this returns a plain `String` that is
/// `""` for the subscribe-all case rather than an `Option`.
pub(super) fn vision_subscribe_camera_id(args: &Value) -> String {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some("camera_id"))
            .and_then(|(_, v)| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Forward the host's vehicle-state snapshots to one connection, at most one
/// per `min_interval`, newest wins: a snapshot that arrives inside the interval
/// replaces the one waiting, so a producer burst never becomes a plugin burst
/// and a paced plugin never receives stale state. The first snapshot goes at
/// once. Returns when the host's reader or the connection goes away.
pub(super) async fn pace_snapshots(
    mut rx: broadcast::Receiver<Arc<Value>>,
    tx: mpsc::Sender<Arc<Value>>,
    min_interval: Duration,
) {
    let mut last_sent: Option<tokio::time::Instant> = None;
    loop {
        let mut snapshot = match rx.recv().await {
            Ok(snapshot) => snapshot,
            // Fell behind the fanout: the next recv resumes at the tail, which
            // is the snapshot this subscriber wants anyway.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if let Some(due) = last_sent.map(|sent| sent + min_interval) {
            if tokio::time::Instant::now() < due {
                tokio::time::sleep_until(due).await;
                loop {
                    match rx.try_recv() {
                        Ok(newer) => snapshot = newer,
                        Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }
        }
        if tx.send(snapshot).await.is_err() {
            return; // connection gone
        }
        last_sent = Some(tokio::time::Instant::now());
    }
}
