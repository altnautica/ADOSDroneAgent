//! The vision-engine request/response socket client.
//!
//! The vision engine owns `/run/ados/vision.sock`, which speaks the
//! length-prefixed msgpack [`Envelope`] wire (4-byte big-endian length + a
//! msgpack envelope). Unlike the MAVLink socket (a broadcast the command route
//! fires frames at), this is request/response: the front writes one request
//! envelope and reads the matching reply. The only call the control surface
//! needs is `vision.designate_track` (operator click-to-follow), which is
//! infrequent, so this opens a fresh connection per call and closes it — no held
//! state, no reconnect dance.
//!
//! An absent socket (no vision engine, or vision disabled) surfaces as
//! [`VisionError::Io`], which the route maps to a 503 — a designate is never
//! silently dropped.

use std::path::{Path, PathBuf};

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::methods;
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::Value;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// The vision socket file name under the runtime dir.
pub const VISION_SOCKET_NAME: &str = "vision.sock";

/// The default vision socket path, honouring `ADOS_RUN_DIR` like the other IPC
/// clients so a test points it at a tempdir. Defaults to `/run/ados/vision.sock`.
pub fn default_vision_socket() -> PathBuf {
    let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&run_dir).join(VISION_SOCKET_NAME)
}

/// A vision request-path error.
#[derive(Debug, Error)]
pub enum VisionError {
    /// The vision socket could not be reached or the I/O failed (the socket is
    /// absent, or the connection broke). The route maps it to a 503.
    #[error("vision socket io failed: {0}")]
    Io(#[from] std::io::Error),
    /// The reply could not be framed/deframed (over the cap, or a malformed
    /// envelope on the wire).
    #[error("vision frame error: {0}")]
    Frame(String),
    /// The engine answered with an envelope `error` (e.g. a bad request). Carries
    /// the engine's message verbatim; the route surfaces it as a 4xx.
    #[error("{0}")]
    Rpc(String),
}

/// Connects to the vision engine socket and runs a single request/response.
#[derive(Clone)]
pub struct VisionIpcClient {
    socket_path: PathBuf,
}

impl VisionIpcClient {
    /// Build a client for the given socket path.
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Build a client at the default vision socket path (`ADOS_RUN_DIR`-aware).
    pub fn default_socket() -> Self {
        Self::new(default_vision_socket())
    }

    /// The socket path this client talks to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Send a `vision.designate_track` request (lock a camera's tracker onto a
    /// specific box) and return the engine's response args.
    pub async fn designate_track(&self, args: Value) -> Result<Value, VisionError> {
        self.request(methods::DESIGNATE_TRACK, "vision.track.designate", args)
            .await
    }

    /// One fresh-connection request/response against the engine socket.
    async fn request(
        &self,
        method: &str,
        capability: &str,
        args: Value,
    ) -> Result<Value, VisionError> {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: method.to_string(),
            capability: capability.to_string(),
            args,
            request_id: "ctl-vision".to_string(),
            token: String::new(),
            error: None,
        };
        let frame = env
            .encode_frame()
            .map_err(|e| VisionError::Frame(format!("encode envelope: {e}")))?;

        let mut stream = UnixStream::connect(&self.socket_path).await?;
        stream.write_all(&frame).await?;
        stream.flush().await?;

        let mut header = [0u8; HEADER_SIZE];
        stream.read_exact(&mut header).await?;
        let len = decode_len(header, PLUGIN_MAX_FRAME, false)
            .map_err(|e| VisionError::Frame(format!("response length: {e}")))?;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await?;
        let resp = Envelope::from_msgpack(&body)
            .map_err(|e| VisionError::Frame(format!("decode envelope: {e}")))?;
        if let Some(err) = resp.error {
            return Err(VisionError::Rpc(err));
        }
        Ok(resp.args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// A designate against an absent socket is an I/O error (the route maps it to
    /// a 503), not a panic.
    #[tokio::test]
    async fn designate_against_absent_socket_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = VisionIpcClient::new(dir.path().join("absent.sock"));
        let err = client.designate_track(Value::Map(vec![])).await.unwrap_err();
        assert!(matches!(err, VisionError::Io(_)), "expected Io: {err:?}");
    }

    /// A round-trip against a mock engine: the client sends a request envelope and
    /// reads the reply envelope's args.
    #[tokio::test]
    async fn designate_round_trips_with_a_mock_engine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vision.sock");
        let listener = UnixListener::bind(&path).unwrap();

        // Mock engine: read one request envelope, reply with a fixed ok envelope.
        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, PLUGIN_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            let req = Envelope::from_msgpack(&body).unwrap();
            assert_eq!(req.method, methods::DESIGNATE_TRACK);
            let reply = Envelope {
                version: PROTOCOL_VERSION,
                kind: "response".to_string(),
                method: methods::DESIGNATE_TRACK.to_string(),
                capability: String::new(),
                args: Value::Map(vec![
                    (Value::from("designated"), Value::Boolean(true)),
                    (Value::from("track_id"), Value::from(7u64)),
                ]),
                request_id: req.request_id,
                token: String::new(),
                error: None,
            };
            let frame = reply.encode_frame().unwrap();
            conn.write_all(&frame).await.unwrap();
            conn.flush().await.unwrap();
        });

        let client = VisionIpcClient::new(path);
        let args = Value::Map(vec![(Value::from("camera_id"), Value::from("cam-0"))]);
        let resp = client.designate_track(args).await.unwrap();
        let map = resp.as_map().unwrap();
        let designated = map
            .iter()
            .find(|(k, _)| k.as_str() == Some("designated"))
            .map(|(_, v)| v.as_bool());
        assert_eq!(designated, Some(Some(true)));
        server.await.unwrap();
    }

    /// An engine `error` envelope surfaces as `VisionError::Rpc`.
    #[tokio::test]
    async fn engine_error_envelope_surfaces_as_rpc_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vision.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, PLUGIN_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            let req = Envelope::from_msgpack(&body).unwrap();
            let reply = Envelope {
                version: PROTOCOL_VERSION,
                kind: "response".to_string(),
                method: req.method,
                capability: String::new(),
                args: Value::Map(vec![]),
                request_id: req.request_id,
                token: String::new(),
                error: Some("designate missing bbox".to_string()),
            };
            let frame = reply.encode_frame().unwrap();
            conn.write_all(&frame).await.unwrap();
            conn.flush().await.unwrap();
        });

        let client = VisionIpcClient::new(path);
        let err = client
            .designate_track(Value::Map(vec![]))
            .await
            .unwrap_err();
        assert!(matches!(err, VisionError::Rpc(m) if m.contains("missing bbox")));
        server.await.unwrap();
    }
}
