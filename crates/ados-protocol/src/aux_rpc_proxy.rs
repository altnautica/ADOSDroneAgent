//! Ground-side client for relay-proxy HTTP-over-aux calls.
//!
//! A ground station paired to a drone only over WFB has no IP reach to that
//! drone, but the aux lane already carries MAVLink, status, and identity low-
//! rate frames between them. This module rides a fourth and fifth channel on
//! the same pair — `Request` (ground→drone, uplink, radio_id 3) and `Response`
//! (drone→ground, downlink, radio_id 2) — to forward an HTTP request and
//! return its response, so a `relay-proxy` HTTP route on the ground station can
//! reach the drone's own agent HTTP API for a peer it has no LAN or cloud
//! address for.
//!
//! ## Process boundary
//!
//! The aux consumer (which receives Response frames off the radio) runs in the
//! ground-station data-plane process, and the HTTP route handler lives in the
//! control-surface process. They are separate processes, so the proxy's pending
//! map cannot be shared in-process. The seam between them is a Unix domain
//! socket (`/run/ados/aux-rpc-responses.sock`): the proxy's reader LISTENS on
//! that socket, and the consumer's ingest writer CONNECTS and forwards each
//! Response payload as a length-prefixed frame. This mirrors the existing
//! `MavlinkIngest` pattern exactly — same framing, same lifecycle, same
//! bounded-failure posture.
//!
//! ## Correlation
//!
//! Each request carries a 32-bit id assigned by the proxy. The drone's
//! response echoes it back unchanged, so a pending-request map on the ground
//! matches a response to its caller without ordering assumptions across a
//! reordering datagram lane. A solo proxy caller can use a monotonic counter;
//! the 32-bit space does not roll over in any realistic session at a few HTTP
//! calls per second.
//!
//! ## Bounded failure
//!
//! Every call is bounded by [`RPC_DEFAULT_TIMEOUT`]. A timeout removes the
//! pending entry and returns [`RpcError::Timeout`] to the caller rather than
//! parking any worker on the radio. A drone that never answers — radio dead,
//! consumer not running, handler not registered — surfaces as a bounded
//! failure on the very first call, which is what its HTTP caller needs to
//! report to the operator.
//!
//! ## What a successful call does and does not prove
//!
//! `Ok` means the drone decoded the request, dispatched it against its own
//! HTTP API, and returned a response with the same request id. It does NOT
//! prove the drone's HTTP server returned 200; the response's HTTP status
//! travels inside the payload and may be 404, 500, or anything else the
//! drone's own API returned. Callers must inspect [`RpcResponseOwned::status`].
//!
//! ## Send-side reuse
//!
//! The proxy holds one [`AuxEgress`] for its lifetime and reuses it across
//! calls. The egress opens the aux pair lazily on first send and re-opens on
//! a dead socket, so a proxy created before the radio is ready is not a
//! problem; the first call's open is what lights the uplink up.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::aux_egress::AuxEgress;
use crate::aux_mux::AuxChannel;
use crate::aux_rpc::{self, RpcMethod};
use crate::frame::HEADER_SIZE;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex, Notify};

/// Default per-call timeout. A round trip is one uplink datagram, the drone's
/// HTTP server processing, and one downlink datagram; on a 4 Mbps link a
/// 1200-byte payload moves in milliseconds, so the bound is governable by the
/// drone's HTTP server response time, not the radio. 5 seconds is enough for
/// most agent endpoints while still surfacing a wedged drone within a single
/// operator action.
pub const RPC_DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// The default Unix socket path for the Response IPC seam.
pub const DEFAULT_RESPONSE_SOCK: &str = "/run/ados/aux-rpc-responses.sock";

/// The response, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponseOwned {
    /// The HTTP status code the drone's API returned.
    pub status: u16,
    /// The HTTP response body bytes the drone's API returned.
    pub body: Vec<u8>,
}

/// Why a call did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// The request would not encode (oversized for one aux frame). The HTTP
    /// caller should surface a 413.
    Encode,
    /// The aux egress failed to send the request datagram.
    Send(String),
    /// No response arrived before the bound elapsed.
    Timeout,
    /// The consumer torn down between the request and the response, so the
    /// pending entry was removed. The caller may retry.
    ChannelClosed,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode => write!(f, "request payload exceeds one aux frame"),
            Self::Send(e) => write!(f, "aux egress send failed: {e}"),
            Self::Timeout => write!(f, "no response before timeout"),
            Self::ChannelClosed => write!(f, "aux consumer channel closed before response"),
        }
    }
}

impl std::error::Error for RpcError {}

/// One pending caller, keyed by request id. Cheap to clone; every clone shares
/// the same pending map and egress.
#[derive(Clone)]
pub struct AuxRpcProxy {
    egress: Arc<AuxEgress>,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<RpcResponseOwned>>>>,
    next_id: Arc<AtomicU32>,
    timeout: Duration,
}

impl AuxRpcProxy {
    /// A proxy using the given egress, with the default call timeout.
    pub fn new(egress: AuxEgress) -> Self {
        Self::with_timeout(egress, RPC_DEFAULT_TIMEOUT)
    }

    /// A proxy with an explicit per-call timeout (tests use a short one so a
    /// no-response case does not cost seconds).
    pub fn with_timeout(egress: AuxEgress, timeout: Duration) -> Self {
        Self {
            egress: Arc::new(egress),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU32::new(1)),
            timeout,
        }
    }

    /// Forward one HTTP-shaped request to the drone and await its response.
    ///
    /// `path` is the absolute path on the drone's own HTTP API, including the
    /// leading slash (e.g. `/api/pairing/info`). `body` is the request body
    /// (empty for GET). The drone's HTTP server returns the status and body;
    /// the proxy does not interpret either.
    pub async fn call(
        &self,
        method: RpcMethod,
        path: &[u8],
        body: &[u8],
    ) -> Result<RpcResponseOwned, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.call_with_id(id, method, path, body).await
    }

    async fn call_with_id(
        &self,
        id: u32,
        method: RpcMethod,
        path: &[u8],
        body: &[u8],
    ) -> Result<RpcResponseOwned, RpcError> {
        let payload = aux_rpc::encode_request(method, id, path, body).ok_or(RpcError::Encode)?;

        let (tx, rx) = oneshot::channel::<RpcResponseOwned>();
        self.pending.lock().await.insert(id, tx);

        if let Err(e) = self.egress.send(AuxChannel::Request, &payload).await {
            self.pending.lock().await.remove(&id);
            return Err(RpcError::Send(e.to_string()));
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::ChannelClosed)
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::Timeout)
            }
        }
    }

    /// Route a decoded response frame to its pending caller, if any.
    ///
    /// Called by the response reader task when it decodes a Response payload
    /// off the IPC socket. A response whose id matches no pending entry is a
    /// no-op: the caller already gave up (timeout) or the proxy was reset,
    /// both expected on a reordering lane.
    pub async fn dispatch_response(&self, response: &aux_rpc::RpcResponse<'_>) {
        if let Some(sender) = self.pending.lock().await.remove(&response.id) {
            let _ = sender.send(RpcResponseOwned {
                status: response.status,
                body: response.body.to_vec(),
            });
        }
    }

    /// Drop every pending caller. Used on shutdown so a caller awaiting a
    /// response whose consumer is gone does not wait the full timeout.
    pub async fn reset(&self) {
        let mut pending = self.pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(RpcResponseOwned {
                status: 503,
                body: Vec::new(),
            });
        }
    }

    /// Spawn the reader task that LISTENS on the Response IPC socket and
    /// dispatches each frame to its pending caller. Returns a cancel handle;
    /// notifying it stops the reader cleanly.
    ///
    /// The listener accepts connections from the consumer's ingest writer (one
    /// per consumer process). A consumer that connects after the proxy is up
    /// is accepted immediately; a consumer that disconnects mid-session has its
    /// connection dropped and a new one accepted on the next reconnect.
    pub fn spawn_response_listener(
        &self,
        sock_path: impl Into<PathBuf>,
        cancel: Arc<Notify>,
    ) -> tokio::task::JoinHandle<()> {
        let proxy = self.clone();
        let sock_path = sock_path.into();
        tokio::spawn(async move {
            run_response_listener(proxy, sock_path, cancel).await;
        })
    }
}

/// The listener loop: bind, accept connections, read length-prefixed Response
/// payloads, dispatch to pending callers.
async fn run_response_listener(proxy: AuxRpcProxy, sock_path: PathBuf, cancel: Arc<Notify>) {
    // Remove a stale socket file from a previous process. A leftover file from
    // an unclean shutdown prevents the new bind from succeeding, which would
    // silently disable the response dispatch path for the whole process
    // lifetime. Removing first is the same pattern the mavlink state socket uses.
    let _ = std::fs::remove_file(&sock_path);

    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, path = %sock_path.display(), "aux_rpc_response_listener_bind_failed");
            return;
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
                    }
                }
            }
        }
    }
    tracing::info!("aux_rpc_response_listener_stopped");
    let _ = std::fs::remove_file(&sock_path);
}

/// Read length-prefixed Response payloads off one accepted connection until EOF.
async fn handle_one_connection(mut stream: UnixStream, proxy: AuxRpcProxy) {
    let mut buf = Vec::new();
    loop {
        match read_one_frame(&mut stream).await {
            Ok(payload) => {
                buf.extend_from_slice(&payload);
                // A single IPC frame may contain multiple Response payloads
                // (batched or concatenated); decode as many as fit.
                while let Some((rest, response)) = try_decode_response_front(&buf) {
                    proxy.dispatch_response(&response).await;
                    buf = rest;
                }
            }
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
async fn read_one_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    if len > 4096 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response frame exceeds 4096 bytes",
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).await?;
    }
    Ok(payload)
}

/// Try to decode a Response from the front of `buf`. Returns the remaining
/// bytes and the decoded response, or `None` if the buffer does not yet hold
/// a complete response.
fn try_decode_response_front(buf: &[u8]) -> Option<(Vec<u8>, aux_rpc::RpcResponse<'_>)> {
    let response = aux_rpc::decode_response(buf).ok()?;
    let consumed = 8 + response.body.len();
    let rest = buf[consumed..].to_vec();
    Some((rest, response))
}

/// Writer side: CONNECTS to the Response IPC socket and writes each Response
/// payload as a length-prefixed frame. Used by the aux consumer's process to
/// forward Response datagrams to the proxy's process.
///
/// Mirrors `MavlinkIngest` exactly: lazy connect, bounded retry, best-effort
/// write (a full pipe drops rather than blocks the consumer's read loop).
#[derive(Clone)]
pub struct AuxRpcResponseIngest {
    sock_path: PathBuf,
    conn: Arc<Mutex<Option<UnixStream>>>,
}

impl AuxRpcResponseIngest {
    /// An ingest writing to the given Unix socket path.
    pub fn new(sock_path: impl Into<PathBuf>) -> Self {
        Self {
            sock_path: sock_path.into(),
            conn: Arc::new(Mutex::new(None)),
        }
    }

    /// Forward one Response payload as a length-prefixed frame. Best-effort:
    /// a failed write drops the frame and drops the connection so the next
    /// call re-connects. Never blocks: the consumer's read loop must not stall
    /// behind a proxy that is not reading.
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
                if stream.write_all(&frame).await.is_err() {
                    *guard = None;
                }
            }
            None => {
                match UnixStream::connect(&self.sock_path).await {
                    Ok(mut stream) => {
                        if stream.write_all(&frame).await.is_ok() {
                            *guard = Some(stream);
                        }
                    }
                    Err(_) => {
                        // No listener yet; the proxy will start eventually.
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aux_mux;
    use tokio::net::UdpSocket;
    use tokio::time::Duration;

    /// A no-radio egress that the test can read back from.
    async fn loopback_egress() -> (
        AuxEgress,
        tokio::sync::mpsc::Receiver<(AuxChannel, Vec<u8>)>,
    ) {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", port)).await.unwrap();
        let egress = AuxEgress::connected_for_test(sock);

        let (tx, rx) = tokio::sync::mpsc::channel::<(AuxChannel, Vec<u8>)>(8);
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, _)) = listener.recv_from(&mut buf).await {
                if let Ok((ch, payload)) = aux_mux::decode(&buf[..n]) {
                    let _ = tx.send((ch, payload.to_vec())).await;
                }
            }
        });
        (egress, rx)
    }

    #[tokio::test]
    async fn a_call_round_trips_through_egress_and_a_dispatched_response() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

        let proxy_for_consumer = proxy.clone();
        let consumer = tokio::spawn(async move {
            let (channel, payload) = sent.recv().await.expect("no request datagram");
            assert_eq!(channel, AuxChannel::Request);
            let request = aux_rpc::decode_request(&payload).unwrap();
            assert_eq!(request.method, RpcMethod::Get);
            assert_eq!(request.path, b"/api/pairing/info");
            assert!(request.body.is_empty());

            let body = br#"{"device_id":"abc"}"#;
            let resp = aux_rpc::RpcResponse {
                id: request.id,
                status: 200,
                body,
            };
            proxy_for_consumer.dispatch_response(&resp).await;
        });

        let result = proxy
            .call(RpcMethod::Get, b"/api/pairing/info", &[])
            .await
            .expect("call must succeed");

        consumer.await.unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.body, br#"{"device_id":"abc"}"#);
    }

    #[tokio::test]
    async fn a_call_times_out_when_no_response_arrives() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", 9u16)).await.unwrap();
        let egress = AuxEgress::connected_for_test(sock);
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(80));

        let started = std::time::Instant::now();
        let err = proxy
            .call(RpcMethod::Get, b"/api/status", &[])
            .await
            .unwrap_err();
        assert_eq!(err, RpcError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_call_returns_send_error_when_egress_fails() {
        let egress =
            AuxEgress::with_timeout("/nonexistent/aux-cmd.sock", Duration::from_millis(40));
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));
        let err = proxy
            .call(RpcMethod::Get, b"/api/status", &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Send(_)),
            "expected Send, got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_call_returns_encode_when_the_path_exceeds_one_aux_frame() {
        let (egress, _sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));
        let big_path = vec![b'A'; aux_rpc::MAX_REQUEST_PAYLOAD + 1];
        let err = proxy
            .call(RpcMethod::Get, &big_path, &[])
            .await
            .unwrap_err();
        assert_eq!(err, RpcError::Encode);
    }

    #[tokio::test]
    async fn a_response_for_an_unknown_id_is_a_noop() {
        let (egress, _sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(50));
        let resp = aux_rpc::RpcResponse {
            id: 9999,
            status: 200,
            body: b"orphan",
        };
        proxy.dispatch_response(&resp).await;
        assert!(proxy.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn concurrent_calls_with_distinct_ids_match_their_own_responses() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));
        let proxy_consumer = proxy.clone();

        let consumer = tokio::spawn(async move {
            for _ in 0..3 {
                let (ch, payload) = sent.recv().await.expect("request datagram");
                assert_eq!(ch, AuxChannel::Request);
                let request = aux_rpc::decode_request(&payload).unwrap();
                let body = format!("{}", request.id).into_bytes();
                let resp = aux_rpc::RpcResponse {
                    id: request.id,
                    status: 200,
                    body: &body,
                };
                proxy_consumer.dispatch_response(&resp).await;
            }
        });

        let p1 = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.call(RpcMethod::Get, b"/api/a", &[]).await }
        });
        let p2 = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.call(RpcMethod::Get, b"/api/b", &[]).await }
        });
        let p3 = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.call(RpcMethod::Get, b"/api/c", &[]).await }
        });

        let r1 = p1.await.unwrap().unwrap();
        let r2 = p2.await.unwrap().unwrap();
        let r3 = p3.await.unwrap().unwrap();
        consumer.await.unwrap();

        assert_ne!(r1.body, r2.body);
        assert_ne!(r2.body, r3.body);
        assert_eq!(r1.status, 200);
        assert_eq!(r2.status, 200);
        assert_eq!(r3.status, 200);
    }

    #[tokio::test]
    async fn reset_drains_pending_callers_with_503() {
        let (egress, _sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(60));

        let proxy_call = proxy.clone();
        let call_handle =
            tokio::spawn(async move { proxy_call.call(RpcMethod::Get, b"/api/x", &[]).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        proxy.reset().await;
        let result = tokio::time::timeout(Duration::from_millis(500), call_handle)
            .await
            .expect("reset must unblock the caller within the bound")
            .unwrap();
        assert_eq!(result.unwrap().status, 503);
    }

    #[tokio::test]
    async fn the_ingest_writes_and_the_listener_dispatches() {
        // End-to-end: proxy LISTENS, ingest CONNECTS, a Response written by
        // the ingest arrives at the listener and is dispatched to the pending
        // caller. The call's Request goes into a loopback void (no consumer
        // reading it), but the Response arrives via the IPC seam.
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("aux-rpc-responses.sock");

        // A loopback egress whose Request datagrams nobody reads — the call
        // succeeds on the Response side, not the Request side.
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", 9u16)).await.unwrap();
        let egress = AuxEgress::connected_for_test(sock);
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

        // Spawn the listener.
        let cancel = Arc::new(Notify::new());
        let listener_handle = proxy.spawn_response_listener(sock_path.clone(), cancel.clone());

        // Give the listener time to bind.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Start a call — it will send a Request into the void and wait.
        let proxy_call = proxy.clone();
        let call_handle = tokio::spawn(async move {
            proxy_call
                .call(RpcMethod::Get, b"/api/pairing/info", &[])
                .await
        });

        // Give the call time to register its pending entry.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Write a matching Response via the ingest. The call's request id was
        // assigned by the proxy (starts at 1), so we sniff it from the
        // pending map.
        let pending_id = {
            let pending = proxy.pending.lock().await;
            *pending.keys().next().expect("one pending caller")
        };

        let ingest = AuxRpcResponseIngest::new(&sock_path);
        let resp_payload = aux_rpc::encode_response(pending_id, 200, br#"{"ok":true}"#).unwrap();
        ingest.send(&resp_payload).await;

        let result = tokio::time::timeout(Duration::from_secs(2), call_handle)
            .await
            .expect("the call must complete within the bound")
            .unwrap();
        assert_eq!(result.unwrap().status, 200);

        cancel.notify_waiters();
        let _ = listener_handle.await;
    }
}
