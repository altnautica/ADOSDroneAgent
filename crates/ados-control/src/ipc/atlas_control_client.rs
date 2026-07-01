//! The atlas capture-control socket client.
//!
//! `ados-atlas` owns the drone's capture session and exposes an on-box,
//! daemon-lifetime control socket (`<run_dir>/atlas-control.sock`) that starts,
//! stops, pauses, resumes, or reads the session at runtime without a restart.
//! This client is that socket's caller: the native atlas routes on the front
//! forward each operator action here.
//!
//! The wire is one newline-delimited JSON request `{"cmd": "..."}` and one
//! newline-delimited JSON [`CaptureStatus`] reply (the session's status AFTER the
//! command applied), matching the `ados-atlas` control server. One fresh
//! connection per call — capture control actions are infrequent. An absent socket
//! (the capture service not running, because atlas is disabled or the drone has
//! no cameras) surfaces as [`AtlasControlError::Io`], which the route maps to a
//! 503 so a capture command is never silently dropped.
//!
//! Auth: the off-box auth is the LAN pairing-key edge on the atlas write routes
//! (the same posture as `/api/vision/designate`); by the time a request reaches
//! this socket it is an on-box, trusted caller.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_protocol::atlas::CaptureStatus;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The atlas control socket file name under the run dir.
const CONTROL_SOCKET_NAME: &str = "atlas-control.sock";
/// The default run dir the capture service binds its sockets under.
const RUN_DIR_DEFAULT: &str = "/run/ados";
/// A bounded per-request timeout so a wedged capture service cannot hang the
/// route's connection; the reply for a local socket is near-instant.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// The default control socket path, honouring the `ADOS_RUN_DIR` override the
/// capture service resolves it under (so a dev / SITL run points both at a
/// tempdir).
pub fn default_control_socket() -> PathBuf {
    let dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| RUN_DIR_DEFAULT.into());
    Path::new(&dir).join(CONTROL_SOCKET_NAME)
}

/// An atlas capture-control error.
#[derive(Debug, Error)]
pub enum AtlasControlError {
    /// The control socket could not be reached or the I/O failed (the capture
    /// service is not up). The route maps it to a 503.
    #[error("atlas control socket io failed: {0}")]
    Io(#[from] std::io::Error),
    /// The reply line could not be parsed as a capture status.
    #[error("atlas control reply parse failed: {0}")]
    Parse(String),
    /// The request did not complete within the bounded timeout.
    #[error("atlas control request timed out")]
    Timeout,
}

/// Connects to the atlas control socket and runs a single request/response.
#[derive(Clone)]
pub struct AtlasControlClient {
    socket_path: PathBuf,
}

impl AtlasControlClient {
    /// Build a client for the given socket path.
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Build a client for the default (`ADOS_RUN_DIR`-aware) socket path.
    pub fn default_socket() -> Self {
        Self::new(default_control_socket())
    }

    /// Read the current capture status without changing it.
    pub async fn status(&self) -> Result<CaptureStatus, AtlasControlError> {
        self.command("status").await
    }

    /// Begin a fresh capture session.
    pub async fn start(&self) -> Result<CaptureStatus, AtlasControlError> {
        self.command("start").await
    }

    /// Finalize + bag the live session so the compute node reconstructs it.
    pub async fn stop(&self) -> Result<CaptureStatus, AtlasControlError> {
        self.command("stop").await
    }

    /// Pause the live session.
    pub async fn pause(&self) -> Result<CaptureStatus, AtlasControlError> {
        self.command("pause").await
    }

    /// Resume a paused session.
    pub async fn resume(&self) -> Result<CaptureStatus, AtlasControlError> {
        self.command("resume").await
    }

    /// Run one command with the bounded timeout.
    async fn command(&self, cmd: &str) -> Result<CaptureStatus, AtlasControlError> {
        match tokio::time::timeout(REQUEST_TIMEOUT, self.command_inner(cmd)).await {
            Ok(result) => result,
            Err(_) => Err(AtlasControlError::Timeout),
        }
    }

    /// Connect, write one `{"cmd": "..."}` line, read one newline-JSON
    /// `CaptureStatus` reply.
    async fn command_inner(&self, cmd: &str) -> Result<CaptureStatus, AtlasControlError> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (read_half, mut write_half) = stream.into_split();
        let request = format!("{{\"cmd\":\"{cmd}\"}}\n");
        write_half.write_all(request.as_bytes()).await?;
        write_half.flush().await?;
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(AtlasControlError::Parse("empty reply".to_string()));
        }
        serde_json::from_str(line.trim()).map_err(|e| AtlasControlError::Parse(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::atlas::{CaptureState, VioHealth};
    use tokio::net::UnixListener;

    fn fixed_status() -> CaptureStatus {
        CaptureStatus {
            session_id: "sess-1".to_string(),
            state: CaptureState::Capturing,
            keyframes: 4,
            vio_health: VioHealth::Good,
            camera_count: 1,
            ingest_rate_hz: 9.5,
        }
    }

    /// Bind a stand-in control server that reads one request line and replies with
    /// a fixed status, so the client wire can be asserted without a real service.
    async fn stub_server(socket: PathBuf, recorded: std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                recorded.lock().unwrap().push(line.trim().to_string());
                let mut body = serde_json::to_vec(&fixed_status()).unwrap();
                body.push(b'\n');
                let _ = write_half.write_all(&body).await;
                let _ = write_half.flush().await;
            }
        });
    }

    #[tokio::test]
    async fn status_round_trips_the_capture_status() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atlas-control.sock");
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        stub_server(socket.clone(), recorded.clone()).await;
        // Give the listener a beat to be ready.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = AtlasControlClient::new(socket);
        let status = client.status().await.unwrap();
        assert_eq!(status.state, CaptureState::Capturing);
        assert_eq!(status.session_id, "sess-1");
        assert_eq!(recorded.lock().unwrap()[0], r#"{"cmd":"status"}"#);
    }

    #[tokio::test]
    async fn a_mutating_command_sends_its_verb() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atlas-control.sock");
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        stub_server(socket.clone(), recorded.clone()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = AtlasControlClient::new(socket);
        let _ = client.stop().await.unwrap();
        assert_eq!(recorded.lock().unwrap()[0], r#"{"cmd":"stop"}"#);
    }

    #[tokio::test]
    async fn an_absent_socket_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nope.sock");
        let client = AtlasControlClient::new(socket);
        let err = client.status().await.unwrap_err();
        assert!(matches!(err, AtlasControlError::Io(_)));
    }
}
