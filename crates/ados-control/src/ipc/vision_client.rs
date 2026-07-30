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
use std::time::Duration;

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::methods;
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::Value;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// The vision socket file name under the runtime dir.
pub const VISION_SOCKET_NAME: &str = "vision.sock";

/// Wall bound on one vision request, covering connect, write and both reads.
///
/// The engine answers off a local socket, so a healthy reply lands in well
/// under a millisecond; this exists purely for the pathological case where the
/// engine accepts the connection and then never replies. That case is not an
/// I/O error, so without a bound it is an unbounded hang rather than a failure.
/// It matters because this client is no longer only on the infrequent designate
/// path: `/api/status` and `/api/status/full` call it on every poll, and the
/// GCS polls those continuously, so one wedged-but-alive engine would stall
/// every poller indefinitely. A timeout surfaces as [`VisionError::Io`], which
/// the status path already degrades to `false` and the designate route to a 503.
const VISION_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

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

    /// Send a `vision.list_models` request and return the engine's response args
    /// (`{models: Binary(msgpack Vec<ModelInfo>)}`). A read-back of the model
    /// registry for the GCS vision hub.
    pub async fn list_models(&self) -> Result<Value, VisionError> {
        self.request(methods::LIST_MODELS, "vision.model.list", Value::Nil)
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

        let exchange = async {
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
        };

        match tokio::time::timeout(VISION_REQUEST_TIMEOUT, exchange).await {
            Ok(result) => result,
            Err(_) => Err(VisionError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "vision engine did not answer {method} within {}s",
                    VISION_REQUEST_TIMEOUT.as_secs()
                ),
            ))),
        }
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
        let err = client
            .designate_track(Value::Map(vec![]))
            .await
            .unwrap_err();
        assert!(matches!(err, VisionError::Io(_)), "expected Io: {err:?}");
    }

    /// An engine that accepts the connection and then goes silent must not hang
    /// the caller. This is the case a plain `Err(_) => false` cannot catch: a
    /// stalled read is not an I/O error, and `/api/status` runs this on every
    /// poll, so an unbounded read would stall every poller indefinitely.
    /// Waits out the real bound (a couple of seconds) rather than pulling in
    /// tokio's `test-util` feature just to virtualise the clock.
    #[tokio::test]
    async fn a_silent_engine_times_out_instead_of_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // Accept, then hold the connection open forever without replying.
        let _engine = tokio::spawn(async move {
            let _stream = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let err = VisionIpcClient::new(path).list_models().await.unwrap_err();
        match err {
            VisionError::Io(e) => assert_eq!(
                e.kind(),
                std::io::ErrorKind::TimedOut,
                "a silent engine must surface as a timeout"
            ),
            other => panic!("expected a timeout, got {other:?}"),
        }
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

    /// `list_models` sends the `vision.list_models` method and reads back the
    /// engine's `{models: Binary(msgpack Vec<ModelInfo>)}` reply. The route
    /// decodes the Binary field; here we prove the client carries the right
    /// method + returns the reply args verbatim.
    #[tokio::test]
    async fn list_models_carries_the_method_and_returns_args() {
        use ados_protocol::framebus::{ModelExecution, ModelInfo, ModelKind};

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
            assert_eq!(req.method, methods::LIST_MODELS);
            let models = vec![ModelInfo {
                id: "yolov8n".to_string(),
                kind: ModelKind::Detection,
                execution: ModelExecution::EngineRun,
                backend_loaded: true,
                output_classes: vec!["person".to_string()],
                fps: 14.5,
                latency_ms: 22.0,
                is_inference_capable: true,
            }];
            let bytes = rmp_serde::to_vec_named(&models).unwrap();
            let reply = Envelope {
                version: PROTOCOL_VERSION,
                kind: "response".to_string(),
                method: methods::LIST_MODELS.to_string(),
                capability: String::new(),
                args: Value::Map(vec![(Value::from("models"), Value::Binary(bytes))]),
                request_id: req.request_id,
                token: String::new(),
                error: None,
            };
            let frame = reply.encode_frame().unwrap();
            conn.write_all(&frame).await.unwrap();
            conn.flush().await.unwrap();
        });

        let client = VisionIpcClient::new(path);
        let resp = client.list_models().await.unwrap();
        let map = resp.as_map().unwrap();
        let bytes = map
            .iter()
            .find(|(k, _)| k.as_str() == Some("models"))
            .and_then(|(_, v)| v.as_slice())
            .expect("models binary present");
        let decoded: Vec<ModelInfo> = rmp_serde::from_slice(bytes).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, "yolov8n");
        assert!(decoded[0].backend_loaded);
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
