//! Request gating and method routing for one connection.

use super::*;

impl<H: HostServices> Connection<H> {
    /// Gate and route one request envelope.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_request<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &mut CapabilityToken,
        env: Envelope,
        subscriptions: &mut Vec<String>,
        mavlink_subs: &mut Vec<String>,
        mav_tx: &tokio::sync::mpsc::Sender<MavlinkDelivery>,
        vision_subs: &mut Vec<String>,
        vis_tx: &tokio::sync::mpsc::Sender<VisionDelivery>,
        detection_subs: &mut Vec<String>,
        det_tx: &tokio::sync::mpsc::Sender<DetectionDelivery>,
        button_subscribed: &mut bool,
        btn_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
        msp_subscribed: &mut bool,
        msp_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
        aux_subscribed: &mut bool,
        aux_tx: &tokio::sync::mpsc::Sender<(u8, Vec<u8>)>,
        dz_subscribed: &mut bool,
        dz_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
        telemetry_subscribed: &mut bool,
        tel_tx: &mpsc::Sender<Arc<Value>>,
        forwarders: &mut Forwarders,
        in_flight: &mut JoinSet<DetachedReply>,
    ) -> Result<(), ServerError> {
        let req_id = env.request_id.clone();
        if token.is_expired(now_secs()) {
            // The token aged out mid-session. The session itself is still
            // authenticated (it passed `hello`), so the honest answer is to
            // re-mint from authoritative state rather than to refuse: the TTL
            // exists to bound a leaked token, not to end a plugin's access at
            // the ten-minute mark. `mint_current` re-reads the grant set, so a
            // capability the operator revoked in the meantime is NOT carried
            // into the new token, and it rewrites the env file so a reconnect
            // presents the live token too.
            match self
                .mint
                .as_ref()
                .and_then(|m| m.mint_current(&self.plugin_id))
            {
                Some(fresh) => {
                    tracing::info!(
                        plugin_id = %self.plugin_id,
                        expires_at = fresh.expires_at,
                        "re-minted an expired capability token mid-session"
                    );
                    let wire = fresh.to_token_string();
                    let expires_at = fresh.expires_at;
                    let caps: Vec<Value> = fresh
                        .granted_caps
                        .iter()
                        .map(|c| Value::from(c.as_str()))
                        .collect();
                    *token = fresh;
                    self.prune_revoked(
                        token,
                        Subscriptions {
                            events: &mut *subscriptions,
                            mavlink: &mut *mavlink_subs,
                            vision: &mut *vision_subs,
                            detections: &mut *detection_subs,
                            button: &mut *button_subscribed,
                            msp: &mut *msp_subscribed,
                            aux: &mut *aux_subscribed,
                            display_zone: &mut *dz_subscribed,
                            telemetry: &mut *telemetry_subscribed,
                        },
                        &mut *forwarders,
                    );
                    let notice = Envelope {
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
                    write_frame(write_half, &notice).await?;
                }
                // No mint wired, or the plugin is no longer enabled: the
                // expired token stays expired and the request is refused.
                None => {
                    return send_error(write_half, &req_id, crate::dispatch::errors::TOKEN_EXPIRED)
                        .await
                }
            }
        }
        let expired = token.is_expired(now_secs());
        match gate(&env.method, expired, &token.granted_caps) {
            Gate::TokenExpired => {
                send_error(write_half, &req_id, crate::dispatch::errors::TOKEN_EXPIRED).await
            }
            Gate::UnknownMethod(msg) => send_error(write_half, &req_id, &msg).await,
            Gate::CapabilityDenied(msg) => send_error(write_half, &req_id, &msg).await,
            Gate::Allow(method) => {
                self.route(
                    write_half,
                    token,
                    method,
                    &env,
                    subscriptions,
                    mavlink_subs,
                    mav_tx,
                    vision_subs,
                    vis_tx,
                    detection_subs,
                    det_tx,
                    button_subscribed,
                    btn_tx,
                    msp_subscribed,
                    msp_tx,
                    aux_subscribed,
                    aux_tx,
                    dz_subscribed,
                    dz_tx,
                    telemetry_subscribed,
                    tel_tx,
                    forwarders,
                    in_flight,
                )
                .await
            }
        }
    }

    /// Run a gated method. The event surface and ping are served in-process;
    /// every other method routes to the host facade.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn route<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        method: Method,
        env: &Envelope,
        subscriptions: &mut Vec<String>,
        mavlink_subs: &mut Vec<String>,
        mav_tx: &tokio::sync::mpsc::Sender<MavlinkDelivery>,
        vision_subs: &mut Vec<String>,
        vis_tx: &tokio::sync::mpsc::Sender<VisionDelivery>,
        detection_subs: &mut Vec<String>,
        det_tx: &tokio::sync::mpsc::Sender<DetectionDelivery>,
        button_subscribed: &mut bool,
        btn_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
        msp_subscribed: &mut bool,
        msp_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
        aux_subscribed: &mut bool,
        aux_tx: &tokio::sync::mpsc::Sender<(u8, Vec<u8>)>,
        dz_subscribed: &mut bool,
        dz_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
        telemetry_subscribed: &mut bool,
        tel_tx: &mpsc::Sender<Arc<Value>>,
        forwarders: &mut Forwarders,
        in_flight: &mut JoinSet<DetachedReply>,
    ) -> Result<(), ServerError> {
        match method {
            Method::Ping => {
                let result = handlers::ping_result(&self.plugin_id);
                send_response(write_half, &env.request_id, result).await
            }
            // telemetry.subscribe arms the per-connection vehicle-state push,
            // like button.subscribe arms the press stream: one subscribe per
            // connection (a second is idempotent; re-arming would double every
            // snapshot), and a forwarder off the host's shared reader that paces
            // to TELEMETRY_STATE_MAX_HZ, newest wins. The response names the
            // topic the snapshots arrive on.
            Method::TelemetrySubscribe => {
                if *telemetry_subscribed {
                    let result = Value::Map(vec![(
                        Value::from("already_subscribed"),
                        Value::Boolean(true),
                    )]);
                    return send_response(write_half, &env.request_id, result).await;
                }
                *telemetry_subscribed = true;
                if let Some(rx) = self.host.telemetry_state_stream(&self.plugin_id) {
                    forwarders.push(
                        Method::TelemetrySubscribe,
                        tokio::spawn(pace_snapshots(
                            rx,
                            tel_tx.clone(),
                            TELEMETRY_STATE_MIN_INTERVAL,
                        )),
                    );
                }
                let result = Value::Map(vec![
                    (Value::from("subscribed"), Value::Boolean(true)),
                    (Value::from("topic"), Value::from(TELEMETRY_STATE_TOPIC)),
                ]);
                send_response(write_half, &env.request_id, result).await
            }
            Method::EventPublish => {
                match handlers::prepare_publish(
                    &self.plugin_id,
                    &env.args,
                    &token.granted_caps,
                    &self.shared_topics,
                    now_ms(),
                ) {
                    PublishOutcome::Publish(event) => {
                        // Persist the latest event per topic to the plugin's
                        // state sidecar before fanning out, so the native front
                        // can serve the plugin's own published state to the GCS
                        // over the LAN. The publish authorization is the security
                        // boundary; a write fault is logged, never fatal to the
                        // publish.
                        if let Err(e) = crate::state_sidecar::record(
                            &self.socket_dir,
                            &self.plugin_id,
                            &event.topic,
                            &event.payload,
                            event.timestamp_ms,
                        ) {
                            tracing::warn!(
                                plugin_id = %self.plugin_id,
                                topic = %event.topic,
                                error = %e,
                                "failed to write plugin state sidecar"
                            );
                        }
                        let delivered = self.bus.publish(event);
                        let result = Value::Map(vec![(
                            Value::from("delivered"),
                            Value::Integer((delivered as i64).into()),
                        )]);
                        send_response(write_half, &env.request_id, result).await
                    }
                    PublishOutcome::Denied(e) => {
                        send_error(write_half, &env.request_id, &e.0).await
                    }
                }
            }
            Method::EventSubscribe => {
                match handlers::prepare_subscribe(
                    &self.plugin_id,
                    &env.args,
                    &token.granted_caps,
                    &self.shared_topics,
                ) {
                    Ok(pattern) => {
                        if subscriptions.contains(&pattern) {
                            let result = Value::Map(vec![(
                                Value::from("already_subscribed"),
                                Value::Boolean(true),
                            )]);
                            return send_response(write_half, &env.request_id, result).await;
                        }
                        if subscriptions.len() >= handlers::EVENT_MAX_SUBSCRIPTIONS {
                            return send_error(
                                write_half,
                                &env.request_id,
                                &format!(
                                    "subscription limit reached ({} per connection)",
                                    handlers::EVENT_MAX_SUBSCRIPTIONS
                                ),
                            )
                            .await;
                        }
                        subscriptions.push(pattern);
                        let result =
                            Value::Map(vec![(Value::from("subscribed"), Value::Boolean(true))]);
                        send_response(write_half, &env.request_id, result).await
                    }
                    Err(e) => send_error(write_half, &env.request_id, &e.0).await,
                }
            }
            Method::MavlinkSubscribe => {
                self.subscribe_mavlink(write_half, env, mavlink_subs, mav_tx, forwarders)
                    .await
            }
            Method::VisionSubscribeFrames => {
                self.subscribe_vision_frames(write_half, env, vision_subs, vis_tx, forwarders)
                    .await
            }
            Method::ButtonSubscribe => {
                self.subscribe_button(write_half, env, button_subscribed, btn_tx, forwarders)
                    .await
            }
            Method::MspSubscribe => {
                self.subscribe_msp(write_half, env, msp_subscribed, msp_tx, forwarders)
                    .await
            }
            Method::RadioAuxStreamSubscribe => {
                self.subscribe_aux_stream(write_half, env, aux_subscribed, aux_tx, forwarders)
                    .await
            }
            Method::DisplayZoneSubscribe => {
                self.subscribe_display_zone(write_half, env, dz_subscribed, dz_tx, forwarders)
                    .await
            }
            Method::VisionSubscribeDetections => {
                self.subscribe_detections(write_half, env, detection_subs, det_tx, forwarders)
                    .await
            }
            // Host-coupled methods route to the facade. The three payload-gated
            // methods (mavlink.send, mavlink.register_component,
            // peripheral.register_driver) receive the token's granted caps and
            // apply their capability gate inside the handler, after argument
            // validation, exactly where the Python handlers apply it. NoopHost
            // returns Ok(not_implemented) (mirroring the Python stub bodies); a
            // real host returns Err(HostError) for a soft failure, which becomes
            // the response envelope `error` field with the exact Python wire body.
            // A slow forward runs detached; its reply lands through the loop.
            other if runs_detached(other) => {
                self.spawn_detached(write_half, in_flight, token, other, env)
                    .await
            }
            other => {
                match handlers::route_host_method(
                    &*self.host,
                    other,
                    &self.plugin_id,
                    self.session,
                    &env.args,
                    &token.granted_caps,
                )
                .await
                {
                    Ok(result) => {
                        if other == Method::TelemetryExtend {
                            self.record_extended_channel(&env.args);
                        }
                        send_response(write_half, &env.request_id, result).await
                    }
                    Err(e) => send_error(write_half, &env.request_id, &e.body()).await,
                }
            }
        }
    }

    /// Start a slow host method off the dispatch loop (see [`runs_detached`]).
    /// The gate already passed, so the call carries the caps it was granted.
    /// At [`DETACHED_MAX_IN_FLIGHT`] the next call to finish is answered first.
    pub(super) async fn spawn_detached<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        in_flight: &mut JoinSet<DetachedReply>,
        token: &CapabilityToken,
        method: Method,
        env: &Envelope,
    ) -> Result<(), ServerError> {
        if in_flight.len() >= DETACHED_MAX_IN_FLIGHT {
            if let Some(done) = in_flight.join_next().await {
                write_detached_reply(write_half, done).await?;
            }
        }
        let host = Arc::clone(&self.host);
        let plugin_id = self.plugin_id.clone();
        let session = self.session;
        let args = env.args.clone();
        let granted = token.granted_caps.clone();
        let request_id = env.request_id.clone();
        in_flight.spawn(async move {
            let result =
                handlers::route_host_method(&*host, method, &plugin_id, session, &args, &granted)
                    .await;
            (request_id, result)
        });
        Ok(())
    }
}

/// Whether a host method runs off the connection's dispatch loop.
///
/// These are the stateless forwards whose reply waits on another process or
/// the network: the cloud relay (a record write waits out the cloud's own
/// answer, up to fifteen seconds), the vision engine, the supervisor's
/// video-pipeline restart, and an mDNS browse (it collects answers for its
/// whole window). Awaited inline, each would stall every other request and
/// every push on the connection for its whole round trip.
///
/// Everything else stays inline, in arrival order: the in-process methods
/// answer at once, output commands (MAVLink, MSP, GPIO, aux datagrams) must
/// reach their line in the order sent, and the methods that take or release
/// session state (the aux stream's ownership, an mDNS record) must never land
/// after the session that made them is released.
pub(super) fn runs_detached(method: Method) -> bool {
    matches!(
        method,
        Method::CloudPublish
            | Method::CloudRecordsPut
            | Method::VideoSourceSet
            | Method::VisionRegisterModel
            | Method::VisionInfer
            | Method::VisionPublishDetection
            | Method::VisionDesignateTrack
            | Method::MdnsBrowse
    )
}

/// A detached host-method call's outcome: the request id it answers and the
/// handler's result.
pub(super) type DetachedReply = (String, Result<HostResult, HostError>);

/// Answer a finished detached call. A handler that panicked took its request id
/// with it, so there is nothing to address a reply to: the plugin's own request
/// timeout answers the caller, and the session keeps serving.
pub(super) async fn write_detached_reply<W: AsyncWriteExt + Unpin>(
    write_half: &mut W,
    done: Result<DetachedReply, JoinError>,
) -> Result<(), ServerError> {
    match done {
        Ok((request_id, Ok(result))) => send_response(write_half, &request_id, result).await,
        Ok((request_id, Err(e))) => send_error(write_half, &request_id, &e.body()).await,
        Err(e) => {
            tracing::error!(error = %e, "detached host method did not complete");
            Ok(())
        }
    }
}
