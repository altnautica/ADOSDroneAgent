//! The WebSocket proxy.

use super::*;

/// A WebSocket session's admission inputs, kept for the periodic re-check.
pub(super) struct WsSession {
    pub(super) caller: CallerClass,
    pub(super) key: Option<String>,
    pub(super) ticket_posture: Option<Pairing>,
}

/// Pull the ticket value that follows the `ados-ws-ticket` marker in the offered
/// subprotocol list (`["ados-ws-ticket", "<token>"]`). The token itself is
/// pipe-delimited and carries no commas, so it survives the comma-split of the
/// `Sec-WebSocket-Protocol` header intact.
pub(super) fn extract_ticket(offered: &[String]) -> Option<&str> {
    let pos = offered.iter().position(|p| p == WS_TICKET_SUBPROTOCOL)?;
    offered.get(pos + 1).map(String::as_str)
}

/// Parse the offered WebSocket subprotocols from the handshake headers. Values
/// may be split across multiple `Sec-WebSocket-Protocol` headers and/or
/// comma-joined within one; flatten both forms.
pub(super) fn offered_subprotocols(
    headers: &tokio_tungstenite::tungstenite::http::HeaderMap,
) -> Vec<String> {
    headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The WebSocket limits: messages and frames capped at
/// [`edge::WS_MAX_MESSAGE`], so an admitted client cannot make the router
/// buffer tens of megabytes per socket for writes the FC path would refuse.
pub(super) fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(edge::WS_MAX_MESSAGE),
        max_frame_size: Some(edge::WS_MAX_MESSAGE),
        ..WebSocketConfig::default()
    }
}

/// WebSocket MAVLink proxy. Binds `<bind_addr>:<port>` ([`ws_bind_addr`],
/// retrying like [`run_tcp_proxy`]) and bridges binary WebSocket frames to/from
/// the FC, the way a browser GCS connects. Text/ping frames are ignored; only
/// binary frames carry MAVLink.
///
/// `auth` gates the handshake by the agent's pairing posture (see
/// [`WsProxyAuth`]).
pub async fn run_ws_proxy(
    fc: Arc<FcConnection>,
    bind_addr: &str,
    port: u16,
    auth: WsProxyAuth,
    listening: ListenFlag,
    cancel: Shutdown,
) {
    let bound = bind_until(
        &cancel,
        || TcpListener::bind((bind_addr, port)),
        |e| tracing::warn!(bind_addr, port, error = %e, "ws_proxy_bind_failed"),
    )
    .await;
    let Some(listener) = bound else {
        return;
    };
    let _listening = ListeningGuard::new(&listening);
    tracing::info!(
        bind_addr,
        port,
        enforce_auth = auth.enforce,
        "ws_proxy_listening"
    );
    let mut accept_errors = AcceptErrors::default();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, addr)) => {
                    tokio::spawn(handle_ws_client(fc.clone(), stream, addr, auth.clone()));
                }
                Err(e) => {
                    accept_errors.on_error(port, &e).await;
                }
            },
            _ = cancel.wait() => return,
        }
    }
}

/// The handshake-inspection result computed in the `accept_hdr_async` callback
/// and acted on after the handshake completes.
pub(super) struct HandshakeDecision {
    pub(super) admit: bool,
    pub(super) access: Access,
    pub(super) session: WsSession,
}

// The handshake-rejection callback returns the WebSocket library's
// `ErrorResponse` (an `http::Response<Option<String>>`), whose size the callback
// signature fixes; it cannot be boxed without changing the upstream contract, so
// the large-Err lint is allowed for this one site.
#[allow(clippy::result_large_err)]
pub(super) async fn handle_ws_client(
    fc: Arc<FcConnection>,
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    auth: WsProxyAuth,
) {
    // Classify the caller from the peer and the handshake headers (a
    // forwarding header makes it remote), resolve the posture, and (with
    // enforcement on) reject an unauthorized connection at the handshake. With
    // enforcement off the callback admits everything; the log happens after the
    // handshake from the captured decision.
    let local = stream.local_addr().ok().map(|a| a.ip());
    let decision: Arc<StdMutex<Option<HandshakeDecision>>> = Arc::new(StdMutex::new(None));
    let callback_decision = decision.clone();
    let callback_auth = auth.clone();
    let ws = tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        move |req: &HandshakeRequest, mut response: HandshakeResponse| {
            let (admit, access) = callback_auth.handshake_decision(peer.ip(), local, req.headers());
            let session = callback_auth.ws_session(peer.ip(), local, req.headers(), access);
            *callback_decision.lock().unwrap_or_else(|p| p.into_inner()) = Some(HandshakeDecision {
                admit,
                access,
                session,
            });
            if admit {
                // If the client offered the ticket subprotocol, the server MUST
                // select one of the offered subprotocols (RFC 6455 §4.2.2) or a
                // browser handshake fails — echo the marker back. We never echo
                // the token itself, only the marker.
                if offered_subprotocols(req.headers())
                    .iter()
                    .any(|p| p == WS_TICKET_SUBPROTOCOL)
                {
                    response.headers_mut().insert(
                        SEC_WEBSOCKET_PROTOCOL,
                        HeaderValue::from_static(WS_TICKET_SUBPROTOCOL),
                    );
                }
                Ok(response)
            } else if callback_auth.current() == Pairing::Unpaired {
                // The same refusal the HTTP edge gives an unpaired node's
                // non-lifeline caller: there is no key it could have sent.
                let mut err = ErrorResponse::new(Some(
                    "This device is not paired yet. Pair it first, or reach it over its hotspot or USB connection."
                        .to_string(),
                ));
                *err.status_mut() = StatusCode::FORBIDDEN;
                Err(err)
            } else {
                let mut err = ErrorResponse::new(Some(
                    "Missing X-ADOS-Key header. This agent is paired and requires authentication."
                        .to_string(),
                ));
                *err.status_mut() = StatusCode::UNAUTHORIZED;
                err.headers_mut()
                    .insert("x-ados-auth", HeaderValue::from_static("required"));
                Err(err)
            }
        },
        Some(ws_config()),
    )
    .await;

    // Surface the posture decision regardless of admit/reject, so the
    // observe-only stage produces the same audit signal a bench session uses
    // before flipping enforcement on.
    // Carried into the send path so the relay fallback can record when the
    // bytes it radiates came from a caller that did not clear the gate.
    let mut ws_origin = ClientOrigin::Trusted;
    let mut session = None;
    if let Some(d) = decision.lock().unwrap_or_else(|p| p.into_inner()).take() {
        ws_origin = origin_of(d.access);
        session = Some(d.session);
        if d.access == Access::Unauthorized {
            tracing::warn!(
                peer = %peer,
                enforce_auth = auth.enforce,
                admitted = d.admit,
                "ws_proxy_unauthorized"
            );
        }
    }

    let ws = match ws {
        Ok(w) => w,
        // A rejected handshake (enforcement on) or a malformed one both land
        // here; the rejection already wrote the 401 response.
        Err(_) => return,
    };
    let (mut write, mut read) = ws.split();
    let (mut rx, route_ep) = fc.routes().client_feed("ws", fc.subscribe());
    let mut raw_rx = fc.subscribe_raw();

    // FC -> client (binary frames).
    let writer = tokio::spawn(async move {
        loop {
            // Forward whichever lane produces bytes: the MAVLink frame lane for a
            // MAVLink FC, the raw byte lane for an MSP FC. Exactly one lane is
            // populated for a given FC, so there is no duplication.
            let bytes = tokio::select! {
                r = rx.recv() => match r {
                    Ok(frame) => frame,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
                r = raw_rx.recv() => match r {
                    Ok(raw) => raw,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
            };
            // tungstenite 0.24's `Binary` owns a `Vec`, so this lane pays one
            // copy per frame per WebSocket client — exactly what it paid before,
            // when the fan-out handed every consumer its own `Vec`. Every other
            // consumer now avoids that copy.
            if write.send(Message::Binary(bytes.to_vec())).await.is_err() {
                break;
            }
        }
    });

    // client -> FC (binary frames only; ignore text/ping/pong), whole MAVLink
    // frames only. A message over the size cap ends the stream with an error.
    // The admission is re-checked every `AUTH_RECHECK`, so a pair, unpair or
    // key rotation closes a session that no longer passes.
    let mut framer = ClientFramer::default();
    let mut recheck = recheck_interval();
    loop {
        tokio::select! {
            msg = read.next() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    forward_client_bytes(&fc, &mut framer, &data, ws_origin, route_ep.as_ref()).await
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = recheck.tick() => {
                if session.as_ref().is_some_and(|s| !auth.ws_still_admitted(s)) {
                    tracing::warn!(peer = %peer, "ws_proxy_session_revoked");
                    break;
                }
            }
        }
    }
    writer.abort();
    if let Some(ep) = &route_ep {
        fc.routes().retire(ep);
    }
}
