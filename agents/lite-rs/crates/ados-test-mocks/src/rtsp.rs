//! Minimal RTSP/1.0 server fixture for tests.
//!
//! Pure tokio + bytes. Handles only the verbs the lite agent's RTSP
//! push pipeline issues:
//!
//!   * `OPTIONS` — public method advertisement.
//!   * `ANNOUNCE` — SDP body, ignored beyond a 200 OK.
//!   * `SETUP` — captures the requested transport and mints a
//!     session id.
//!   * `RECORD` / `PLAY` — flips the connection into the
//!     interleaved-frame mode if the client requested
//!     `RTP/AVP/TCP;interleaved=...` in `SETUP`.
//!   * `TEARDOWN` — closes the session.
//!
//! Interleaved RTP frames (RFC 2326 §10.12) arrive on the same TCP
//! connection prefixed with `$<channel><len>`; the server captures
//! them as raw bytes into a shared `Vec<Vec<u8>>` so tests can
//! inspect what the client sent without decoding the RTP payload.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const READ_BUFFER_INITIAL_CAPACITY: usize = 4096;
const READ_CHUNK_SIZE: usize = 4096;
const MAX_HEADER_BYTES: usize = 8192;

#[derive(Debug, thiserror::Error)]
pub enum MockRtspError {
    #[error("failed to bind ephemeral port: {0}")]
    Bind(#[from] std::io::Error),
}

/// Snapshot of in-flight server state. Cloned into each connection
/// task so multiple concurrent sessions can record into the same
/// vectors.
#[derive(Default)]
struct SharedState {
    captured_packets: Mutex<Vec<Vec<u8>>>,
    session_count: AtomicUsize,
}

/// Handle to a running mock RTSP server.
pub struct MockRtspServer {
    port: u16,
    state: Arc<SharedState>,
    accept_task: Option<JoinHandle<()>>,
}

impl MockRtspServer {
    /// Bind on `127.0.0.1:0` and start accepting RTSP control
    /// connections.
    pub async fn start() -> Result<Self, MockRtspError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let state = Arc::new(SharedState::default());

        let task_state = Arc::clone(&state);
        let accept_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let conn_state = Arc::clone(&task_state);
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, conn_state).await {
                                tracing::debug!(?err, "mock rtsp connection ended with error");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::debug!(?err, "mock rtsp accept loop closed");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            port,
            state,
            accept_task: Some(accept_task),
        })
    }

    /// `rtsp://127.0.0.1:<port>/<stream>` for the named stream.
    pub fn url(&self, stream: &str) -> String {
        let trimmed = stream.trim_start_matches('/');
        format!("rtsp://127.0.0.1:{}/{}", self.port, trimmed)
    }

    /// Bound TCP port on `127.0.0.1`.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Returns a copy of every interleaved RTP packet captured so
    /// far across all sessions. The returned `Vec` is a snapshot;
    /// further packets land in the internal buffer.
    pub fn captured_rtp_packets(&self) -> Vec<Vec<u8>> {
        // The mutex is `std::sync::Mutex`, and writers never hold
        // it across an `await`, so a synchronous lock here is fine
        // even when called from inside a tokio runtime.
        let guard = self.state.captured_packets.lock().expect("mutex poisoned");
        guard.clone()
    }

    /// Number of RTSP sessions the server has minted on `SETUP`.
    /// Useful for reconnect tests where the client is expected to
    /// re-handshake after a transport drop.
    pub fn captured_session_count(&self) -> usize {
        self.state.session_count.load(Ordering::SeqCst)
    }

    /// Stop accepting new connections. In-flight connections wind
    /// down naturally as their TCP halves close.
    pub async fn shutdown(mut self) {
        if let Some(task) = self.accept_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for MockRtspServer {
    fn drop(&mut self) {
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
    }
}

#[derive(Debug, Default)]
struct ParsedRequest {
    method: String,
    uri: String,
    headers: HashMap<String, String>,
    body_len: usize,
}

async fn handle_connection(
    mut stream: TcpStream,
    state: Arc<SharedState>,
) -> Result<(), std::io::Error> {
    let mut buffer = BytesMut::with_capacity(READ_BUFFER_INITIAL_CAPACITY);
    let mut interleaved_active = false;

    loop {
        // First-byte decision: `$` means an interleaved binary
        // frame. Any other byte starts an RTSP text request.
        if buffer.is_empty() && !read_more(&mut stream, &mut buffer).await? {
            return Ok(());
        }

        if interleaved_active && buffer[0] == b'$' {
            if !try_consume_interleaved_frame(&mut stream, &mut buffer, &state).await? {
                // Need more bytes; loop again to read them.
                continue;
            }
            continue;
        }

        // Try to parse a complete RTSP request from the buffer.
        match parse_request(&buffer)? {
            None => {
                if buffer.len() > MAX_HEADER_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "rtsp header too large",
                    ));
                }
                if !read_more(&mut stream, &mut buffer).await? {
                    return Ok(());
                }
            }
            Some((req, header_len)) => {
                let total = header_len + req.body_len;
                if buffer.len() < total {
                    if !read_more(&mut stream, &mut buffer).await? {
                        return Ok(());
                    }
                    continue;
                }
                let _body = buffer.split_to(total).split_off(header_len);
                respond_to_request(&mut stream, &req, &state, &mut interleaved_active).await?;
            }
        }
    }
}

async fn read_more(
    stream: &mut TcpStream,
    buffer: &mut BytesMut,
) -> Result<bool, std::io::Error> {
    let mut chunk = vec![0u8; READ_CHUNK_SIZE];
    let n = stream.read(&mut chunk).await?;
    if n == 0 {
        return Ok(false);
    }
    buffer.extend_from_slice(&chunk[..n]);
    Ok(true)
}

/// Try to peel one interleaved frame off the buffer. Returns Ok(true)
/// if a frame was consumed, Ok(false) if more bytes are needed.
async fn try_consume_interleaved_frame(
    _stream: &mut TcpStream,
    buffer: &mut BytesMut,
    state: &SharedState,
) -> Result<bool, std::io::Error> {
    if buffer.len() < 4 {
        return Ok(false);
    }
    let len = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
    if buffer.len() < 4 + len {
        return Ok(false);
    }
    // Skip the 4-byte `$<channel><len>` header; capture the payload.
    let _ = buffer.split_to(4);
    let payload = buffer.split_to(len).to_vec();
    let mut guard = state.captured_packets.lock().expect("mutex poisoned");
    guard.push(payload);
    Ok(true)
}

/// Returns the parsed request and the byte length of the header
/// section (request line + headers + terminating CRLF CRLF). Returns
/// `None` if the request is incomplete.
fn parse_request(buffer: &[u8]) -> Result<Option<(ParsedRequest, usize)>, std::io::Error> {
    let header_end = match find_header_terminator(buffer) {
        Some(end) => end,
        None => return Ok(None),
    };
    let header_bytes = &buffer[..header_end];
    let header_text = std::str::from_utf8(header_bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rtsp header is not valid utf-8",
        )
    })?;

    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "request line missing method")
        })?
        .to_string();
    let uri = parts
        .next()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "request line missing uri")
        })?
        .to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(
                name.trim().to_ascii_lowercase(),
                value.trim().to_string(),
            );
        }
    }

    let body_len = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    Ok(Some((
        ParsedRequest {
            method,
            uri,
            headers,
            body_len,
        },
        header_end,
    )))
}

fn find_header_terminator(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

async fn respond_to_request(
    stream: &mut TcpStream,
    req: &ParsedRequest,
    state: &SharedState,
    interleaved_active: &mut bool,
) -> Result<(), std::io::Error> {
    let cseq = req.headers.get("cseq").cloned().unwrap_or_default();
    let session = req.headers.get("session").cloned();

    let method = req.method.to_ascii_uppercase();
    let body = String::new();
    let mut extra_headers: Vec<String> = Vec::new();

    match method.as_str() {
        "OPTIONS" => {
            extra_headers.push(
                "Public: OPTIONS, ANNOUNCE, SETUP, RECORD, PLAY, TEARDOWN, DESCRIBE".to_string(),
            );
        }
        "ANNOUNCE" => {
            // SDP body already drained by the caller; nothing to do.
        }
        "SETUP" => {
            let session_id = state.session_count.fetch_add(1, Ordering::SeqCst) + 1;
            extra_headers.push(format!("Session: {session_id:08x}"));
            if let Some(transport) = req.headers.get("transport") {
                extra_headers.push(format!("Transport: {transport}"));
                if transport.contains("interleaved") {
                    *interleaved_active = true;
                }
            }
        }
        "RECORD" | "PLAY" => {
            if let Some(s) = session {
                extra_headers.push(format!("Session: {s}"));
            }
        }
        "TEARDOWN" => {
            if let Some(s) = session {
                extra_headers.push(format!("Session: {s}"));
            }
            *interleaved_active = false;
        }
        _ => {
            // Unsupported verb. Reply with a 200 OK anyway so the
            // client does not abort the test setup; real servers
            // would 501. The fixture is permissive on purpose.
        }
    }

    let mut response = String::new();
    response.push_str("RTSP/1.0 200 OK\r\n");
    if !cseq.is_empty() {
        response.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    for header in &extra_headers {
        response.push_str(header);
        response.push_str("\r\n");
    }
    response.push_str("Server: ados-mock-rtsp/0.1\r\n");
    response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    response.push_str("\r\n");
    response.push_str(&body);

    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;

    // After RECORD with interleaved transport, the client expects
    // the server to keep the socket open and start sending frames
    // back; for capture-only fixtures we just wait for the client
    // to write its `$` frames. Mark `interleaved_active` so the
    // outer loop knows to switch to the binary path.
    if method == "RECORD" {
        *interleaved_active = true;
    }
    if method == "TEARDOWN" {
        *interleaved_active = false;
    }
    if uri_is_empty(&req.uri) {
        // Unreachable in practice; kept as a defensive guard.
    }

    Ok(())
}

fn uri_is_empty(uri: &str) -> bool {
    uri.is_empty()
}
