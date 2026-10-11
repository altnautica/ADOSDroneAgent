//! `/ws/mesh`: the mesh event stream off the journal tail.

use super::*;

/// The `/ws/mesh` upgrade entry point. Resolves the handshake auth and the
/// profile gate, then fans the mesh-event journal + the pairing-event journal
/// into the one socket.
pub async fn ws_mesh(
    State(state): State<AppState>,
    headers: HeaderMap,
    caller: Option<Extension<CallerClass>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_MESH_EVENTS, caller_of(caller));
    let Some((ws, auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(move |socket| mesh_loop(socket, state, auth))
}

/// Fan the two journals into the socket: profile-gate after accept (so a
/// wrong-profile node closes 1008 the way the residual handler did), then follow
/// both `mesh-events.jsonl` and `pair-events.jsonl` and forward each well-formed
/// line verbatim. Each journal line already carries the
/// `{bus, kind, timestamp_ms, payload}` envelope the residual `ws_mesh_events`
/// sent (the mesh journal stamps `bus:"mesh"`, the pairing manager stamps
/// `bus:"pair"`), so forwarding the line IS the byte-faithful frame. There is no
/// initial snapshot; the keepalive beat runs alongside.
pub(super) async fn mesh_loop(mut socket: WebSocket, state: AppState, _auth: WsAuth) {
    if !is_ground_station(&state) {
        close_profile_mismatch(&mut socket).await;
        return;
    }

    let mut mesh_tail = JournalTail::new(state.run_dir.join("mesh-events.jsonl"));
    let mut pair_tail = JournalTail::new(state.run_dir.join("pair-events.jsonl"));
    let mut keepalive = keepalive_ticker();
    let mut last_heard = Instant::now();

    loop {
        // Drain whatever new lines each journal has, forwarding each verbatim.
        // A journal that is missing/rotating yields nothing and is retried on the
        // next poll, so one absent journal never starves the other.
        let mut forwarded_any = false;
        for tail in [&mut mesh_tail, &mut pair_tail] {
            while let Some(line) = tail.next_line().await {
                // Forward only well-formed JSON; a blank or malformed line is
                // dropped.
                let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if !send_json(&mut socket, &event).await {
                    return;
                }
                forwarded_any = true;
            }
        }

        // After forwarding, loop straight back to drain any further backlog;
        // otherwise wait out the poll interval. The select keeps the loop
        // responsive to a disconnect and to the keepalive beat either way.
        let wait = if forwarded_any {
            Duration::ZERO
        } else {
            MESH_POLL_INTERVAL
        };
        tokio::select! {
            incoming = socket.recv() => {
                if !client_still_open(&incoming) {
                    return;
                }
                last_heard = Instant::now();
            }
            _ = tokio::time::sleep(wait) => {}
            _ = keepalive.tick() => {
                if !keepalive_beat(&mut socket, last_heard).await {
                    return;
                }
            }
        }
    }
}

/// A follower over an append-only newline-JSON journal.
///
/// When the journal already exists as the stream opens, the tail seeks to its
/// end, so a long-lived journal never replays stale events into a freshly
/// connected client. Any file the tail opens after that (the journal appearing
/// late, or being replaced by rename or truncated by a writer restart) holds only
/// events written since the client connected, so it is read from its start.
/// Replacement is detected by the path's device and inode differing from the
/// open handle's, not by size, because a new file can outgrow the old offset
/// before the next poll.
pub(super) struct JournalTail {
    pub(super) path: PathBuf,
    pub(super) reader: Option<BufReader<tokio::fs::File>>,
    /// `(device, inode)` of the open handle.
    pub(super) identity: Option<(u64, u64)>,
    pub(super) offset: u64,
    /// Whether the next open reads from the start instead of seeking to the end.
    pub(super) from_start: bool,
}

/// The `(device, inode)` pair naming the file behind `meta`.
pub(super) fn file_identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

impl JournalTail {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            reader: None,
            identity: None,
            offset: 0,
            from_start: false,
        }
    }

    /// Drop the handle; the next call reopens the path from its start.
    pub(super) fn reopen_from_start(&mut self) {
        self.reader = None;
        self.identity = None;
        self.offset = 0;
        self.from_start = true;
    }

    /// Return the next complete line from the journal, or `None` when there is
    /// nothing new to read right now (missing file, no new bytes, a partial
    /// trailing line, or a replacement just detected).
    pub(super) async fn next_line(&mut self) -> Option<String> {
        // `read_line` comes from the top-level `AsyncBufReadExt`; `seek` needs
        // the seek extension trait in scope here.
        use tokio::io::AsyncSeekExt;

        if self.reader.is_none() {
            let mut file = match tokio::fs::File::open(&self.path).await {
                Ok(file) => file,
                Err(_) => {
                    // Missing now: whatever appears later is post-connect.
                    self.from_start = true;
                    return None;
                }
            };
            let identity = file_identity(&file.metadata().await.ok()?);
            let start = if self.from_start {
                0
            } else {
                file.seek(std::io::SeekFrom::End(0)).await.ok()?
            };
            self.offset = start;
            self.identity = Some(identity);
            self.reader = Some(BufReader::new(file));
            self.from_start = true;
        }

        let reader = self.reader.as_mut()?;
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            // EOF with no bytes: nothing new on this handle. If the path now
            // names another file, or ours shrank below what we read, or it is
            // gone, reopen from the start on the next call.
            Ok(0) => {
                let current = tokio::fs::metadata(&self.path).await.ok();
                let same = current.as_ref().is_some_and(|meta| {
                    Some(file_identity(meta)) == self.identity && meta.len() >= self.offset
                });
                if !same {
                    self.reopen_from_start();
                }
                None
            }
            Ok(n) => {
                // A line is complete only when it ends in a newline; a partial
                // trailing write is held back until the writer finishes it.
                if line.ends_with('\n') {
                    self.offset += n as u64;
                    Some(line)
                } else {
                    // Rewind so the partial line is re-read once it is complete.
                    let reader = self.reader.as_mut()?;
                    let _ = reader.seek(std::io::SeekFrom::Start(self.offset)).await;
                    None
                }
            }
            Err(_) => {
                self.reopen_from_start();
                None
            }
        }
    }
}
