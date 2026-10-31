//! Broadcast server: accept loop, fan-out and client bookkeeping.

use super::*;

/// How long the accept loops back off after a failed `accept()` before
/// retrying. Long enough that a persistent error cannot hot-spin the reactor,
/// short enough that a transient one costs a client no perceptible delay.
/// Shared by [`IpcBroadcast`] and [`serve_rpc`] so the two accept loops cannot
/// drift into different recovery policies.
pub(crate) const ACCEPT_RETRY_BACKOFF: Duration = Duration::from_millis(200);

/// The next accepted connection's [`IpcPeer::conn`].
pub(super) static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// Kernel send buffer requested on every accepted broadcast client.
///
/// A stalled consumer is evicted once its kernel send buffer AND its
/// outbound queue are full. Unpinned, the kernel buffer is
/// `net.core.wmem_default`, which the installer raises to 4 MiB for video,
/// so a stalled MAVLink, MSP or state reader buffered megabytes of stale
/// frames before eviction. Linux doubles the request and caps it at
/// `wmem_max`, so the effective buffer is at most 512 KiB.
pub(super) const IPC_CLIENT_SEND_BUFFER_BYTES: usize = 256 * 1024;

/// Where an accept loop gets its client connections.
///
/// The real implementation is the bound [`UnixListener`]. It is a trait purely
/// so the accept-ERROR branch is reachable from a test: a live `accept()` only
/// fails under process-wide fd exhaustion or a kernel resource shortage, which
/// cannot be induced in-process without wrecking every other test sharing the
/// runtime. That is exactly why the branch which decides whether these sockets
/// survive an accept error carried no coverage, and why it was a `break`.
///
/// Written as `-> impl Future<..> + Send` rather than `async fn` on purpose, and
/// `clippy::manual_async_fn` is allowed for exactly that reason: the accept loop
/// runs under `tokio::spawn`, so the returned future MUST be `Send`, and an
/// `async fn` in a trait gives no way to say so. Simplifying to `async fn` makes
/// the spawn fail to compile.
pub(super) trait ClientSource: Send + 'static {
    #[allow(clippy::manual_async_fn)]
    fn next_client(&mut self) -> impl Future<Output = io::Result<UnixStream>> + Send;
}

impl ClientSource for OperatorListener {
    #[allow(clippy::manual_async_fn)]
    fn next_client(&mut self) -> impl Future<Output = io::Result<UnixStream>> + Send {
        async move {
            OperatorListener::accept(self)
                .await
                .map(|(stream, _addr)| stream)
        }
    }
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
        // Owner+group rw (0o660, group `ados-operator`), matching every other
        // command socket in the tree; the shared helper owns the create-dir /
        // remove-stale / bind / chmod / chgrp hygiene and the per-accept peer
        // check.
        //
        // Two of these sockets — the MAVLink and MSP lanes — accept inbound
        // bytes that are written straight through to the flight controller, so
        // a peer that can open one can arm an aircraft. The trusted-local plane
        // is root plus the operator group; plugins run as `ados`, outside it,
        // and reach the flight controller through the plugin host, which gates
        // every write on the plugin's granted capabilities.
        let listener = bind_command_socket(&path, 0o660)?;

        Ok(Self::from_source(
            path,
            listener,
            queue_depth,
            keep_last,
            inbound,
        ))
    }

    /// Start the server over an arbitrary [`ClientSource`].
    ///
    /// Split out of [`Self::bind`] so the accept loop's error-recovery branch is
    /// exercisable: the source is the only part of the loop a test cannot drive
    /// through a real socket.
    pub(super) fn from_source<S: ClientSource>(
        path: PathBuf,
        mut source: S,
        queue_depth: usize,
        keep_last: bool,
        inbound: Option<usize>,
    ) -> (Self, Option<mpsc::Receiver<InboundCommand>>) {
        let clients: Arc<Mutex<Vec<ClientHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let last: Arc<Mutex<Option<Bytes>>> = Arc::new(Mutex::new(None));
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
                let stream = match source.next_client().await {
                    Ok(s) => s,
                    Err(e) => {
                        // Never exit. This was `break`, which ended the accept
                        // task for the life of the process on ONE transient
                        // error — EMFILE/ENFILE under fd pressure, ENOBUFS,
                        // ECONNABORTED — while leaving the struct looking
                        // healthy: `client_count()` still reported the
                        // already-connected clients and `dropped_clients()`
                        // never ticked, so nothing anywhere could tell the
                        // socket had stopped accepting. Both the MAVLink and the
                        // state socket are bound through this one function, so a
                        // single hiccup silently closed the FC command surface
                        // and the telemetry snapshot to every future client
                        // until a manual restart.
                        //
                        // Back off so a persistent error cannot hot-spin, then
                        // retry forever — the same policy `serve_rpc` below
                        // already applies to the command sockets.
                        tracing::warn!(error = %e, "ipc broadcast socket accept failed");
                        tokio::time::sleep(ACCEPT_RETRY_BACKOFF).await;
                        continue;
                    }
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

        (
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
        )
    }
}

impl IpcBroadcast {
    /// Broadcast a byte buffer to all connected clients. Clients whose queue is
    /// full are dropped. If `keep_last` is set, the buffer is also stored as the
    /// last-state replayed to future clients.
    ///
    /// Takes [`Bytes`] rather than `Vec<u8>` because every clone below — one per
    /// attached client, plus one for the retained last-state — used to be a full
    /// copy of the buffer. On the FC frame lane that is ~66 copies/s per
    /// connected consumer, growing with the number of consumers; as `Bytes` each
    /// is a refcount bump and the cost stops scaling with the client count. A
    /// producer holding a freshly built `Vec<u8>` converts with `.into()`, which
    /// is a move, not a copy.
    pub async fn broadcast(&self, buf: Bytes) {
        if self.keep_last {
            *self.last.lock().await = Some(buf.clone());
        }
        let mut clients = self.clients.lock().await;
        prune_finished(&mut clients);
        let mut dropped = 0u64;
        clients.retain_mut(|client| match client.tx.try_send(buf.clone()) {
            Ok(()) => true,
            // Queue full: the consumer is genuinely not draining. Evict it
            // and count it — a slow consumer is a real fault, and this
            // counter is the only producer-visible signal that it happened.
            Err(mpsc::error::TrySendError::Full(_)) => {
                client.reader.abort();
                client.writer.abort();
                dropped += 1;
                false
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
                false
            }
        });
        // One relaxed add for the whole broadcast rather than per pruned
        // client; the owning service polls this and reports it.
        if dropped > 0 {
            self.dropped_clients.fetch_add(dropped, Ordering::Relaxed);
        }
    }

    /// Number of currently connected clients. Clients that already hung up are
    /// pruned first, so a socket that never broadcasts does not count (or hold
    /// the file descriptors of) every client that ever connected.
    pub async fn client_count(&self) -> usize {
        let mut clients = self.clients.lock().await;
        prune_finished(&mut clients);
        clients.len()
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

/// Drop the handles of clients that already hung up. The writer task ends once
/// the reader reports EOF, so a finished writer marks a gone client; its reader
/// is aborted in case it is still unwinding.
pub(super) fn prune_finished(clients: &mut Vec<ClientHandle>) {
    clients.retain(|c| {
        if c.writer.is_finished() {
            c.reader.abort();
            false
        } else {
            true
        }
    });
}
