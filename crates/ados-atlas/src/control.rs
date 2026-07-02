//! The capture control seam: an inbound Unix socket the ground control station
//! drives the capture session through (start / stop / pause / resume / status).
//!
//! The daemon auto-starts one session at launch and captures by default; this
//! socket lets an operator take control of that session at runtime without a
//! restart. The front (`ados-control`) forwards each operator action here; a
//! request is one newline-delimited JSON object `{"cmd": "..."}` and the reply is
//! one newline-delimited JSON [`CaptureStatus`] — the session's status AFTER the
//! command was applied, so the caller sees the resulting state directly.
//!
//! Each accepted command is forwarded to the capture loop over an mpsc channel;
//! the loop is the single owner of the session, so all mutation happens there.
//! A mutating command (start/stop/pause/resume) is sent first, then a status
//! query, over the same channel — because the loop is a single FIFO consumer the
//! status reply always reflects the post-mutation state.

use std::io;

use ados_protocol::atlas::CaptureStatus;
use ados_protocol::ipc::bind_command_socket;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// A control command forwarded from the control socket to the capture loop. The
/// mutating variants carry no reply channel (the socket server follows them with
/// a `Status` query to read back the resulting state); `Status` carries a
/// one-shot the loop answers without blocking its frame path.
pub enum AtlasControlCmd {
    /// Begin a fresh capture session (a new session id, counters reset).
    Start,
    /// Finalize the live session and mark it bagged so the compute node's
    /// reconstruct trigger fires.
    Stop,
    /// Pause capture (keyframe selection stops; selectors are retained).
    Pause,
    /// Resume a paused session.
    Resume,
    /// Read the current capture status without changing it.
    Status(oneshot::Sender<CaptureStatus>),
}

/// The parsed operator action a control-socket line maps to. Distinct from
/// [`AtlasControlCmd`] because a mutating action is sent to the loop as its
/// mutating variant AND a following `Status` query, so the wire reply is the
/// resulting status regardless of which command was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlAction {
    Start,
    Stop,
    Pause,
    Resume,
    Status,
}

/// The `{"cmd": "..."}` request body a control-socket line carries.
#[derive(Debug, Deserialize)]
struct ControlRequest {
    cmd: String,
}

/// Map a request line to an action. Returns `None` for a malformed line or an
/// unknown command name; the caller treats an unknown request as a no-op status
/// query so the wire reply stays a uniform [`CaptureStatus`].
fn parse_action(line: &str) -> Option<ControlAction> {
    let req: ControlRequest = serde_json::from_str(line.trim()).ok()?;
    Some(match req.cmd.trim().to_ascii_lowercase().as_str() {
        "start" => ControlAction::Start,
        "stop" => ControlAction::Stop,
        "pause" => ControlAction::Pause,
        "resume" => ControlAction::Resume,
        "status" => ControlAction::Status,
        _ => return None,
    })
}

/// Bind the control socket at `socket_path` and spawn its accept loop, returning
/// the accept task's handle. Mirrors the atlas bus bind hygiene (create the
/// parent dir, clear a stale socket, world-accessible mode) so a re-run never
/// fails with `EADDRINUSE`. Each accepted connection is served on its own task;
/// commands are forwarded to the capture loop over `tx`.
pub async fn serve_control(
    socket_path: &str,
    tx: mpsc::Sender<AtlasControlCmd>,
) -> io::Result<JoinHandle<()>> {
    // The shared helper owns the create-dir / remove-stale / bind / chmod hygiene
    // (mode 0o666, world-accessible, matching the atlas bus). The accept loop and
    // per-connection handler stay bespoke below: this socket's reply is
    // conditional — a command that arrives after the capture loop has gone closes
    // with NO response — which the uniform-response `serve_rpc` helper cannot
    // express, so only the bind hygiene is shared here.
    let listener = bind_command_socket(socket_path, 0o666)?;
    tracing::info!(path = %socket_path, "atlas_control_listening");

    let handle = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _addr)) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "atlas_control_accept_failed");
                    break;
                }
            };
            let conn_tx = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, conn_tx).await {
                    tracing::debug!(error = %e, "atlas_control_conn_error");
                }
            });
        }
    });
    Ok(handle)
}

/// Serve one control connection: read one request line, forward it to the
/// capture loop, and write back the resulting status as one newline-delimited
/// JSON object. A closed connection before any request, or a capture loop that
/// has gone away (the daemon is stopping), ends the connection quietly.
async fn handle_conn(stream: UnixStream, tx: mpsc::Sender<AtlasControlCmd>) -> io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        // The client closed without sending a request.
        return Ok(());
    }
    let action = parse_action(&line).unwrap_or(ControlAction::Status);
    let Some(status) = apply_over_channel(&tx, action).await else {
        // The capture loop dropped its receiver (the daemon is stopping); close.
        return Ok(());
    };
    let mut body = serde_json::to_vec(&status).map_err(io::Error::other)?;
    body.push(b'\n');
    write_half.write_all(&body).await?;
    write_half.flush().await?;
    Ok(())
}

/// Forward `action` to the capture loop and return the resulting status. A
/// mutating action is sent first, then a `Status` query, so the reply reflects
/// the post-mutation state (the loop is a single FIFO consumer). Returns `None`
/// only when the loop's receiver is gone.
async fn apply_over_channel(
    tx: &mpsc::Sender<AtlasControlCmd>,
    action: ControlAction,
) -> Option<CaptureStatus> {
    match action {
        ControlAction::Start => tx.send(AtlasControlCmd::Start).await.ok()?,
        ControlAction::Stop => tx.send(AtlasControlCmd::Stop).await.ok()?,
        ControlAction::Pause => tx.send(AtlasControlCmd::Pause).await.ok()?,
        ControlAction::Resume => tx.send(AtlasControlCmd::Resume).await.ok()?,
        ControlAction::Status => {}
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(AtlasControlCmd::Status(reply_tx)).await.ok()?;
    reply_rx.await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::atlas::{CaptureState, VioHealth};
    use std::sync::{Arc, Mutex};

    fn fixed_status(state: CaptureState) -> CaptureStatus {
        CaptureStatus {
            session_id: "sess-x".to_string(),
            state,
            keyframes: 3,
            vio_health: VioHealth::Good,
            camera_count: 1,
            ingest_rate_hz: 9.5,
        }
    }

    #[test]
    fn parse_action_maps_the_five_commands() {
        assert_eq!(
            parse_action(r#"{"cmd":"start"}"#),
            Some(ControlAction::Start)
        );
        assert_eq!(parse_action(r#"{"cmd":"stop"}"#), Some(ControlAction::Stop));
        assert_eq!(
            parse_action(r#"{"cmd":"pause"}"#),
            Some(ControlAction::Pause)
        );
        assert_eq!(
            parse_action(r#"{"cmd":"resume"}"#),
            Some(ControlAction::Resume)
        );
        assert_eq!(
            parse_action(r#"{"cmd":"status"}"#),
            Some(ControlAction::Status)
        );
        // Case-insensitive + whitespace-tolerant.
        assert_eq!(
            parse_action("  {\"cmd\": \" START \"}  \n"),
            Some(ControlAction::Start)
        );
    }

    #[test]
    fn parse_action_rejects_unknown_and_malformed() {
        assert_eq!(parse_action(r#"{"cmd":"reboot"}"#), None);
        assert_eq!(parse_action("not json"), None);
        assert_eq!(parse_action(r#"{"nope":"start"}"#), None);
    }

    /// Spawn a stand-in capture loop that records mutating commands and answers
    /// `Status` with a fixed snapshot, so a socket round-trip can be asserted
    /// without a real session.
    fn stub_loop(
        mut rx: mpsc::Receiver<AtlasControlCmd>,
        state: CaptureState,
    ) -> Arc<Mutex<Vec<&'static str>>> {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = recorded.clone();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    AtlasControlCmd::Start => sink.lock().unwrap().push("start"),
                    AtlasControlCmd::Stop => sink.lock().unwrap().push("stop"),
                    AtlasControlCmd::Pause => sink.lock().unwrap().push("pause"),
                    AtlasControlCmd::Resume => sink.lock().unwrap().push("resume"),
                    AtlasControlCmd::Status(reply) => {
                        let _ = reply.send(fixed_status(state));
                    }
                }
            }
        });
        recorded
    }

    async fn round_trip(socket: &str, request: &str) -> CaptureStatus {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(request.as_bytes()).await.unwrap();
        write_half.write_all(b"\n").await.unwrap();
        write_half.flush().await.unwrap();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    #[tokio::test]
    async fn status_request_returns_the_status_without_mutating() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atlas-control.sock");
        let socket_str = socket.to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(16);
        let recorded = stub_loop(rx, CaptureState::Capturing);
        let _handle = serve_control(&socket_str, tx).await.unwrap();

        let status = round_trip(&socket_str, r#"{"cmd":"status"}"#).await;
        assert_eq!(status.state, CaptureState::Capturing);
        assert_eq!(status.session_id, "sess-x");
        // No mutating command reached the loop for a status request.
        assert!(recorded.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn start_request_forwards_a_mutation_then_reads_status() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atlas-control.sock");
        let socket_str = socket.to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(16);
        let recorded = stub_loop(rx, CaptureState::Capturing);
        let _handle = serve_control(&socket_str, tx).await.unwrap();

        let status = round_trip(&socket_str, r#"{"cmd":"start"}"#).await;
        assert_eq!(status.state, CaptureState::Capturing);
        // The mutation was forwarded before the status read.
        assert_eq!(*recorded.lock().unwrap(), vec!["start"]);
    }

    #[tokio::test]
    async fn each_mutating_command_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atlas-control.sock");
        let socket_str = socket.to_str().unwrap().to_string();
        let (tx, rx) = mpsc::channel(16);
        let recorded = stub_loop(rx, CaptureState::Bagged);
        let _handle = serve_control(&socket_str, tx).await.unwrap();

        for cmd in ["stop", "pause", "resume"] {
            round_trip(&socket_str, &format!("{{\"cmd\":\"{cmd}\"}}")).await;
        }
        assert_eq!(*recorded.lock().unwrap(), vec!["stop", "pause", "resume"]);
    }
}
