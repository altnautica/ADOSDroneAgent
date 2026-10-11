//! The per-connection serve loop.

use super::*;

impl<H: HostServices> Connection<H> {
    /// Run the dispatch loop of an authenticated connection. Returns when the
    /// peer closes or a protocol error occurs.
    pub(super) async fn serve(
        &self,
        token: CapabilityToken,
        mut read_half: tokio::net::unix::OwnedReadHalf,
        mut write_half: tokio::net::unix::OwnedWriteHalf,
    ) -> Result<(), ServerError> {
        // Mutable: a rotation (permission change or TTL refresh) replaces the
        // session's token in place, so the gate always runs against the
        // operator's current grant set without tearing the connection down.
        let mut token = token;
        // The event fan-out task pushes matching events to this plugin. Built
        // once the session is up; it filters the shared bus by the plugin's
        // active subscription patterns. A single task drains the bus receiver
        // and writes deliver envelopes; subscription patterns accumulate as the
        // plugin issues event.subscribe.
        let mut subscriptions: Vec<String> = Vec::new();
        let mut bus_rx = self.bus.subscribe();

        // MAVLink subscriptions. Each `mavlink.subscribe` records the msg_name
        // and a frame receiver from the host's MAVLink client; a per-subscription
        // forwarder task pulls frames and tags them with the msg_name into one
        // merged channel, so a single select branch writes the `mavlink.deliver`
        // envelopes without competing writers on the socket. Mirrors the Python
        // pump (one pump task per subscription) collapsed onto this connection's
        // single writer. The Vec records active names for the already-subscribed
        // dedupe; the forwarders own the receivers.
        let mut mavlink_subs: Vec<String> = Vec::new();
        let (mav_tx, mut mav_rx) = tokio::sync::mpsc::channel::<MavlinkDelivery>(256);
        let mut forwarders = Forwarders::default();

        // Vision frame-descriptor subscriptions. Each `vision.subscribe_frames`
        // records the camera_id and a descriptor receiver from the host's vision
        // client; a per-subscription forwarder pulls descriptor bytes into one
        // merged channel, so a single select branch writes the `vision.deliver`
        // envelopes without competing writers. Same shape as the MAVLink pump.
        let mut vision_subs: Vec<String> = Vec::new();
        let (vis_tx, mut vis_rx) = tokio::sync::mpsc::channel::<VisionDelivery>(256);

        // Vision detection-batch subscriptions. Each `vision.subscribe_detections`
        // records the camera_id and a batch receiver from the host's vision
        // client; a per-subscription forwarder pulls batch bytes into one merged
        // channel, so a single select branch writes the `vision.deliver_detection`
        // envelopes without competing writers. Same shape as the frame pump.
        let mut detection_subs: Vec<String> = Vec::new();
        let (det_tx, mut det_rx) = tokio::sync::mpsc::channel::<DetectionDelivery>(256);

        // Front-panel button subscription. One `button.subscribe` per connection
        // arms a forwarder off the host's shared bus receiver into this merged
        // channel, so the select loop writes `button.deliver` envelopes with no
        // competing writers. Same shape as the two vision pumps.
        let mut button_subscribed = false;
        let (btn_tx, mut btn_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        // MSP byte-plane subscription. One `msp.subscribe` per connection arms a
        // forwarder off the host's MSP client broadcast into this merged channel,
        // so the select loop writes `msp.deliver` envelopes with no competing
        // writers. Same shape as the button pump; there is no per-message filter
        // (MSP has no topic), so a subscriber receives every FC->host chunk.
        let mut msp_subscribed = false;
        let (msp_tx, mut msp_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);

        // Auxiliary-stream application subscribe. One `radio.aux_stream.subscribe`
        // per connection arms a forwarder off the host's shared aux reader into
        // this merged channel, so the select loop writes the `radio.aux_stream.
        // deliver` envelopes with no competing writers. Same shape as the button
        // pump; each item is `(channel, payload)`.
        let mut aux_subscribed = false;
        let (aux_tx, mut aux_rx) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(64);

        // Plugin-page touch-zone subscribe. One `display.zone.subscribe` per
        // connection arms a forwarder off the host's shared tap watcher into this
        // merged channel, so the select loop writes the `display.zone.tapped`
        // envelopes with no competing writers. Same shape as the button pump;
        // each item is the tapped zone `key`.
        let mut dz_subscribed = false;
        let (dz_tx, mut dz_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        // Vehicle-state subscribe. One `telemetry.subscribe` per connection arms
        // a pacing forwarder off the host's shared state reader into this
        // channel, so the select loop writes the `telemetry.state` events with no
        // competing writers. The forwarder keeps only the newest snapshot, so
        // the channel needs no depth.
        let mut telemetry_subscribed = false;
        let (tel_tx, mut tel_rx) = mpsc::channel::<Arc<Value>>(1);

        // Slow host methods (see `runs_detached`) run off this loop, so a relay
        // or engine round trip never holds up the connection's other requests
        // and pushes. The select loop writes each reply as it lands; its request
        // id correlates it, as it does every response.
        let mut in_flight: JoinSet<DetachedReply> = JoinSet::new();

        // MCP tool invocation: register an outbound-request sender so the control
        // socket can reach THIS live connection (the host->plugin request flow),
        // and track pending replies here so the read branch can resolve them. On
        // session end the sender is unregistered and any pending waiter is failed
        // (the one-shot senders drop), so a control-socket invoke never hangs.
        let (invoke_tx, mut invoke_rx) = tokio::sync::mpsc::channel::<InvokeRequest>(16);
        // Register a CLONE and keep our own handle so the session-end unregister is
        // identity-checked (a reconnect overlap must not evict the successor's
        // sender registered under the same plugin id).
        self.invoke.register(&self.plugin_id, invoke_tx.clone());
        let mut pending_invokes: std::collections::HashMap<
            String,
            tokio::sync::oneshot::Sender<Result<Value, String>>,
        > = std::collections::HashMap::new();
        // Periodically reap pending invokes whose registry-side receiver is gone
        // (the invoke timed out or was abandoned), so an adversarial plugin that
        // accepts invokes but never replies cannot grow the map unbounded.
        let mut reap = tokio::time::interval(std::time::Duration::from_secs(10));
        reap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Token rotation: register a token sender so a re-mint (permission
        // change, or the daemon's proactive pre-expiry refresh) reaches THIS
        // session. Without it a rotation would only rewrite the env file and
        // the live connection would keep gating on the token it verified at
        // `hello` until the plugin restarted.
        let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<CapabilityToken>(4);
        self.refresh.register(&self.plugin_id, refresh_tx.clone());

        // Requests arrive through a dedicated reader task that owns the read
        // half. `read_envelope` is two `read_exact` calls, which are not
        // cancel-safe: polled directly as a `select!` branch, a delivery that
        // won the race while a request frame was only partly buffered dropped
        // the bytes already read and desynchronised the stream for the rest of
        // the session. The channel receive below is cancel-safe.
        let (frame_tx, mut frame_rx) = mpsc::channel::<Result<Envelope, ServerError>>(8);
        let reader = tokio::spawn(async move {
            loop {
                match read_envelope(&mut read_half).await {
                    Ok(Some(env)) => {
                        if frame_tx.send(Ok(env)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        let _ = frame_tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        });

        // ---- dispatch loop --------------------------------------------
        let result = loop {
            // Race the inbound request against an outgoing event or MAVLink frame
            // to deliver, so a long-lived subscriber both serves requests and
            // receives pushes on the one connection (the Python server uses
            // coroutines; one select loop is the single-task equivalent).
            tokio::select! {
                frame = frame_rx.recv() => {
                    let env = match frame {
                        Some(Ok(env)) => env,
                        None => break Ok(()),
                        Some(Err(e)) => break Err(e),
                    };
                    // A response frame is the plugin's reply to a host-issued
                    // tool.invoke: resolve the waiter, do NOT route it as a request.
                    if env.kind == "response" {
                        if let Some(reply) = pending_invokes.remove(&env.request_id) {
                            let outcome = match env.error {
                                Some(msg) => Err(msg),
                                None => Ok(env.args),
                            };
                            let _ = reply.send(outcome);
                        }
                        continue;
                    }
                    if let Err(e) = self
                        .handle_request(
                            &mut write_half,
                            &mut token,
                            env,
                            &mut subscriptions,
                            &mut mavlink_subs,
                            &mav_tx,
                            &mut vision_subs,
                            &vis_tx,
                            &mut detection_subs,
                            &det_tx,
                            &mut button_subscribed,
                            &btn_tx,
                            &mut msp_subscribed,
                            &msp_tx,
                            &mut aux_subscribed,
                            &aux_tx,
                            &mut dz_subscribed,
                            &dz_tx,
                            &mut telemetry_subscribed,
                            &tel_tx,
                            &mut forwarders,
                            &mut in_flight,
                        )
                        .await
                    {
                        break Err(e);
                    }
                }
                req = invoke_rx.recv() => {
                    // A control-socket tool.invoke targeting this plugin. Gate on
                    // the plugin's verified token carrying mcp.expose (the host
                    // authority; the runner re-checks), then write the request and
                    // track the pending reply. `None` means the registry dropped
                    // the sender (session ending) — keep serving.
                    if let Some(InvokeRequest { request_id, tool, arguments, reply }) = req {
                        // Re-check expiry, like the request path does: the session
                        // outlives the token's TTL, so an invoke arriving after the
                        // token aged out must be refused, not run.
                        if token.is_expired(now_secs()) {
                            let _ = reply.send(Err(crate::dispatch::errors::TOKEN_EXPIRED.to_string()));
                        } else if !token.granted_caps.contains("mcp.expose") {
                            let _ = reply.send(Err("capability_denied: mcp.expose".to_string()));
                        } else {
                            let env = Envelope {
                                version: PROTOCOL_VERSION,
                                kind: "request".to_string(),
                                method: "tool.invoke".to_string(),
                                capability: "mcp.expose".to_string(),
                                args: Value::Map(vec![
                                    (Value::from("tool"), Value::from(tool)),
                                    (Value::from("arguments"), arguments),
                                ]),
                                request_id: request_id.clone(),
                                token: String::new(),
                                error: None,
                            };
                            match write_frame(&mut write_half, &env).await {
                                Ok(()) => {
                                    pending_invokes.insert(request_id, reply);
                                }
                                Err(_) => {
                                    let _ = reply.send(Err("plugin_write_failed".to_string()));
                                }
                            }
                        }
                    }
                }
                evt = bus_rx.recv() => {
                    match evt {
                        Ok(event) => {
                            if handlers::may_deliver(&self.plugin_id, &event, &self.shared_topics)
                                && subscriptions.iter().any(|p| handlers::topic_matches(p, &event.topic))
                            {
                                if let Err(e) = self.deliver_event(&mut write_half, &token, &event).await {
                                    break Err(e);
                                }
                            }
                        }
                        // Lagged: a slow subscriber missed events. Keep serving
                        // rather than tearing the session down; the next recv
                        // resumes at the current tail.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    }
                }
                delivery = mav_rx.recv() => {
                    // None means every forwarder dropped its sender; nothing more
                    // to deliver, but keep serving requests, so do nothing.
                    if let Some(MavlinkDelivery { msg_name, frame }) = delivery {
                        if let Err(e) = self
                            .deliver_mavlink(&mut write_half, &token, &msg_name, &frame)
                            .await
                        {
                            break Err(e);
                        }
                    }
                }
                delivery = vis_rx.recv() => {
                    // None means every vision forwarder dropped its sender; keep
                    // serving requests rather than tearing down.
                    if let Some(VisionDelivery { descriptor }) = delivery {
                        if let Err(e) = self
                            .deliver_vision_frame(&mut write_half, &token, &descriptor)
                            .await
                        {
                            break Err(e);
                        }
                    }
                }
                press = btn_rx.recv() => {
                    // None means the forwarder dropped its sender; keep serving
                    // requests rather than tearing the session down.
                    if let Some(press) = press {
                        if let Err(e) = self
                            .deliver_button_press(&mut write_half, &token, &press)
                            .await
                        {
                            break Err(e);
                        }
                    }
                }
                bytes = msp_rx.recv() => {
                    // None means the MSP forwarder dropped its sender; keep serving.
                    if let Some(bytes) = bytes {
                        if let Err(e) = self
                            .deliver_msp(&mut write_half, &token, &bytes)
                            .await
                        {
                            break Err(e);
                        }
                    }
                }
                app = aux_rx.recv() => {
                    // None means every aux forwarder dropped its sender; keep
                    // serving requests rather than tearing the session down.
                    if let Some((channel, payload)) = app {
                        if let Err(e) = self
                            .deliver_aux_stream(&mut write_half, &token, channel, &payload)
                            .await
                        {
                            break Err(e);
                        }
                    }
                }
                key = dz_rx.recv() => {
                    // None means every zone forwarder dropped its sender; keep
                    // serving requests rather than tearing the session down.
                    if let Some(key) = key {
                        // Zone keys are ASCII; a non-UTF-8 key is a host bug, so
                        // drop it rather than send a broken envelope.
                        if let Ok(key) = String::from_utf8(key) {
                            if let Err(e) = self
                                .deliver_display_zone_tap(&mut write_half, &token, &key)
                                .await
                            {
                                break Err(e);
                            }
                        }
                    }
                }
                delivery = det_rx.recv() => {
                    // None means every detection forwarder dropped its sender;
                    // keep serving requests rather than tearing down.
                    if let Some(DetectionDelivery { batch }) = delivery {
                        if let Err(e) = self
                            .deliver_vision_detection(&mut write_half, &token, &batch)
                            .await
                        {
                            break Err(e);
                        }
                    }
                }
                snapshot = tel_rx.recv() => {
                    // None means the telemetry forwarder dropped its sender;
                    // keep serving requests rather than tearing down.
                    if let Some(snapshot) = snapshot {
                        if let Err(e) = self
                            .deliver_telemetry_state(&mut write_half, &token, &snapshot)
                            .await
                        {
                            break Err(e);
                        }
                    }
                }
                Some(done) = in_flight.join_next(), if !in_flight.is_empty() => {
                    if let Err(e) = write_detached_reply(&mut write_half, done).await {
                        break Err(e);
                    }
                }
                fresh = refresh_rx.recv() => {
                    // A rotated token for this session. Adopt it (so the gate
                    // runs against the operator's current grant set from the
                    // next request on) and forward it to the plugin, whose own
                    // copy is what it presents if it reconnects. `None` means
                    // the registry dropped the sender (session ending); keep
                    // serving.
                    if let Some(fresh) = fresh {
                        let expires_at = fresh.expires_at;
                        let caps: Vec<Value> = fresh
                            .granted_caps
                            .iter()
                            .map(|c| Value::from(c.as_str()))
                            .collect();
                        let wire = fresh.to_token_string();
                        token = fresh;
                        self.prune_revoked(
                            &token,
                            Subscriptions {
                                events: &mut subscriptions,
                                mavlink: &mut mavlink_subs,
                                vision: &mut vision_subs,
                                detections: &mut detection_subs,
                                button: &mut button_subscribed,
                                msp: &mut msp_subscribed,
                                aux: &mut aux_subscribed,
                                display_zone: &mut dz_subscribed,
                                telemetry: &mut telemetry_subscribed,
                            },
                            &mut forwarders,
                        );
                        let env = Envelope {
                            version: PROTOCOL_VERSION,
                            kind: "event".to_string(),
                            method: TOKEN_REFRESH_METHOD.to_string(),
                            capability: String::new(),
                            args: Value::Map(vec![
                                (Value::from("token"), Value::from(wire)),
                                (Value::from("expires_at"), Value::from(expires_at)),
                                (Value::from("granted_caps"), Value::Array(caps)),
                            ]),
                            request_id: format!("tok-{}", now_ms()),
                            token: String::new(),
                            error: None,
                        };
                        if let Err(e) = write_frame(&mut write_half, &env).await {
                            break Err(e);
                        }
                        tracing::info!(
                            plugin_id = %self.plugin_id,
                            expires_at,
                            "pushed rotated capability token to plugin"
                        );
                    }
                }
                _ = reap.tick() => {
                    // Drop pending invokes whose one-shot reply receiver was
                    // dropped by the registry (timeout/abandon). Cheap; bounds the
                    // map to genuinely in-flight invokes.
                    pending_invokes.retain(|_, reply| !reply.is_closed());
                }
            }
        };

        // Unregister the invoke sender so a later control-socket invoke fails
        // closed (plugin_not_running). Identity-checked so a superseding
        // reconnect's sender is never evicted by this ended session. Dropping
        // `pending_invokes` here fails any in-flight waiter (its one-shot sender
        // drops -> plugin_disconnected), so no control-socket invoke hangs past
        // the session.
        self.invoke.unregister_if(&self.plugin_id, &invoke_tx);
        // Same for the token sender: a rotation after this session ended has
        // nothing live to reach, and the env file it also rewrote is what the
        // plugin reads when it reconnects.
        self.refresh.unregister_if(&self.plugin_id, &refresh_tx);

        // Stop the per-subscription forwarder tasks, the detached host calls
        // and the request reader so none survive the session. A detached call
        // is a stateless forward, so cutting it short leaves nothing held.
        forwarders.abort_all();
        in_flight.abort_all();
        reader.abort();
        result
    }
}
