//! One-shot request/response serving and framed read helpers.

use super::*;

/// Serve a one-shot request/response command socket.
///
/// The accept loop never exits on a transient `accept()` error: it logs, backs
/// off briefly, and retries, so a momentary fd-pressure hiccup cannot silently
/// kill the command surface for the process lifetime (a command socket that
/// dies while the service stays up would need a manual restart). Each
/// connection is handled on its own task: read exactly one newline-terminated
/// request (capped at
/// `max_request` bytes so a peer that never sends a newline cannot grow the
/// buffer without bound; an over-cap request closes the connection with no
/// response), call `handler` with the request bytes (trailing newline
/// stripped), write the returned bytes followed by a single trailing newline,
/// flush, and close.
///
/// `handler` carries the pure parse + dispatch: it takes the raw request line
/// and returns the raw response line. Framing (the trailing newline) and the
/// socket lifecycle are owned here, so a service only supplies its dispatch.
///
/// Use this only for sockets that are unambiguously one-shot newline
/// request/response. A length-prefixed, streaming, stateful (handshake), or
/// multi-request socket keeps its own accept loop.
pub async fn serve_rpc<H, Fut>(listener: OperatorListener, max_request: usize, handler: H)
where
    H: Fn(Vec<u8>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Vec<u8>> + Send + 'static,
{
    loop {
        let mut stream = match listener.accept().await {
            Ok((s, _addr)) => s,
            Err(e) => {
                // Backoff so a persistent accept error cannot hot-spin; never
                // exit, so the command surface survives transient fd pressure.
                tracing::warn!(error = %e, "command socket accept failed");
                tokio::time::sleep(ACCEPT_RETRY_BACKOFF).await;
                continue;
            }
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            // One newline-terminated request per connection. A clean EOF before
            // any request, or a line that exceeds the cap, just closes.
            let request = match read_newline_line(&mut stream, max_request).await {
                Ok(Some(req)) => req,
                Ok(None) | Err(_) => return,
            };
            let mut response = handler(request).await;
            response.push(b'\n');
            if stream.write_all(&response).await.is_err() {
                return;
            }
            let _ = stream.flush().await;
            // `stream` drops here, closing the connection.
        });
    }
}

/// Connect to a Unix socket with bounded retry (as the Python
/// `StateIPCClient.connect` does). Returns the stream once connected.
pub async fn connect_with_retry(
    path: impl AsRef<Path>,
    retries: u32,
    delay: Duration,
) -> io::Result<UnixStream> {
    let path = path.as_ref();
    let mut last_err: Option<io::Error> = None;
    for attempt in 0..retries.max(1) {
        match UnixStream::connect(path).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < retries {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connect failed")))
}

/// Read one length-prefixed frame from a stream. Returns `Ok(None)` on clean
/// EOF. Used to consume the MAVLink broadcast on the client side.
pub async fn read_length_prefixed<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    max: usize,
    reject_zero: bool,
) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; frame::HEADER_SIZE];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = frame::decode_len(header, max, reject_zero)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload).await?;
    }
    Ok(Some(payload))
}

/// Read one newline-terminated line (state socket v1) from a stream. Returns
/// `Ok(None)` on clean EOF. The returned buffer excludes the trailing newline.
///
/// `max` caps the line length so a peer that never sends a newline cannot grow
/// the buffer without bound. A line that reaches `max` without a newline
/// returns `InvalidData`.
pub async fn read_newline_line<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    max: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read_exact(&mut byte).await {
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(Some(buf));
                }
                if buf.len() >= max {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "state line exceeded maximum length without a newline",
                    ));
                }
                buf.push(byte[0]);
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(buf))
                };
            }
            Err(e) => return Err(e),
        }
    }
}
