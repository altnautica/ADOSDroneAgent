//! Stream-arming subscribe methods: each attaches a per-connection forwarder
//! that feeds the merged delivery channel the serve loop writes from.

use super::*;

impl<H: HostServices> Connection<H> {
    /// mavlink.subscribe both validates and arms the per-connection push
    /// stream. The name must be a message the dialect defines; it is
    /// resolved to its id once, and the forwarder passes on only frames
    /// whose header carries that id, each tagged with the name. The host
    /// returns `{"already_subscribed": true}` or `{"subscribed": true,
    /// "msg_name": <m>}`.
    pub(super) async fn subscribe_mavlink<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        env: &Envelope,
        mavlink_subs: &mut Vec<String>,
        mav_tx: &mpsc::Sender<MavlinkDelivery>,
        forwarders: &mut Forwarders,
    ) -> Result<(), ServerError> {
        let Some(name) = mavlink_subscribe_msg_name(&env.args) else {
            return send_error(
                write_half,
                &env.request_id,
                "msg_name must be a non-empty string",
            )
            .await;
        };
        let Some(msg_id) = ados_protocol::mavlink::message_id_from_name(&name) else {
            return send_error(
                write_half,
                &env.request_id,
                &format!("unknown MAVLink message name: {name}"),
            )
            .await;
        };
        if mavlink_subs.iter().any(|n| n == &name) {
            let result = Value::Map(vec![(
                Value::from("already_subscribed"),
                Value::Boolean(true),
            )]);
            return send_response(write_half, &env.request_id, result).await;
        }
        mavlink_subs.push(name.clone());
        // Arm the push stream when the host has a MAVLink link. A router
        // chunk may batch several frames, so each is checked on its own.
        if let Some(mut rx) = self.host.mavlink_subscribe_stream(&self.plugin_id, &name) {
            let tx = mav_tx.clone();
            let fwd_name = name.clone();
            forwarders.push(
                Method::MavlinkSubscribe,
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(chunk) => {
                                for frame in ados_protocol::aux_mux::split_frames(&chunk) {
                                    if crate::realhost::mavlink_msg_id(frame) != Some(msg_id) {
                                        continue;
                                    }
                                    let delivery = MavlinkDelivery {
                                        msg_name: fwd_name.clone(),
                                        frame: frame.to_vec(),
                                    };
                                    if tx.send(delivery).await.is_err() {
                                        return; // connection gone
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }),
            );
        }
        let result = Value::Map(vec![
            (Value::from("subscribed"), Value::Boolean(true)),
            (Value::from("msg_name"), Value::from(name.as_str())),
        ]);
        send_response(write_half, &env.request_id, result).await
    }

    /// vision.subscribe_frames arms the per-connection frame-descriptor
    /// push stream, exactly like mavlink.subscribe arms the frame pump. On
    /// the first subscribe to a camera we obtain a descriptor receiver from
    /// the host's vision client (if wired) and spawn a forwarder that pushes
    /// each descriptor into the merged delivery channel, so the select loop
    /// writes `vision.deliver` envelopes. A camera_id of "" subscribes to
    /// every camera (the host fans out all descriptors); the response echoes
    /// the requested camera_id.
    pub(super) async fn subscribe_vision_frames<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        env: &Envelope,
        vision_subs: &mut Vec<String>,
        vis_tx: &mpsc::Sender<VisionDelivery>,
        forwarders: &mut Forwarders,
    ) -> Result<(), ServerError> {
        let camera_id = vision_subscribe_camera_id(&env.args);
        if vision_subs.iter().any(|c| c == &camera_id) {
            let result = Value::Map(vec![(
                Value::from("already_subscribed"),
                Value::Boolean(true),
            )]);
            return send_response(write_half, &env.request_id, result).await;
        }
        vision_subs.push(camera_id.clone());
        if let Some(mut rx) = self
            .host
            .vision_subscribe_stream(&self.plugin_id, &camera_id)
        {
            let tx = vis_tx.clone();
            forwarders.push(
                Method::VisionSubscribeFrames,
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(descriptor) => {
                                if tx.send(VisionDelivery { descriptor }).await.is_err() {
                                    break; // connection gone
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }),
            );
        }
        let result = Value::Map(vec![
            (Value::from("subscribed"), Value::Boolean(true)),
            (Value::from("camera_id"), Value::from(camera_id.as_str())),
        ]);
        send_response(write_half, &env.request_id, result).await
    }

    /// button.subscribe arms the per-connection front-panel press stream.
    /// The host owns one connection to the bus and hands out receivers, so
    /// arming here is just attaching a forwarder. A second subscribe on the
    /// same connection is idempotent rather than an error: re-arming would
    /// duplicate every press to that plugin.
    pub(super) async fn subscribe_button<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        env: &Envelope,
        button_subscribed: &mut bool,
        btn_tx: &mpsc::Sender<Vec<u8>>,
        forwarders: &mut Forwarders,
    ) -> Result<(), ServerError> {
        if *button_subscribed {
            let result = Value::Map(vec![(
                Value::from("already_subscribed"),
                Value::Boolean(true),
            )]);
            return send_response(write_half, &env.request_id, result).await;
        }
        *button_subscribed = true;
        if let Some(mut rx) = self.host.button_subscribe_stream(&self.plugin_id) {
            let tx = btn_tx.clone();
            forwarders.push(
                Method::ButtonSubscribe,
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(press) => {
                                if tx.send(press).await.is_err() {
                                    break;
                                }
                            }
                            // Lagged: this subscriber fell behind and the
                            // oldest presses were dropped. Keep going — a
                            // missed press is better than a dead stream.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }),
            );
        }
        let result = Value::Map(vec![(Value::from("subscribed"), Value::Boolean(true))]);
        send_response(write_half, &env.request_id, result).await
    }

    /// msp.subscribe arms the per-connection raw-MSP push stream, exactly
    /// like button.subscribe. One subscribe per connection; the forwarder
    /// pushes every FC->host chunk into the merged channel, so the select
    /// loop writes `msp.deliver` envelopes. There is no per-message filter
    /// (MSP has no topic), so the plugin's own codec parses the stream.
    pub(super) async fn subscribe_msp<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        env: &Envelope,
        msp_subscribed: &mut bool,
        msp_tx: &mpsc::Sender<Vec<u8>>,
        forwarders: &mut Forwarders,
    ) -> Result<(), ServerError> {
        if *msp_subscribed {
            let result = Value::Map(vec![(
                Value::from("already_subscribed"),
                Value::Boolean(true),
            )]);
            return send_response(write_half, &env.request_id, result).await;
        }
        *msp_subscribed = true;
        if let Some(mut rx) = self.host.msp_subscribe_stream(&self.plugin_id) {
            let tx = msp_tx.clone();
            forwarders.push(
                Method::MspSubscribe,
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(bytes) => {
                                if tx.send(bytes).await.is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }),
            );
        }
        let result = Value::Map(vec![(Value::from("subscribed"), Value::Boolean(true))]);
        send_response(write_half, &env.request_id, result).await
    }

    /// radio.aux_stream.subscribe arms the per-connection application-
    /// datagram push stream off the host's shared aux reader, exactly like
    /// button.subscribe arms the press stream. A second subscribe on the
    /// same connection is idempotent rather than an error: re-arming would
    /// duplicate every app frame to that plugin. The stream is one shared
    /// resource, so only the session that opened it may subscribe, and
    /// the forwarder passes a datagram only while that session still owns
    /// it: another plugin holding the capability never sees the owner's
    /// application traffic.
    pub(super) async fn subscribe_aux_stream<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        env: &Envelope,
        aux_subscribed: &mut bool,
        aux_tx: &mpsc::Sender<(u8, Vec<u8>)>,
        forwarders: &mut Forwarders,
    ) -> Result<(), ServerError> {
        if *aux_subscribed {
            let result = Value::Map(vec![(
                Value::from("already_subscribed"),
                Value::Boolean(true),
            )]);
            return send_response(write_half, &env.request_id, result).await;
        }
        let rx = match self
            .host
            .radio_aux_stream_subscribe_stream(&self.plugin_id, self.session)
        {
            Ok(rx) => rx,
            Err(e) => return send_error(write_half, &env.request_id, &e.body()).await,
        };
        *aux_subscribed = true;
        if let Some(mut rx) = rx {
            let tx = aux_tx.clone();
            let host = self.host.clone();
            let (plugin_id, session) = (self.plugin_id.clone(), self.session);
            forwarders.push(
                Method::RadioAuxStreamSubscribe,
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(app) => {
                                if !host.aux_stream_owned_by(&plugin_id, session) {
                                    continue;
                                }
                                if tx.send(app).await.is_err() {
                                    break;
                                }
                            }
                            // Lagged: this subscriber fell behind and the
                            // oldest frames were dropped. Keep going — the app
                            // lane is lossy-tolerant by design.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }),
            );
        }
        let result = Value::Map(vec![(Value::from("subscribed"), Value::Boolean(true))]);
        send_response(write_half, &env.request_id, result).await
    }

    /// display.zone.subscribe arms the per-connection touch-zone tap stream
    /// for the plugin's reserved display page, exactly like button.subscribe
    /// arms the press stream. A second subscribe on the same connection is
    /// idempotent (re-arming would duplicate every tap to that plugin). The
    /// page is one shared resource, so a tap is passed on only while this
    /// plugin owns the page: a tap on another plugin's page never reaches it.
    pub(super) async fn subscribe_display_zone<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        env: &Envelope,
        dz_subscribed: &mut bool,
        dz_tx: &mpsc::Sender<Vec<u8>>,
        forwarders: &mut Forwarders,
    ) -> Result<(), ServerError> {
        if *dz_subscribed {
            let result = Value::Map(vec![(
                Value::from("already_subscribed"),
                Value::Boolean(true),
            )]);
            return send_response(write_half, &env.request_id, result).await;
        }
        *dz_subscribed = true;
        if let Some(mut rx) = self.host.display_zone_tap_stream(&self.plugin_id) {
            let tx = dz_tx.clone();
            let host = self.host.clone();
            let plugin_id = self.plugin_id.clone();
            forwarders.push(
                Method::DisplayZoneSubscribe,
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(key) => {
                                if !host.display_page_owned_by(&plugin_id) {
                                    continue;
                                }
                                if tx.send(key).await.is_err() {
                                    break;
                                }
                            }
                            // Lagged: the watcher produced taps faster than this
                            // subscriber drained them. Drop the oldest — a
                            // missed tap is better than a dead stream.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }),
            );
        }
        let result = Value::Map(vec![(Value::from("subscribed"), Value::Boolean(true))]);
        send_response(write_half, &env.request_id, result).await
    }

    /// vision.subscribe_detections arms the per-connection detection-batch
    /// push stream, exactly like vision.subscribe_frames arms the frame
    /// pump. On the first subscribe to a camera we obtain a batch receiver
    /// from the host's vision client (if wired) and spawn a forwarder that
    /// pushes each batch into the merged delivery channel, so the select
    /// loop writes `vision.deliver_detection` envelopes. A camera_id of ""
    /// subscribes to every camera (the host fans out all batches); the
    /// response echoes the requested camera_id.
    pub(super) async fn subscribe_detections<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        env: &Envelope,
        detection_subs: &mut Vec<String>,
        det_tx: &mpsc::Sender<DetectionDelivery>,
        forwarders: &mut Forwarders,
    ) -> Result<(), ServerError> {
        let camera_id = vision_subscribe_camera_id(&env.args);
        if detection_subs.iter().any(|c| c == &camera_id) {
            let result = Value::Map(vec![(
                Value::from("already_subscribed"),
                Value::Boolean(true),
            )]);
            return send_response(write_half, &env.request_id, result).await;
        }
        detection_subs.push(camera_id.clone());
        if let Some(mut rx) = self
            .host
            .vision_subscribe_detection_stream(&self.plugin_id, &camera_id)
        {
            let tx = det_tx.clone();
            forwarders.push(
                Method::VisionSubscribeDetections,
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(batch) => {
                                if tx.send(DetectionDelivery { batch }).await.is_err() {
                                    break; // connection gone
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }),
            );
        }
        let result = Value::Map(vec![
            (Value::from("subscribed"), Value::Boolean(true)),
            (Value::from("camera_id"), Value::from(camera_id.as_str())),
        ]);
        send_response(write_half, &env.request_id, result).await
    }

    /// Mirror a merged `telemetry.extend` channel into the plugin's state
    /// sidecar as `telemetry.<channel>`: the file the native front serves at
    /// `GET /api/plugins/{id}/state`, which is how the GCS gets a plugin's own
    /// extended channel to that plugin's UI. The merge already succeeded, so a
    /// write fault is logged, never returned.
    pub(super) fn record_extended_channel(&self, args: &Value) {
        let field = |key: &str| match args {
            Value::Map(m) => m
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v),
            _ => None,
        };
        let Some(channel) = field("channel")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        else {
            return;
        };
        let payload = match field("payload") {
            Some(v @ Value::Map(_)) => v.clone(),
            _ => Value::Map(vec![]),
        };
        let topic = format!("telemetry.{channel}");
        if let Err(e) = crate::state_sidecar::record(
            &self.socket_dir,
            &self.plugin_id,
            &topic,
            &payload,
            now_ms(),
        ) {
            tracing::warn!(
                plugin_id = %self.plugin_id,
                topic = %topic,
                error = %e,
                "failed to write plugin state sidecar"
            );
        }
    }

    /// Drop every subscription the adopted `token` no longer covers.
    ///
    /// A revoke re-mints the token, but a stream armed under the old grant
    /// would keep running: its forwarder pulls frames whatever the token now
    /// says. So each forwarder whose subscribe method needs a capability the
    /// token lacks is aborted, and its subscription state is cleared so a
    /// later re-grant can subscribe again. Event subscriptions are re-checked
    /// pattern by pattern, since a topic can need its own capability.
    pub(super) fn prune_revoked(
        &self,
        token: &CapabilityToken,
        subs: Subscriptions<'_>,
        forwarders: &mut Forwarders,
    ) {
        let holds = |m: Method| {
            m.required_cap()
                .is_none_or(|c| token.granted_caps.contains(c))
        };
        forwarders.abort_where(|m| !holds(m));
        let clear_list = |list: &mut Vec<String>, m: Method| {
            if !holds(m) {
                list.clear();
            }
        };
        clear_list(subs.mavlink, Method::MavlinkSubscribe);
        clear_list(subs.vision, Method::VisionSubscribeFrames);
        clear_list(subs.detections, Method::VisionSubscribeDetections);
        for (flag, m) in [
            (subs.button, Method::ButtonSubscribe),
            (subs.msp, Method::MspSubscribe),
            (subs.aux, Method::RadioAuxStreamSubscribe),
            (subs.display_zone, Method::DisplayZoneSubscribe),
            (subs.telemetry, Method::TelemetrySubscribe),
        ] {
            if !holds(m) {
                *flag = false;
            }
        }
        if holds(Method::EventSubscribe) {
            subs.events.retain(|pattern| {
                handlers::is_subscribe_allowed(
                    &self.plugin_id,
                    pattern,
                    &token.granted_caps,
                    &self.shared_topics,
                )
            });
        } else {
            subs.events.clear();
        }
    }

    /// Whether `token` still carries the capability `method` needs. Every
    /// push re-checks it, so nothing already queued for a stream leaks out
    /// after a revoke.
    pub(super) fn holds(token: &CapabilityToken, method: Method) -> bool {
        method
            .required_cap()
            .is_none_or(|c| token.granted_caps.contains(c))
    }
}
