//! Unix-socket IPC transport for the MAVLink and state sockets.
//!
//! Mirrors the semantics of `ADOSDroneAgent/src/ados/core/ipc.py`:
//!
//! - The owning service binds a Unix socket (perms `0o660`, group `ados`,
//!   stale socket removed first) and broadcasts byte buffers to every connected
//!   client. The trusted local plane is root plus that group and nothing wider:
//!   two of these sockets forward inbound bytes straight to the flight
//!   controller.
//! - Each client has a bounded outbound queue. A client whose queue fills is
//!   dropped rather than allowed to grow unbounded (slow-client policy).
//! - The state socket additionally replays the last buffer to a client the
//!   moment it connects, so a late subscriber does not wait for the next
//!   publish.
//! - The MAVLink socket is bidirectional: a frame written by a client is a
//!   command toward the flight controller, surfaced here on an inbound channel.
//!
//! Framing is the caller's concern. [`broadcast`](IpcBroadcast::broadcast)
//! sends the exact bytes given (the caller frames via [`crate::frame`] or
//! [`crate::state`]). The inbound reader, used only by the MAVLink socket,
//! decodes 4-byte big-endian length-prefixed frames.
//!
//! ## Who wrote this
//!
//! An inbound payload arrives as an [`InboundCommand`], which carries the
//! writing connection's [`IpcPeer`] alongside the bytes. The owning service
//! used to receive a bare buffer and had no way to tell one writer from
//! another, so it recorded a single hardcoded provenance for every one of
//! them. That reading was wrong for the writer that matters most: a process
//! that accepts bytes from off the node and forwards them here is not the same
//! as a process that produced them itself, and a service that cannot see the
//! difference reports remote traffic as locally originated.

use std::future::Future;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::frame;

/// The control payload a writer sends to declare that the bytes it is about to
/// forward did not originate on this node.
///
/// Sent as an ordinary length-prefixed frame, so no framing changes and no
/// existing writer is affected. It cannot collide with real traffic on either
/// socket that carries inbound bytes: a MAVLink frame starts `0xFD` (v2) or
/// `0xFE` (v1) and an MSP one starts `$`, while this starts with a NUL.
///
/// The declaration is one-way and downward. A writer can only ever give up the
/// benefit of the doubt with it, never claim one, so a writer that does not
/// send it is treated exactly as it was before and a writer that forges it only
/// declassifies itself. It is consumed by the reader and never forwarded on.
pub const IPC_DECLARE_OFF_BOX_SOURCE: &[u8] = b"\x00ados-ipc/source=off-box";

/// Who wrote an inbound payload, as the accepting socket can tell.
///
/// The credentials come from the kernel (`SO_PEERCRED` and its equivalents),
/// so they cannot be forged by the writer. `off_box_source` is the writer's own
/// declaration (see [`IPC_DECLARE_OFF_BOX_SOURCE`]) and is sticky for the life
/// of the connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IpcPeer {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub pid: Option<i32>,
    /// The writing process has declared that it is forwarding bytes that
    /// reached this node from somewhere else.
    pub off_box_source: bool,
}

/// One inbound payload plus the identity of the connection that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundCommand {
    pub payload: Vec<u8>,
    pub peer: IpcPeer,
}

/// Read the connecting process's credentials. A platform or kernel that will
/// not answer leaves the fields empty rather than inventing a value.
fn peer_identity(stream: &UnixStream) -> IpcPeer {
    match stream.peer_cred() {
        Ok(cred) => IpcPeer {
            uid: Some(cred.uid()),
            gid: Some(cred.gid()),
            pid: cred.pid(),
            off_box_source: false,
        },
        Err(_) => IpcPeer::default(),
    }
}

/// One connected client: its outbound queue plus the writer and reader tasks,
/// so both can be aborted when the client is pruned or the server is dropped.
struct ClientHandle {
    tx: mpsc::Sender<Vec<u8>>,
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
}

/// A Unix-socket broadcast server. Drops slow clients; optionally replays the
/// last buffer on connect; optionally decodes inbound length-prefixed frames.
pub struct IpcBroadcast {
    path: PathBuf,
    queue_depth: usize,
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    last: Arc<Mutex<Option<Vec<u8>>>>,
    keep_last: bool,
    accept_task: JoinHandle<()>,
    /// Monotonic count of clients evicted for falling behind — a full outbound
    /// queue, and only that. A consumer that simply disconnected is pruned
    /// without advancing this counter, so the number means "a consumer could
    /// not keep up" rather than "a client went away". A slow consumer is
    /// otherwise pruned silently; surfacing this counter lets the owning
    /// service report the eviction the same way the radio TX-liveness watchdog
    /// surfaces a stalled link — a silent drop is itself the failure mode to
    /// detect, and a counter that also ticks on normal disconnects would drown
    /// that signal.
    dropped_clients: Arc<AtomicU64>,
}

impl IpcBroadcast {
    /// Bind the socket and start accepting clients.
    ///
    /// - `queue_depth`: per-client outbound queue size (256 for MAVLink, 32 for
    ///   state, per the contracts).
    /// - `keep_last`: replay the last broadcast buffer to a newly connected
    ///   client (state socket behaviour).
    /// - `inbound`: if `Some`, each client connection is also read as a stream
    ///   of length-prefixed frames whose payloads are forwarded on the channel
    ///   as [`InboundCommand`]s carrying the writing connection's identity
    ///   (MAVLink command path). The capacity bounds the inbound backlog.
    pub async fn bind(
        path: impl AsRef<Path>,
        queue_depth: usize,
        keep_last: bool,
        inbound: Option<usize>,
    ) -> io::Result<(Self, Option<mpsc::Receiver<InboundCommand>>)> {
        let path = path.as_ref().to_path_buf();
        // Owner+group rw (0o660, group `ados`), matching every other command
        // socket in the tree; the shared helper owns the create-dir /
        // remove-stale / bind / chmod / chgrp hygiene.
        //
        // This was 0o666 to match the Python server. That is world-writable, and
        // two of these sockets — the MAVLink and MSP lanes — accept inbound bytes
        // that are written straight through to the flight controller, so any
        // local process could arm an aircraft. The trusted-local plane is
        // root plus the `ados` group; nothing legitimate needs more, because
        // every service runs as root and plugins reach the flight controller
        // through the plugin host rather than by opening these sockets.
        let listener = bind_command_socket(&path, 0o660)?;

        let clients: Arc<Mutex<Vec<ClientHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let last: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let dropped_clients = Arc::new(AtomicU64::new(0));

        let (inbound_tx, inbound_rx) = match inbound {
            Some(cap) => {
                let (tx, rx) = mpsc::channel::<InboundCommand>(cap);
                (Some(tx), Some(rx))
            }
            None => (None, None),
        };

        let accept_clients = clients.clone();
        let accept_last = last.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((s, _addr)) => s,
                    Err(_) => break,
                };
                Self::on_client(
                    stream,
                    queue_depth,
                    keep_last,
                    accept_clients.clone(),
                    accept_last.clone(),
                    inbound_tx.clone(),
                )
                .await;
            }
        });

        Ok((
            Self {
                path,
                queue_depth,
                clients,
                last,
                keep_last,
                accept_task,
                dropped_clients,
            },
            inbound_rx,
        ))
    }

    async fn on_client(
        stream: UnixStream,
        queue_depth: usize,
        keep_last: bool,
        clients: Arc<Mutex<Vec<ClientHandle>>>,
        last: Arc<Mutex<Option<Vec<u8>>>>,
        inbound_tx: Option<mpsc::Sender<InboundCommand>>,
    ) {
        let peer = peer_identity(&stream);
        let (mut read_half, mut write_half) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(queue_depth);

        // Replay the last buffer immediately for the state socket.
        if keep_last {
            if let Some(buf) = last.lock().await.clone() {
                let _ = tx.try_send(buf);
            }
        }

        // Writer task: drain the queue to the socket, await flush so kernel
        // backpressure stays inside the task and never blocks the producer.
        let writer = tokio::spawn(async move {
            while let Some(buf) = rx.recv().await {
                if write_half.write_all(&buf).await.is_err() {
                    break;
                }
                if write_half.flush().await.is_err() {
                    break;
                }
            }
        });

        // Reader task. The MAVLink socket (inbound channel present) decodes
        // length-prefixed frames and forwards the payloads as commands toward
        // the flight controller. The write-only state socket has no inbound
        // protocol, so it just drains raw bytes to detect EOF, matching the
        // Python state server which only waits for the client to disconnect.
        let reader = tokio::spawn(async move {
            match inbound_tx {
                Some(tx) => {
                    let mut peer = peer;
                    loop {
                        let mut header = [0u8; frame::HEADER_SIZE];
                        if read_half.read_exact(&mut header).await.is_err() {
                            break;
                        }
                        let len = match frame::decode_len(header, frame::MAVLINK_MAX_FRAME, false) {
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        let mut payload = vec![0u8; len];
                        if len > 0 && read_half.read_exact(&mut payload).await.is_err() {
                            break;
                        }
                        // The origin declaration is a control frame, not
                        // traffic: it latches for the rest of the connection
                        // and is not passed on to the flight controller.
                        if payload == IPC_DECLARE_OFF_BOX_SOURCE {
                            peer.off_box_source = true;
                            continue;
                        }
                        if tx.send(InboundCommand { payload, peer }).await.is_err() {
                            break;
                        }
                    }
                }
                None => {
                    let mut scratch = [0u8; 256];
                    while read_half.read(&mut scratch).await.unwrap_or(0) > 0 {}
                }
            }
        });

        clients
            .lock()
            .await
            .push(ClientHandle { tx, writer, reader });
    }

    /// Broadcast a byte buffer to all connected clients. Clients whose queue is
    /// full are dropped. If `keep_last` is set, the buffer is also stored as the
    /// last-state replayed to future clients.
    pub async fn broadcast(&self, buf: Vec<u8>) {
        if self.keep_last {
            *self.last.lock().await = Some(buf.clone());
        }
        let mut clients = self.clients.lock().await;
        let mut keep = Vec::with_capacity(clients.len());
        let mut dropped = 0u64;
        for client in clients.drain(..) {
            match client.tx.try_send(buf.clone()) {
                Ok(()) => keep.push(client),
                // Queue full: the consumer is genuinely not draining. Evict it
                // and count it — a slow consumer is a real fault, and this
                // counter is the only producer-visible signal that it happened.
                Err(mpsc::error::TrySendError::Full(_)) => {
                    client.reader.abort();
                    client.writer.abort();
                    dropped += 1;
                }
                // Channel closed: the consumer already went away. That is a
                // normal disconnect, NOT a slow client, so it is pruned without
                // touching the eviction counter. Counting it reported every
                // short-lived reader as a fault: `ados-cloud` opens a fresh
                // connection per enrichment tick (~1 Hz), reads the replayed
                // last-state and closes, which alone logged ~86k spurious
                // `ipc_slow_client_evicted` warnings a day and buried the real
                // evictions in the noise.
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    client.reader.abort();
                    client.writer.abort();
                }
            }
        }
        *clients = keep;
        // One relaxed add for the whole broadcast rather than per pruned
        // client; the owning service polls this and reports it.
        if dropped > 0 {
            self.dropped_clients.fetch_add(dropped, Ordering::Relaxed);
        }
    }

    /// Number of currently connected clients.
    pub async fn client_count(&self) -> usize {
        self.clients.lock().await.len()
    }

    /// The per-client queue depth this server was bound with.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }

    /// Monotonic count of clients evicted for falling behind — a full outbound
    /// queue — since this server was bound. A client that disconnected cleanly
    /// is not counted here.
    ///
    /// A slow consumer is pruned without an error reaching it, so this counter
    /// is the producer-visible signal that an eviction happened. The owning
    /// service folds it into the state snapshot it already publishes so the
    /// eviction is observable remotely rather than silent. Lock-free.
    pub fn dropped_clients(&self) -> u64 {
        self.dropped_clients.load(Ordering::Relaxed)
    }
}

impl Drop for IpcBroadcast {
    fn drop(&mut self) {
        self.accept_task.abort();
        // Abort the per-client reader/writer tasks so none survive the server.
        if let Ok(clients) = self.clients.try_lock() {
            for client in clients.iter() {
                client.reader.abort();
                client.writer.abort();
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind a Unix command socket with the standard hygiene every accept loop
/// needs: create the parent directory, remove a stale socket left by a prior
/// run (so `bind` cannot fail with `EADDRINUSE`), bind, and set the file mode.
/// Returns the bound listener.
///
/// This is the create-dir / remove-stale / bind / chmod sequence that every
/// owning service duplicates. [`IpcBroadcast::bind`] and the one-shot command
/// sockets share it so the hygiene lives in one place.
///
/// Like [`UnixListener::bind`], this must be called from within a Tokio runtime
/// context.
pub fn bind_command_socket(path: impl AsRef<Path>, mode: u32) -> io::Result<UnixListener> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Remove a stale socket so bind does not fail with EADDRINUSE.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    set_ados_group(path);
    Ok(listener)
}

/// Give a freshly-bound socket to the `ados` group.
///
/// This belongs next to the mode, not in each caller, because the two are one
/// decision: every service here runs as root, so a `0o660` socket grants
/// *nobody but root* until the group owns the file. A caller that sets the mode
/// and forgets the chgrp has not written a group-readable socket, it has
/// written a root-only one — and the failure is silent, because root can still
/// reach it, so it only shows up as a non-root client that mysteriously cannot
/// connect.
///
/// Best-effort by design: the installer creates the group, and on a dev host
/// where it is absent this is a quiet no-op so bring-up stays automatic
/// (Rule 26). Linux-only; a stub elsewhere. Mirrors the `ados-control` and
/// `ados-gpio` helpers this generalizes.
#[cfg(target_os = "linux")]
fn set_ados_group(path: &Path) {
    match nix::unistd::Group::from_name("ados") {
        Ok(Some(g)) => {
            if let Err(err) = nix::unistd::chown(path, None, Some(g.gid)) {
                tracing::debug!(
                    error = %err,
                    path = %path.display(),
                    "chgrp command socket to ados failed"
                );
            }
        }
        Ok(None) => tracing::debug!("ados group not present; leaving socket group as-is"),
        Err(err) => tracing::debug!(error = %err, "resolving ados group failed"),
    }
}

#[cfg(not(target_os = "linux"))]
fn set_ados_group(_path: &Path) {}

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
pub async fn serve_rpc<H, Fut>(listener: UnixListener, max_request: usize, handler: H)
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
                tokio::time::sleep(Duration::from_millis(200)).await;
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

/// Connect to a Unix socket with bounded retry (mirrors the Python client's
/// connect-with-retry). Returns the stream once connected.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{encode_frame, MAVLINK_MAX_FRAME};

    fn temp_sock(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let unique = format!("ados-ipc-test-{}-{}.sock", std::process::id(), name);
        p.push(unique);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[tokio::test]
    async fn broadcasts_framed_payloads_to_all_clients() {
        let path = temp_sock("bcast");
        let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

        let mut c1 = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        let mut c2 = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        // Let the accept loop register both clients.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(server.client_count().await, 2);

        server
            .broadcast(encode_frame(b"hello", MAVLINK_MAX_FRAME).unwrap())
            .await;

        let f1 = read_length_prefixed(&mut c1, MAVLINK_MAX_FRAME, false)
            .await
            .unwrap();
        let f2 = read_length_prefixed(&mut c2, MAVLINK_MAX_FRAME, false)
            .await
            .unwrap();
        assert_eq!(f1.as_deref(), Some(&b"hello"[..]));
        assert_eq!(f2.as_deref(), Some(&b"hello"[..]));
    }

    #[tokio::test]
    async fn replays_last_state_on_connect() {
        let path = temp_sock("laststate");
        let (server, _inbound) = IpcBroadcast::bind(&path, 32, true, None).await.unwrap();

        // Publish before any client connects.
        server.broadcast(b"{\"armed\":false}\n".to_vec()).await;

        let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        let line = read_newline_line(&mut client, crate::state::STATE_V2_MAX_FRAME)
            .await
            .unwrap();
        assert_eq!(line.as_deref(), Some(&b"{\"armed\":false}"[..]));
    }

    #[tokio::test]
    async fn slow_client_is_dropped_fast_client_survives() {
        let path = temp_sock("slow");
        // Small queue so a non-draining client fills it once the kernel socket
        // buffer is also full.
        let (server, _inbound) = IpcBroadcast::bind(&path, 2, false, None).await.unwrap();

        // "slow" connects but never reads; "fast" connects and drains continuously.
        let _slow = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        let mut fast = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        let fast_reader = tokio::spawn(async move {
            while let Ok(Some(_)) = read_length_prefixed(&mut fast, MAVLINK_MAX_FRAME, false).await
            {
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(server.client_count().await, 2);

        // Large payloads fill the slow client's kernel send buffer; its writer
        // then blocks, its queue fills to depth 2, and the next broadcast drops
        // it. The fast client keeps draining so it never fills.
        let big = vec![0xABu8; 60_000];
        for _ in 0..60u32 {
            server
                .broadcast(encode_frame(&big, MAVLINK_MAX_FRAME).unwrap())
                .await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The slow client has been pruned; the fast client remains.
        assert_eq!(server.client_count().await, 1);
        // The eviction is observable: the drop counter advanced by exactly the
        // one stalled consumer, not the fast one that kept draining.
        assert_eq!(server.dropped_clients(), 1);
        fast_reader.abort();
    }

    #[tokio::test]
    async fn dropped_client_counter_advances_when_a_consumer_stalls() {
        let path = temp_sock("dropcount");
        // Depth-2 queue so a non-draining client is evicted quickly once its
        // kernel send buffer and then its queue fill.
        let (server, _inbound) = IpcBroadcast::bind(&path, 2, false, None).await.unwrap();

        // A counter starts at zero before any client exists.
        assert_eq!(server.dropped_clients(), 0);

        // One consumer connects and then never reads a single byte.
        let _stalled = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(server.client_count().await, 1);

        // Push large payloads until the stalled consumer's kernel buffer and
        // queue fill and the next broadcast evicts it.
        let big = vec![0x5Au8; 60_000];
        for _ in 0..60u32 {
            server
                .broadcast(encode_frame(&big, MAVLINK_MAX_FRAME).unwrap())
                .await;
            tokio::time::sleep(Duration::from_millis(2)).await;
            if server.client_count().await == 0 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The stalled consumer was pruned, and the eviction is no longer
        // silent: the producer-visible counter advanced.
        assert_eq!(server.client_count().await, 0);
        assert_eq!(server.dropped_clients(), 1);

        // The counter is monotonic: it does not reset across further broadcasts
        // once the client is gone (no remaining client to drop).
        server
            .broadcast(encode_frame(b"tail", MAVLINK_MAX_FRAME).unwrap())
            .await;
        assert_eq!(server.dropped_clients(), 1);
    }

    #[tokio::test]
    async fn a_clean_disconnect_is_not_counted_as_a_slow_client() {
        // The state-socket shape: replay the last buffer on connect, which is
        // what a short-lived reader comes for.
        let path = temp_sock("cleandisconnect");
        let (server, _inbound) = IpcBroadcast::bind(&path, 32, true, None).await.unwrap();
        server
            .broadcast(encode_frame(b"snapshot", MAVLINK_MAX_FRAME).unwrap())
            .await;

        // This is exactly the `ados-cloud` enrichment pattern: connect, take the
        // replayed snapshot, disconnect. Repeated at ~1 Hz it used to log an
        // `ipc_slow_client_evicted` every second on a perfectly healthy agent.
        for _ in 0..3u32 {
            let client = connect_with_retry(&path, 10, Duration::from_millis(20))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(client);
            tokio::time::sleep(Duration::from_millis(30)).await;

            // Two broadcasts: the first may still find the queue writable
            // before the writer task notices the peer is gone, the second is
            // guaranteed to meet a closed channel.
            for _ in 0..2u32 {
                server
                    .broadcast(encode_frame(b"tick", MAVLINK_MAX_FRAME).unwrap())
                    .await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        // The departed clients are pruned...
        assert_eq!(server.client_count().await, 0);
        // ...but none of them was slow, so the eviction counter must be
        // untouched. Before the Full/Closed split this read 3.
        assert_eq!(server.dropped_clients(), 0);
    }

    #[tokio::test]
    async fn inbound_frames_from_client_reach_the_channel() {
        let path = temp_sock("inbound");
        let (_server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
            .await
            .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");

        let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        // Client writes a framed command toward the FC.
        let framed = encode_frame(b"command", MAVLINK_MAX_FRAME).unwrap();
        client.write_all(&framed).await.unwrap();
        client.flush().await.unwrap();

        let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .expect("a command");
        assert_eq!(got.payload, b"command");
        // The kernel answers for a local peer, so the writing process is
        // identified rather than assumed.
        assert!(
            got.peer.uid.is_some(),
            "the accepting socket must record who wrote the payload"
        );
        assert!(!got.peer.off_box_source);
    }

    #[tokio::test]
    async fn a_declared_off_box_source_sticks_and_is_not_forwarded() {
        let path = temp_sock("offbox");
        let (_server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
            .await
            .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");

        let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        for payload in [IPC_DECLARE_OFF_BOX_SOURCE, b"first", b"second"] {
            let framed = encode_frame(payload, MAVLINK_MAX_FRAME).unwrap();
            client.write_all(&framed).await.unwrap();
        }
        client.flush().await.unwrap();

        // The declaration itself never reaches the consumer, and both payloads
        // behind it are marked for the whole life of the connection.
        for expected in [&b"first"[..], &b"second"[..]] {
            let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
                .await
                .unwrap()
                .expect("a command");
            assert_eq!(got.payload, expected);
            assert!(
                got.peer.off_box_source,
                "a declared forwarder must not read as a local producer"
            );
        }
    }

    #[tokio::test]
    async fn one_connection_declaring_does_not_mark_another() {
        let path = temp_sock("offbox-scope");
        let (_server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
            .await
            .unwrap();
        let mut inbound = inbound.expect("inbound channel requested");

        let mut declaring = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        declaring
            .write_all(&encode_frame(IPC_DECLARE_OFF_BOX_SOURCE, MAVLINK_MAX_FRAME).unwrap())
            .await
            .unwrap();
        declaring.flush().await.unwrap();

        let mut plain = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        plain
            .write_all(&encode_frame(b"local", MAVLINK_MAX_FRAME).unwrap())
            .await
            .unwrap();
        plain.flush().await.unwrap();

        let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .expect("a command");
        assert_eq!(got.payload, b"local");
        assert!(!got.peer.off_box_source);
    }

    #[tokio::test]
    async fn read_newline_line_caps_an_endless_line() {
        // A peer that never sends a newline must not grow the buffer past the
        // cap: the reader returns InvalidData instead of allocating unbounded.
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let writer = tokio::spawn(async move {
            // Send more bytes than the cap, with no newline.
            let chunk = vec![b'x'; 64];
            for _ in 0..10 {
                if a.write_all(&chunk).await.is_err() {
                    break;
                }
                let _ = a.flush().await;
            }
            // Keep the stream open so the reader hits the cap, not EOF.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let err = read_newline_line(&mut b, 128).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        writer.abort();
    }

    #[tokio::test]
    async fn bind_command_socket_removes_stale_and_sets_mode() {
        let path = temp_sock("bindhelper");
        // Leave a stale regular file where the socket will go; bind must remove
        // it rather than fail with EADDRINUSE.
        std::fs::write(&path, b"stale").unwrap();
        assert!(path.exists());

        let listener = bind_command_socket(&path, 0o660).unwrap();

        // The requested permission bits are set on the bound socket.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o660);

        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_broadcast_socket_is_never_world_accessible() {
        // Regression guard. These sockets were bound 0o666 to match an older
        // Python server, and two of them — the MAVLink and MSP lanes — forward
        // inbound bytes straight to the flight controller, so world-write meant
        // any local process could arm an aircraft. The trusted local plane is
        // root plus the `ados` group and nothing wider.
        let path = temp_sock("bcast-perm");
        let (server, _inbound) = IpcBroadcast::bind(&path, 8, false, None).await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o007,
            0,
            "broadcast socket must carry no world bits, got {:o}",
            mode & 0o777
        );
        assert_eq!(mode & 0o777, 0o660, "expected owner+group rw only");

        drop(server);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn serve_rpc_round_trips_a_request_and_closes_over_cap() {
        let path = temp_sock("rpc");
        let listener = bind_command_socket(&path, 0o660).unwrap();
        // Echo handler: prefix the request so the test sees the request bytes
        // (trailing newline stripped) reached the dispatch closure.
        let server = tokio::spawn(serve_rpc(listener, 16, |req: Vec<u8>| async move {
            let mut resp = b"echo:".to_vec();
            resp.extend_from_slice(&req);
            resp
        }));

        // A well-formed one-shot request gets one newline-terminated response.
        let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        client.write_all(b"ping\n").await.unwrap();
        client.flush().await.unwrap();
        let line = read_newline_line(&mut client, 256).await.unwrap();
        assert_eq!(line.as_deref(), Some(&b"echo:ping"[..]));
        drop(client);

        // A request longer than the cap (16) with no newline closes the
        // connection with no response. The client then reads no reply: on
        // some platforms that is a clean EOF (`Ok(None)`), on others the
        // server closing with unread data resets the connection (`Err`).
        // Both mean "closed without a reply".
        let mut over = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        let _ = over.write_all(&[b'x'; 64]).await;
        let _ = over.flush().await;
        let outcome = read_newline_line(&mut over, 256).await;
        assert!(
            matches!(outcome, Ok(None) | Err(_)),
            "over-cap request must get no reply, got {outcome:?}"
        );

        server.abort();
        let _ = std::fs::remove_file(&path);
    }
}
