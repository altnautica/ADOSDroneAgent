//! Unix-socket IPC transport for the MAVLink and state sockets.
//!
//! Mirrors the semantics of `ADOSDroneAgent/src/ados/core/ipc.py`:
//!
//! - The owning service binds a Unix socket (perms `0o666`, stale socket
//!   removed first) and broadcasts byte buffers to every connected client.
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

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::frame;

/// A Unix-socket broadcast server. Drops slow clients; optionally replays the
/// last buffer on connect; optionally decodes inbound length-prefixed frames.
pub struct IpcBroadcast {
    path: PathBuf,
    queue_depth: usize,
    clients: Arc<Mutex<Vec<mpsc::Sender<Vec<u8>>>>>,
    last: Arc<Mutex<Option<Vec<u8>>>>,
    keep_last: bool,
    accept_task: JoinHandle<()>,
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
    ///   (MAVLink command path). The capacity bounds the inbound backlog.
    pub async fn bind(
        path: impl AsRef<Path>,
        queue_depth: usize,
        keep_last: bool,
        inbound: Option<usize>,
    ) -> io::Result<(Self, Option<mpsc::Receiver<Vec<u8>>>)> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // Remove a stale socket so bind does not fail with EADDRINUSE.
        let _ = std::fs::remove_file(&path);

        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;

        let clients: Arc<Mutex<Vec<mpsc::Sender<Vec<u8>>>>> = Arc::new(Mutex::new(Vec::new()));
        let last: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));

        let (inbound_tx, inbound_rx) = match inbound {
            Some(cap) => {
                let (tx, rx) = mpsc::channel::<Vec<u8>>(cap);
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
            },
            inbound_rx,
        ))
    }

    async fn on_client(
        stream: UnixStream,
        queue_depth: usize,
        keep_last: bool,
        clients: Arc<Mutex<Vec<mpsc::Sender<Vec<u8>>>>>,
        last: Arc<Mutex<Option<Vec<u8>>>>,
        inbound_tx: Option<mpsc::Sender<Vec<u8>>>,
    ) {
        let (mut read_half, mut write_half) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(queue_depth);

        // Replay the last buffer immediately for the state socket.
        if keep_last {
            if let Some(buf) = last.lock().await.clone() {
                let _ = tx.try_send(buf);
            }
        }

        clients.lock().await.push(tx);

        // Writer task: drain the queue to the socket, await flush so kernel
        // backpressure stays inside the task and never blocks the producer.
        tokio::spawn(async move {
            while let Some(buf) = rx.recv().await {
                if write_half.write_all(&buf).await.is_err() {
                    break;
                }
                if write_half.flush().await.is_err() {
                    break;
                }
            }
        });

        // Reader task (MAVLink command path): decode length-prefixed frames and
        // forward the payloads. For a write-only socket (state) we still drain
        // the read half to detect disconnect, but discard the bytes.
        tokio::spawn(async move {
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
                if let Some(ref tx) = inbound_tx {
                    if tx.send(payload).await.is_err() {
                        break;
                    }
                }
            }
        });
    }

    /// Broadcast a byte buffer to all connected clients. Clients whose queue is
    /// full are dropped. If `keep_last` is set, the buffer is also stored as the
    /// last-state replayed to future clients.
    pub async fn broadcast(&self, buf: Vec<u8>) {
        if self.keep_last {
            *self.last.lock().await = Some(buf.clone());
        }
        let mut clients = self.clients.lock().await;
        clients.retain(|tx| match tx.try_send(buf.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => false, // slow client: drop
            Err(mpsc::error::TrySendError::Closed(_)) => false, // gone: prune
        });
    }

    /// Number of currently connected clients.
    pub async fn client_count(&self) -> usize {
        self.clients.lock().await.len()
    }

    /// The per-client queue depth this server was bound with.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }
}

impl Drop for IpcBroadcast {
    fn drop(&mut self) {
        self.accept_task.abort();
        let _ = std::fs::remove_file(&self.path);
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
pub async fn read_newline_line<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read_exact(&mut byte).await {
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(Some(buf));
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
        let line = read_newline_line(&mut client).await.unwrap();
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
        fast_reader.abort();
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
            .unwrap();
        assert_eq!(got.as_deref(), Some(&b"command"[..]));
    }
}
