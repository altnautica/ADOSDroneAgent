//! Per-plugin Unix-socket RPC server.
//!
//! Ports `src/ados/plugins/ipc_server.py`. One server instance binds one
//! socket per plugin at `<socket_dir>/<plugin_id>.sock`, accepts the plugin
//! runner's connection, runs the `hello` handshake, then loops on request
//! envelopes: re-check token expiry, gate the method on its required
//! capability, route, and reply.
//!
//! The wire is `ados-protocol` unchanged. Frames are length-prefixed msgpack
//! [`Envelope`]s; the token is the pipe-delimited [`CapabilityToken`] the
//! [`TokenIssuer`] mints and verifies (HMAC-SHA256 over the sorted-caps
//! payload, 600 s TTL). This server re-implements none of that; it composes it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{CapabilityToken, Envelope, TokenIssuer, PROTOCOL_VERSION};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::dispatch::{gate, Gate, Method};
use crate::handlers::{self, Event, EventBus, PublishOutcome};
use crate::host::HostServices;
use crate::invoke::{InvokeRegistry, InvokeRequest};

/// Default per-plugin socket directory. Mirrors `SOCKET_DIR` in Python.
pub const DEFAULT_SOCKET_DIR: &str = "/run/ados/plugins";

/// Errors raised while running one plugin's socket server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Wall-clock unix seconds, clamped to 0 before the epoch (a freshly booted
/// SBC before NTP sync), matching the issuer's clamp.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Wall-clock unix milliseconds. Mirrors `events.now_ms`.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A plugin RPC server bound to one socket. Holds the token issuer and the
/// event bus shared with the rest of the host, plus the host-service facade the
/// dispatcher routes host-coupled methods through.
pub struct PluginIpcServer<H: HostServices> {
    socket_dir: PathBuf,
    token_issuer: Arc<TokenIssuer>,
    bus: Arc<EventBus>,
    host: Arc<H>,
    /// The host-to-plugin invoke seam: each connection registers its outbound
    /// sender here, and the control socket reaches a plugin's tools through it.
    invoke: Arc<InvokeRegistry>,
}

impl<H: HostServices> PluginIpcServer<H> {
    pub fn new(
        socket_dir: impl AsRef<Path>,
        token_issuer: Arc<TokenIssuer>,
        bus: Arc<EventBus>,
        host: Arc<H>,
    ) -> Self {
        Self {
            socket_dir: socket_dir.as_ref().to_path_buf(),
            token_issuer,
            bus,
            host,
            invoke: Arc::new(InvokeRegistry::new()),
        }
    }

    /// The shared invoke registry the control socket forwards a `tool.invoke`
    /// into. Hand this to [`crate::control::serve_control`] so an operator's
    /// `POST /api/plugins/{id}/tools/{tool}/invoke` reaches a live plugin.
    pub fn invoke_registry(&self) -> Arc<InvokeRegistry> {
        self.invoke.clone()
    }

    /// The socket path for a plugin id.
    pub fn socket_path(&self, plugin_id: &str) -> PathBuf {
        self.socket_dir.join(format!("{plugin_id}.sock"))
    }

    /// Bind the per-plugin socket and spawn the accept loop. Returns the bound
    /// path and a handle to the accept task. Mirrors `start_for_plugin`:
    /// create the dir, unlink any stale socket, bind, then set mode 0o660.
    pub fn serve_plugin(&self, plugin_id: &str) -> Result<(PathBuf, JoinHandle<()>), ServerError> {
        let path = self.socket_path(plugin_id);
        // The shared helper owns the create-dir / remove-stale / bind / chmod
        // hygiene: the socket's parent is the per-plugin socket dir, so binding it
        // ensures the dir. 0o660 keeps the socket owner+group rw, matching
        // `os.chmod(sock_path, 0o660)` in the Python server.
        let listener = ados_protocol::ipc::bind_command_socket(&path, 0o660)?;

        let plugin_id = plugin_id.to_string();
        let token_issuer = self.token_issuer.clone();
        let bus = self.bus.clone();
        let host = self.host.clone();
        let socket_dir = self.socket_dir.clone();
        let invoke = self.invoke.clone();
        let task = tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((s, _addr)) => s,
                    Err(_) => break,
                };
                let conn = Connection {
                    plugin_id: plugin_id.clone(),
                    token_issuer: token_issuer.clone(),
                    bus: bus.clone(),
                    host: host.clone(),
                    socket_dir: socket_dir.clone(),
                    invoke: invoke.clone(),
                };
                tokio::spawn(async move {
                    if let Err(err) = conn.run(stream).await {
                        tracing::warn!(
                            plugin_id = %conn.plugin_id,
                            error = %err,
                            "plugin connection ended with an error"
                        );
                    }
                    conn.host.release_plugin(&conn.plugin_id);
                });
            }
        });
        Ok((path, task))
    }

    /// Stop serving a plugin: remove its bound socket file. The caller owns the
    /// accept [`JoinHandle`] returned by [`serve_plugin`] and aborts it; this
    /// only unlinks the socket so a later re-serve binds cleanly and no stale
    /// socket lingers. A live connection's `release_plugin` already runs on
    /// disconnect (see [`serve_plugin`]); aborting the accept task stops new
    /// connections.
    pub fn stop_plugin(&self, plugin_id: &str) {
        let path = self.socket_path(plugin_id);
        let _ = std::fs::remove_file(&path);
        // Drop the plugin's published-state sidecar too, so a stopped plugin
        // does not leave a stale state file the front would keep serving.
        crate::state_sidecar::remove(&self.socket_dir, plugin_id);
    }
}

/// One accepted connection from a plugin runner.
struct Connection<H: HostServices> {
    plugin_id: String,
    token_issuer: Arc<TokenIssuer>,
    bus: Arc<EventBus>,
    host: Arc<H>,
    /// The per-plugin socket directory, used to locate the published-state
    /// sidecar written on each authorized publish.
    socket_dir: PathBuf,
    /// The shared invoke registry: this connection registers its outbound
    /// sender here while its session is up so the control socket can reach it.
    invoke: Arc<InvokeRegistry>,
}

impl<H: HostServices> Connection<H> {
    /// Run the handshake then the dispatch loop. Returns when the peer closes
    /// or a protocol error occurs.
    async fn run(&self, stream: UnixStream) -> Result<(), ServerError> {
        let (mut read_half, mut write_half) = stream.into_split();

        // ---- handshake -------------------------------------------------
        let Some(env) = read_envelope(&mut read_half).await? else {
            return Ok(()); // clean EOF before any frame
        };
        if env.method != "hello" {
            send_error(&mut write_half, "-", "expected hello envelope").await?;
            return Ok(());
        }
        let token = match CapabilityToken::from_token_string(&env.token) {
            Ok(t) => t,
            Err(e) => {
                send_error(
                    &mut write_half,
                    &env.request_id,
                    &format!("capability token invalid: {e}"),
                )
                .await?;
                return Ok(());
            }
        };
        if let Err(e) = self.token_issuer.verify(&token, now_secs()) {
            send_error(
                &mut write_half,
                &env.request_id,
                &format!("capability token invalid: {e}"),
            )
            .await?;
            return Ok(());
        }
        if token.plugin_id != self.plugin_id {
            send_error(
                &mut write_half,
                &env.request_id,
                &format!(
                    "token plugin_id {} does not match socket {}",
                    token.plugin_id, self.plugin_id
                ),
            )
            .await?;
            return Ok(());
        }
        // ready handshake response: {"ready": true}
        send_response(
            &mut write_half,
            &env.request_id,
            Value::Map(vec![(Value::from("ready"), Value::Boolean(true))]),
        )
        .await?;

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
        let mut forwarders: Vec<JoinHandle<()>> = Vec::new();

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

        // ---- dispatch loop --------------------------------------------
        let result = loop {
            // Race the inbound request against an outgoing event or MAVLink frame
            // to deliver, so a long-lived subscriber both serves requests and
            // receives pushes on the one connection (the Python server uses
            // coroutines; one select loop is the single-task equivalent).
            tokio::select! {
                frame = read_envelope(&mut read_half) => {
                    let env = match frame {
                        Ok(Some(env)) => env,
                        Ok(None) => break Ok(()),
                        Err(e) => break Err(e),
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
                            &token,
                            env,
                            &mut subscriptions,
                            &mut mavlink_subs,
                            &mav_tx,
                            &mut vision_subs,
                            &vis_tx,
                            &mut detection_subs,
                            &det_tx,
                            &mut forwarders,
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
                        if !token.granted_caps.contains("mcp.expose") {
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
                            if subscriptions.iter().any(|p| handlers::topic_matches(p, &event.topic)) {
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

        // Stop the per-subscription forwarder tasks so none survive the session.
        for f in forwarders {
            f.abort();
        }
        result
    }

    /// Gate and route one request envelope.
    #[allow(clippy::too_many_arguments)]
    async fn handle_request<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        env: Envelope,
        subscriptions: &mut Vec<String>,
        mavlink_subs: &mut Vec<String>,
        mav_tx: &tokio::sync::mpsc::Sender<MavlinkDelivery>,
        vision_subs: &mut Vec<String>,
        vis_tx: &tokio::sync::mpsc::Sender<VisionDelivery>,
        detection_subs: &mut Vec<String>,
        det_tx: &tokio::sync::mpsc::Sender<DetectionDelivery>,
        forwarders: &mut Vec<JoinHandle<()>>,
    ) -> Result<(), ServerError> {
        let req_id = env.request_id.clone();
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
                    forwarders,
                )
                .await
            }
        }
    }

    /// Run a gated method. The event surface and ping are served in-process;
    /// every other method routes to the host facade.
    #[allow(clippy::too_many_arguments)]
    async fn route<W: AsyncWriteExt + Unpin>(
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
        forwarders: &mut Vec<JoinHandle<()>>,
    ) -> Result<(), ServerError> {
        match method {
            Method::Ping => {
                let result = handlers::ping_result(&self.plugin_id);
                send_response(write_half, &env.request_id, result).await
            }
            Method::EventPublish => {
                match handlers::prepare_publish(
                    &self.plugin_id,
                    &env.args,
                    &token.granted_caps,
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
                match handlers::prepare_subscribe(&self.plugin_id, &env.args, &token.granted_caps) {
                    Ok(pattern) => {
                        if subscriptions.contains(&pattern) {
                            let result = Value::Map(vec![(
                                Value::from("already_subscribed"),
                                Value::Boolean(true),
                            )]);
                            return send_response(write_half, &env.request_id, result).await;
                        }
                        subscriptions.push(pattern);
                        let result =
                            Value::Map(vec![(Value::from("subscribed"), Value::Boolean(true))]);
                        send_response(write_half, &env.request_id, result).await
                    }
                    Err(e) => send_error(write_half, &env.request_id, &e.0).await,
                }
            }
            // mavlink.subscribe both validates (the host method shapes the
            // response) and arms the per-connection push stream. The host
            // returns `{"already_subscribed": true}` or `{"subscribed": true,
            // "msg_name": <m>}`; on the first subscribe to a name we obtain a
            // frame receiver from the host's MAVLink client (if wired) and spawn
            // a forwarder that tags each frame with the msg_name into the merged
            // delivery channel, so the select loop pushes `mavlink.deliver`
            // envelopes. Mirrors `handle_mavlink_subscribe` + `spawn_mavlink_pump`.
            Method::MavlinkSubscribe => {
                let Some(name) = mavlink_subscribe_msg_name(&env.args) else {
                    return send_error(
                        write_half,
                        &env.request_id,
                        "msg_name must be a non-empty string",
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
                // Arm the push stream when the host has a MAVLink client. The
                // forwarder tags every frame with this name and forwards into the
                // merged channel; it forwards every frame the host fans out (no
                // per-name byte filtering), matching the Python pump.
                if let Some(mut rx) = self.host.mavlink_subscribe_stream(&self.plugin_id, &name) {
                    let tx = mav_tx.clone();
                    let fwd_name = name.clone();
                    forwarders.push(tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(frame) => {
                                    let delivery = MavlinkDelivery {
                                        msg_name: fwd_name.clone(),
                                        frame,
                                    };
                                    if tx.send(delivery).await.is_err() {
                                        break; // connection gone
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }));
                }
                let result = Value::Map(vec![
                    (Value::from("subscribed"), Value::Boolean(true)),
                    (Value::from("msg_name"), Value::from(name.as_str())),
                ]);
                send_response(write_half, &env.request_id, result).await
            }
            // vision.subscribe_frames arms the per-connection frame-descriptor
            // push stream, exactly like mavlink.subscribe arms the frame pump. On
            // the first subscribe to a camera we obtain a descriptor receiver from
            // the host's vision client (if wired) and spawn a forwarder that pushes
            // each descriptor into the merged delivery channel, so the select loop
            // writes `vision.deliver` envelopes. A camera_id of "" subscribes to
            // every camera (the host fans out all descriptors); the response echoes
            // the requested camera_id.
            Method::VisionSubscribeFrames => {
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
                    forwarders.push(tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(descriptor) => {
                                    if tx.send(VisionDelivery { descriptor }).await.is_err() {
                                        break; // connection gone
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }));
                }
                let result = Value::Map(vec![
                    (Value::from("subscribed"), Value::Boolean(true)),
                    (Value::from("camera_id"), Value::from(camera_id.as_str())),
                ]);
                send_response(write_half, &env.request_id, result).await
            }
            // vision.subscribe_detections arms the per-connection detection-batch
            // push stream, exactly like vision.subscribe_frames arms the frame
            // pump. On the first subscribe to a camera we obtain a batch receiver
            // from the host's vision client (if wired) and spawn a forwarder that
            // pushes each batch into the merged delivery channel, so the select
            // loop writes `vision.deliver_detection` envelopes. A camera_id of ""
            // subscribes to every camera (the host fans out all batches); the
            // response echoes the requested camera_id.
            Method::VisionSubscribeDetections => {
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
                    forwarders.push(tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(batch) => {
                                    if tx.send(DetectionDelivery { batch }).await.is_err() {
                                        break; // connection gone
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }));
                }
                let result = Value::Map(vec![
                    (Value::from("subscribed"), Value::Boolean(true)),
                    (Value::from("camera_id"), Value::from(camera_id.as_str())),
                ]);
                send_response(write_half, &env.request_id, result).await
            }
            // Host-coupled methods route to the facade. The three payload-gated
            // methods (mavlink.send, mavlink.register_component,
            // peripheral.register_driver) receive the token's granted caps and
            // apply their capability gate inside the handler, after argument
            // validation, exactly where the Python handlers apply it. NoopHost
            // returns Ok(not_implemented) (mirroring the Python stub bodies); a
            // real host returns Err(HostError) for a soft failure, which becomes
            // the response envelope `error` field with the exact Python wire body.
            other => {
                match handlers::route_host_method(
                    &*self.host,
                    other,
                    &self.plugin_id,
                    &env.args,
                    &token.granted_caps,
                )
                .await
                {
                    Ok(result) => send_response(write_half, &env.request_id, result).await,
                    Err(e) => send_error(write_half, &env.request_id, &e.body()).await,
                }
            }
        }
    }

    /// Push a matched event to the plugin as an `event.deliver` envelope.
    /// Mirrors `_pump_subscription`.
    async fn deliver_event<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        event: &Event,
    ) -> Result<(), ServerError> {
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

    /// Push a subscribed MAVLink frame to the plugin as a `mavlink.deliver`
    /// envelope. Mirrors the envelope shape in `mavlink_pump._pump_loop`:
    /// kind `event`, method `mavlink.deliver`, capability `mavlink.read`, args
    /// `{msg_name, frame: bytes, timestamp_ms}`, request_id `mav-<ms>`.
    async fn deliver_mavlink<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        msg_name: &str,
        frame: &[u8],
    ) -> Result<(), ServerError> {
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
    async fn deliver_vision_frame<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        descriptor: &[u8],
    ) -> Result<(), ServerError> {
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

    /// Push a vision detection batch to the plugin as a `vision.deliver_detection`
    /// envelope. Mirrors the frame deliver shape: kind `event`, method
    /// `vision.deliver_detection`, capability `vision.detection.subscribe`, args
    /// `{batch: bytes, timestamp_ms}`, request_id `vis-det-<ms>`. The `batch`
    /// bytes are an encoded `ados_protocol::framebus::DetectionBatch`; the plugin
    /// decodes it to read the detections.
    async fn deliver_vision_detection<W: AsyncWriteExt + Unpin>(
        &self,
        write_half: &mut W,
        token: &CapabilityToken,
        batch: &[u8],
    ) -> Result<(), ServerError> {
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

/// One subscribed MAVLink frame to push, tagged with the subscription's
/// msg_name. The per-subscription forwarder builds these into the merged
/// delivery channel the connection's select loop drains.
struct MavlinkDelivery {
    msg_name: String,
    frame: Vec<u8>,
}

/// Extract the `msg_name` arg for `mavlink.subscribe`: a non-empty string, else
/// `None` (the `_rpc_error("msg_name must be a non-empty string")` path).
fn mavlink_subscribe_msg_name(args: &Value) -> Option<String> {
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
struct VisionDelivery {
    descriptor: Vec<u8>,
}

/// One subscribed vision detection batch to push. The per-subscription forwarder
/// builds these into the merged delivery channel the connection's select loop
/// drains. The bytes are an encoded `ados_protocol::framebus::DetectionBatch`.
struct DetectionDelivery {
    batch: Vec<u8>,
}

/// Extract the `camera_id` arg for `vision.subscribe_frames`. Unlike the MAVLink
/// `msg_name`, an absent or empty `camera_id` is valid and means "every camera"
/// (the host fans out all descriptors), so this returns a plain `String` that is
/// `""` for the subscribe-all case rather than an `Option`.
fn vision_subscribe_camera_id(args: &Value) -> String {
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

/// Read one length-prefixed msgpack envelope. `Ok(None)` on clean EOF before
/// the header. Uses `ados-protocol` framing (plugin contract: reject zero,
/// 4 MiB cap) — no wire re-implementation.
async fn read_envelope<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<Envelope>, ServerError> {
    let mut header = [0u8; HEADER_SIZE];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = decode_len(header, PLUGIN_MAX_FRAME, true)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let env = Envelope::from_msgpack(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some(env))
}

/// Write one envelope as a length-prefixed frame and flush.
async fn write_frame<W: AsyncWriteExt + Unpin>(
    write_half: &mut W,
    env: &Envelope,
) -> Result<(), ServerError> {
    let frame = env
        .encode_frame()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    write_half.write_all(&frame).await?;
    write_half.flush().await?;
    Ok(())
}

/// Send a `response` envelope carrying `result` as its args. Mirrors
/// `_send_response`.
async fn send_response<W: AsyncWriteExt + Unpin>(
    write_half: &mut W,
    request_id: &str,
    result: Value,
) -> Result<(), ServerError> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: "response".to_string(),
        capability: String::new(),
        args: result,
        request_id: request_id.to_string(),
        token: String::new(),
        error: None,
    };
    write_frame(write_half, &env).await
}

/// Send a `response` envelope with the `error` field set. Mirrors
/// `_send_error`.
async fn send_error<W: AsyncWriteExt + Unpin>(
    write_half: &mut W,
    request_id: &str,
    message: &str,
) -> Result<(), ServerError> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: "response".to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: Some(message.to_string()),
    };
    write_frame(write_half, &env).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::NoopHost;
    use std::time::Duration;

    #[tokio::test]
    async fn stop_plugin_removes_the_socket_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let issuer = Arc::new(TokenIssuer::new(b"stop-plugin-secret".to_vec()));
        let bus = Arc::new(EventBus::new());
        let host = Arc::new(NoopHost);
        let server = PluginIpcServer::new(dir.path(), issuer, bus, host);

        let (path, accept) = server.serve_plugin("com.example.demo").expect("serve");
        // The socket is bound and connectable.
        let mut connected = false;
        for _ in 0..50 {
            if UnixStream::connect(&path).await.is_ok() {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(connected, "expected the bound socket to be connectable");
        assert!(path.exists(), "socket file should exist while served");

        // Stop: the caller aborts the accept task, stop_plugin unlinks the file.
        accept.abort();
        server.stop_plugin("com.example.demo");
        assert!(
            !path.exists(),
            "socket file should be gone after stop_plugin"
        );
    }

    #[tokio::test]
    async fn tool_invoke_reaches_a_connected_plugin() {
        use std::collections::BTreeSet;

        let dir = tempfile::tempdir().expect("tempdir");
        let issuer = Arc::new(TokenIssuer::new(b"invoke-secret".to_vec()));
        let bus = Arc::new(EventBus::new());
        let host = Arc::new(NoopHost);
        let server = PluginIpcServer::new(dir.path(), issuer.clone(), bus, host);
        let registry = server.invoke_registry();

        let plugin_id = "com.example.demo";
        let (path, accept) = server.serve_plugin(plugin_id).expect("serve");

        // Connect a fake runner once the socket is bound.
        let mut stream = None;
        for _ in 0..50 {
            if let Ok(s) = UnixStream::connect(&path).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (mut rd, mut wr) = stream.expect("connect").into_split();

        // Handshake with an mcp.expose token.
        let mut caps = BTreeSet::new();
        caps.insert("mcp.expose".to_string());
        let token = issuer.mint(plugin_id, &caps, 600).to_token_string();
        let hello = Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: "hello".to_string(),
            capability: String::new(),
            args: Value::Map(vec![]),
            request_id: "h1".to_string(),
            token,
            error: None,
        };
        write_frame(&mut wr, &hello).await.expect("send hello");
        let ready = read_envelope(&mut rd)
            .await
            .expect("read ready")
            .expect("ready envelope");
        assert_eq!(ready.kind, "response");

        // The fake runner answers one tool.invoke by echoing the tool name.
        let runner = tokio::spawn(async move {
            while let Some(env) = read_envelope(&mut rd).await.expect("read") {
                if env.kind == "request" && env.method == "tool.invoke" {
                    let tool = match &env.args {
                        Value::Map(pairs) => pairs
                            .iter()
                            .find_map(|(k, v)| {
                                (k.as_str() == Some("tool"))
                                    .then(|| v.as_str().unwrap_or("").to_string())
                            })
                            .unwrap_or_default(),
                        _ => String::new(),
                    };
                    let resp = Envelope {
                        version: PROTOCOL_VERSION,
                        kind: "response".to_string(),
                        method: "response".to_string(),
                        capability: String::new(),
                        args: Value::Map(vec![(Value::from("ran"), Value::from(tool))]),
                        request_id: env.request_id.clone(),
                        token: String::new(),
                        error: None,
                    };
                    write_frame(&mut wr, &resp).await.expect("reply");
                }
            }
        });

        // Wait for the connection to register its invoke sender post-handshake.
        for _ in 0..50 {
            if registry.is_connected(plugin_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            registry.is_connected(plugin_id),
            "connection must register invoke"
        );

        let out = registry
            .invoke(
                plugin_id,
                "greet",
                Value::Map(vec![]),
                Duration::from_secs(2),
            )
            .await
            .expect("invoke result");
        assert_eq!(
            out,
            Value::Map(vec![(Value::from("ran"), Value::from("greet"))])
        );

        runner.abort();
        accept.abort();
        server.stop_plugin(plugin_id);
    }
}
