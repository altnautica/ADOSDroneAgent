//! Response socket listener and the ingest writer.

use super::*;

/// Fixed delay between response-listener bind attempts. A bind failure (the
/// path held by a directory, a missing parent the process cannot create) is
/// retried until it succeeds or the listener is cancelled, so the response
/// dispatch path is never disabled for the rest of the process lifetime.
pub(super) const BIND_RETRY_DELAY: Duration = Duration::from_secs(3);

/// The listener loop: bind, accept connections, read length-prefixed Response
/// payloads, dispatch to pending callers.
pub(super) async fn run_response_listener(
    proxy: AuxRpcProxy,
    sock_path: PathBuf,
    cancel: Arc<Notify>,
) {
    // The bind helper removes a stale socket file left by an unclean shutdown,
    // sets the mode and the operator group, and re-checks every peer on accept.
    let listener = loop {
        match ipc::bind_command_socket(&sock_path, 0o660) {
            Ok(l) => break l,
            Err(e) => {
                tracing::error!(error = %e, path = %sock_path.display(), "aux_rpc_response_listener_bind_failed");
                tokio::select! {
                    biased;
                    _ = cancel.notified() => return,
                    _ = tokio::time::sleep(BIND_RETRY_DELAY) => {}
                }
            }
        }
    };
    tracing::info!(path = %sock_path.display(), "aux_rpc_response_listener_listening");

    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => break,
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let proxy = proxy.clone();
                        tokio::spawn(handle_one_connection(stream, proxy));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "aux_rpc_response_listener_accept_failed");
                        tokio::time::sleep(ipc::ACCEPT_RETRY_BACKOFF).await;
                    }
                }
            }
        }
    }
    // Shutdown: complete every waiter now rather than leaving each one to burn
    // its full 10-second bound against a consumer that is already gone.
    proxy.reset().await;
    tracing::info!("aux_rpc_response_listener_stopped");
    let _ = std::fs::remove_file(&sock_path);
}

/// Read length-prefixed Response payloads off one accepted connection until EOF.
///
/// [`AuxRpcResponseIngest::send`] writes exactly one fragment per
/// length-prefixed frame, so each read is one whole payload. It is decoded on
/// its own rather than accumulated: a fragmented response is 25 frames on the
/// wire, and one undecodable frame appended to a shared buffer would poison
/// every frame after it for the life of the connection.
pub(super) async fn handle_one_connection(mut stream: UnixStream, proxy: AuxRpcProxy) {
    loop {
        match read_one_frame(&mut stream).await {
            Ok(payload) => match aux_rpc::decode_response(&payload) {
                Ok(response) => proxy.dispatch_response(&response).await,
                Err(e) => {
                    tracing::debug!(error = ?e, len = payload.len(), "aux_rpc_response_undecodable");
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    tracing::debug!(error = %e, "aux_rpc_response_reader_read_failed");
                }
                break;
            }
        }
    }
}

/// Read one length-prefixed frame off the stream. Returns the payload bytes.
pub(super) async fn read_one_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    if len > aux_mux::AUX_MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response frame exceeds the aux payload maximum",
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).await?;
    }
    Ok(payload)
}

impl AuxRpcResponseIngest {
    /// An ingest writing to the given Unix socket path.
    pub fn new(sock_path: impl Into<PathBuf>) -> Self {
        Self {
            sock_path: sock_path.into(),
            conn: Arc::new(Mutex::new(None)),
        }
    }

    /// Forward one Response payload as a length-prefixed frame.
    ///
    /// Best-effort: a failed or slow write drops the frame and drops the
    /// connection so the next call re-connects.
    ///
    /// Both the connect and the write are BOUNDED, and that is the whole
    /// point. The caller awaits this inside the aux consumer's single
    /// `recv_from` loop, so an unbounded write to a proxy that has stopped
    /// reading its socket stalls that slot's entire read loop — not just RPC,
    /// but that drone's MAVLink ingest, its status frames and its peer cache.
    /// The doc used to claim this never blocks while `write_all` on a full
    /// socket buffer did exactly that.
    pub async fn send(&self, payload: &[u8]) {
        // Length-prefix the payload: 4-byte BE length + body, matching the
        // frame module's framing shared across all the agent's IPC.
        let len = payload.len() as u32;
        let mut frame = Vec::with_capacity(HEADER_SIZE + payload.len());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(payload);
        let mut guard = self.conn.lock().await;
        match guard.as_mut() {
            Some(stream) => {
                let wrote = tokio::time::timeout(IPC_WRITE_TIMEOUT, stream.write_all(&frame)).await;
                // A timeout and an error are treated alike: drop the
                // connection so the next call reconnects. A peer that stopped
                // reading is not going to start because we waited longer, and
                // the caller's read loop is what pays for the wait.
                if !matches!(wrote, Ok(Ok(()))) {
                    if wrote.is_err() {
                        tracing::warn!(
                            timeout_ms = IPC_WRITE_TIMEOUT.as_millis() as u64,
                            "aux_rpc_ingest_write_timed_out"
                        );
                    }
                    *guard = None;
                }
            }
            None => {
                match tokio::time::timeout(
                    IPC_CONNECT_TIMEOUT,
                    UnixStream::connect(&self.sock_path),
                )
                .await
                {
                    Ok(Ok(mut stream)) => {
                        let wrote =
                            tokio::time::timeout(IPC_WRITE_TIMEOUT, stream.write_all(&frame)).await;
                        if matches!(wrote, Ok(Ok(()))) {
                            *guard = Some(stream);
                        }
                    }
                    Ok(Err(_)) => {
                        // No listener yet; the proxy will start eventually.
                    }
                    Err(_) => {
                        tracing::warn!(
                            timeout_ms = IPC_CONNECT_TIMEOUT.as_millis() as u64,
                            "aux_rpc_ingest_connect_timed_out"
                        );
                    }
                }
            }
        }
    }
}
