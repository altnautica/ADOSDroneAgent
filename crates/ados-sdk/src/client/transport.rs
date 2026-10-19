//! Request/reply plumbing and the socket reader loop.

use super::*;

impl PluginIpcClient {
    /// Send a request under the client's default timeout. See
    /// [`send_request_within`](Self::send_request_within).
    pub(super) async fn send_request(
        &self,
        method: &str,
        capability: &str,
        args: Value,
    ) -> Result<Envelope, ClientError> {
        self.send_request_within(method, capability, args, self.request_timeout)
            .await
    }

    /// Mint a request id, park a waiter, write the frame, then await the routed
    /// response for at most `timeout`. Mirrors `_send_request`, including the
    /// error-string mapping to typed `capability_denied` /
    /// `allowlist_violation` errors.
    pub(super) async fn send_request_within(
        &self,
        method: &str,
        capability: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Envelope, ClientError> {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let rid = format!("r{n}");
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: method.to_string(),
            capability: capability.to_string(),
            args,
            request_id: rid.clone(),
            // Read per request, not captured at construction: a rotation mid
            // session must be what the next frame carries.
            token: self.token.lock().expect("token lock").clone(),
            error: None,
        };
        let frame = env.encode_frame()?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("pending lock");
            pending.insert(rid.clone(), tx);
        }

        // Write under the shared async lock so concurrent senders serialize.
        {
            let mut guard = self.writer.lock().await;
            let writer = guard.as_mut().ok_or(ClientError::NotConnected);
            match writer {
                Ok(w) => {
                    if let Err(e) = w.write_all(&frame).await.and(w.flush().await) {
                        self.pending.lock().expect("pending lock").remove(&rid);
                        return Err(ClientError::Io(e));
                    }
                }
                Err(_) => {
                    self.pending.lock().expect("pending lock").remove(&rid);
                    return Err(ClientError::NotConnected);
                }
            }
        }

        let response = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(env)) => env,
            // The waiter was dropped: the reader loop ended before the response.
            Ok(Err(_)) => {
                self.pending.lock().expect("pending lock").remove(&rid);
                return Err(ClientError::ConnectionClosed);
            }
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&rid);
                return Err(ClientError::Timeout(timeout));
            }
        };

        if let Some(err) = &response.error {
            return Err(map_error_string(err));
        }
        Ok(response)
    }

    /// Spawn the reader loop. Owns the read half; routes responses by
    /// `request_id`, adopts a rotated capability token, and dispatches
    /// `event` / `mavlink.deliver` pushes to the registered callbacks.
    /// Mirrors `_reader_loop`.
    pub(super) fn spawn_reader_loop(&self, read_half: OwnedReadHalf) -> JoinHandle<()> {
        let pending = self.pending.clone();
        let event_callbacks = self.event_callbacks.clone();
        let mavlink_callbacks = self.mavlink_callbacks.clone();
        let vision_callbacks = self.vision_callbacks.clone();
        let detection_callbacks = self.detection_callbacks.clone();
        let button_callbacks = self.button_callbacks.clone();
        let msp_callbacks = self.msp_callbacks.clone();
        let aux_callbacks = self.aux_callbacks.clone();
        let tool_handler = self.tool_handler.clone();
        let writer = self.writer.clone();
        let token = self.token.clone();
        let plugin_id = self.plugin_id.clone();
        let session_lost = self.session_lost.clone();
        tokio::spawn(async move {
            let mut reader = read_half;
            loop {
                match read_envelope(&mut reader).await {
                    Ok(Some(env)) => {
                        if env.kind == "event" {
                            if env.method == "mavlink.deliver" {
                                dispatch_mavlink(&mavlink_callbacks, &env);
                            } else if env.method == ados_protocol::framebus::methods::DELIVER_FRAME
                            {
                                dispatch_vision(&vision_callbacks, &env);
                            } else if env.method
                                == ados_protocol::framebus::methods::DELIVER_DETECTION
                            {
                                dispatch_detection(&detection_callbacks, &env);
                            } else if env.method == ados_protocol::buttons::DELIVER {
                                dispatch_button(&button_callbacks, &env);
                            } else if env.method == "msp.deliver" {
                                dispatch_button(&msp_callbacks, &env);
                            } else if env.method == TOKEN_REFRESH_METHOD {
                                adopt_refreshed_token(&token, &plugin_id, &env);
                            } else if env.method == "radio.aux_stream.deliver" {
                                // Wildcard-only fan-out, exactly like buttons:
                                // every aux subscriber receives every datagram
                                // and its own codec routes by channel.
                                dispatch_button(&aux_callbacks, &env);
                            } else {
                                dispatch_event(&event_callbacks, &env);
                            }
                        } else if env.kind == "request" && env.method == TOOL_INVOKE_METHOD {
                            let handler = tool_handler.lock().expect("tool handler lock").clone();
                            tokio::spawn(answer_tool_invoke(handler, writer.clone(), env));
                        } else {
                            // request/response: route to the parked waiter.
                            if let Some(tx) = pending
                                .lock()
                                .expect("pending lock")
                                .remove(&env.request_id)
                            {
                                let _ = tx.send(env);
                            }
                        }
                    }
                    // Clean EOF (peer closed) or a frame error: end the loop.
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(plugin_id = %plugin_id, error = %e, "plugin ipc reader loop ended");
                        break;
                    }
                }
            }
            // Drop every parked waiter so in-flight requests fail fast with
            // ConnectionClosed rather than waiting out their timeout.
            pending.lock().expect("pending lock").clear();
            session_lost.send_replace(true);
        })
    }
}

/// Run one host-to-plugin `tool.invoke` and write its `response` frame. The
/// request args are `{tool, arguments}`; the response carries the tool result
/// as args, or the error string in `err`.
pub(super) async fn answer_tool_invoke(
    handler: Option<ToolHandler>,
    writer: Arc<AsyncMutex<Option<OwnedWriteHalf>>>,
    req: Envelope,
) {
    let outcome = match handler {
        None => Err("tool_handler_missing".to_string()),
        Some(h) => match map_get_str(&req.args, "tool") {
            None => Err("tool name missing".to_string()),
            Some(tool) => {
                let arguments = map_get(&req.args, "arguments").unwrap_or(Value::Nil);
                h(tool.to_string(), arguments).await
            }
        },
    };
    let (args, error) = match outcome {
        Ok(v) => (v, None),
        Err(e) => (Value::Map(Vec::new()), Some(e)),
    };
    let reply = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: TOOL_INVOKE_METHOD.to_string(),
        capability: req.capability,
        args,
        request_id: req.request_id,
        token: String::new(),
        error,
    };
    let Ok(frame) = reply.encode_frame() else {
        return;
    };
    let mut guard = writer.lock().await;
    if let Some(w) = guard.as_mut() {
        if w.write_all(&frame).await.and(w.flush().await).is_err() {
            tracing::warn!("tool.invoke reply could not be written");
        }
    }
}
