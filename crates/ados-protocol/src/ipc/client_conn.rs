//! Per-client connection handling: peer identity, reader and writer tasks.

use super::*;

/// Read the connecting process's credentials. A platform or kernel that will
/// not answer leaves the fields empty rather than inventing a value.
pub(super) fn peer_identity(stream: &UnixStream) -> IpcPeer {
    let Ok(cred) = stream.peer_cred() else {
        return IpcPeer::default();
    };
    #[allow(unused_mut)]
    let mut gids = vec![cred.gid()];
    #[cfg(target_os = "linux")]
    gids.extend(peer_supplementary_groups(stream));
    let plugin = is_plugin_peer(
        cred.uid(),
        &gids,
        user_uid(LEGACY_PLUGIN_USER),
        group_gid(PLUGIN_GROUP),
    );
    IpcPeer {
        uid: Some(cred.uid()),
        gid: Some(cred.gid()),
        pid: cred.pid(),
        off_box_source: false,
        injector: plugin.then(|| plugin_plane_claim(cred.uid())),
        conn: 0,
    }
}

pub(super) fn user_uid(name: &str) -> Option<u32> {
    match nix::unistd::User::from_name(name) {
        Ok(user) => user.map(|u| u.uid.as_raw()),
        Err(err) => {
            tracing::debug!(error = %err, user = name, "resolving plugin user failed");
            None
        }
    }
}

impl IpcBroadcast {
    pub(super) async fn on_client(
        stream: UnixStream,
        queue_depth: usize,
        keep_last: bool,
        clients: Arc<Mutex<Vec<ClientHandle>>>,
        last: Arc<Mutex<Option<Bytes>>>,
        inbound_tx: Option<mpsc::Sender<InboundCommand>>,
    ) {
        pin_send_buffer(&stream);
        let mut peer = peer_identity(&stream);
        peer.conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
        let (mut read_half, mut write_half) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Bytes>(queue_depth);

        // Replay the last buffer immediately for the state socket.
        if keep_last {
            if let Some(buf) = last.lock().await.clone() {
                let _ = tx.try_send(buf);
            }
        }

        // Fired by the reader when the client hangs up (EOF or a read error), so
        // the writer ends too instead of idling on a socket that never
        // broadcasts, and the handle can be pruned without waiting for one.
        let (done_tx, mut done_rx) = oneshot::channel::<()>();

        // Writer task: drain the queue to the socket, await flush so kernel
        // backpressure stays inside the task and never blocks the producer.
        let writer = tokio::spawn(async move {
            loop {
                let buf = tokio::select! {
                    biased;
                    _ = &mut done_rx => break,
                    next = rx.recv() => match next {
                        Some(buf) => buf,
                        None => break,
                    },
                };
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
            let _done = done_tx;
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
                        if let Some(claim) = parse_injector_declaration(&payload) {
                            // Sticky, one-way: a writer subjects itself to the PIC
                            // arbiter. Never forwarded to the FC.
                            peer.injector = Some(claim);
                            continue;
                        }
                        if tx
                            .send(InboundCommand {
                                payload,
                                peer: peer.clone(),
                            })
                            .await
                            .is_err()
                        {
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

        let mut clients = clients.lock().await;
        prune_finished(&mut clients);
        clients.push(ClientHandle { tx, writer, reader });
    }
}

/// Pin the kernel send buffer of an accepted broadcast client to
/// [`IPC_CLIENT_SEND_BUFFER_BYTES`], so how much a stalled reader can hold
/// before eviction does not follow the system-wide `wmem_default`. A failure
/// leaves the system default in place; the client is still served.
pub(super) fn pin_send_buffer(stream: &UnixStream) {
    use nix::libc;
    use std::os::fd::AsRawFd;

    let size = IPC_CLIENT_SEND_BUFFER_BYTES as libc::c_int;
    // SAFETY: the pointer refers to a live `c_int` and the length passed is
    // exactly its size; the kernel only reads from it.
    let rc = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            std::ptr::from_ref(&size).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        tracing::warn!(
            error = %io::Error::last_os_error(),
            requested = IPC_CLIENT_SEND_BUFFER_BYTES,
            "ipc_client_sndbuf_set_failed"
        );
    }
}
