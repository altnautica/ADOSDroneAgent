//! Inference backends.
//!
//! A [`VisionBackend`] loads a model into a [`LoadedModel`] that runs inference
//! on one frame at a time. Three backends share the trait:
//!
//! - [`MockBackend`] — always available, returns no detections. The default so
//!   the engine runs (and the crate builds) without any native runtime, and the
//!   fallback when a real backend cannot load a model.
//! - `OnnxBackend` — an ONNX Runtime backend, compiled in only under the `onnx`
//!   cargo feature so the default build stays free of the heavy native runtime.
//! - [`RknnSidecarBackend`] — an IPC client that forwards load + infer requests
//!   to the Python accelerator sidecar over `/run/ados/vision-rknn.sock` using
//!   the same 4-byte big-endian length-prefixed msgpack framing as the other
//!   agent sockets. The NPU vendor runtime (RKNN, TensorRT) is reached only
//!   through that sidecar, never linked here.

use ados_protocol::framebus::{
    BoundingBox, Detection, DetectionHead, FrameFormat, LockState, ModelMetadata,
};
use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::Duration;

/// Cap on a single framed message, matching the sidecar's `MAX_FRAME_BYTES`.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// A loaded, ready-to-run model.
pub trait LoadedModel: Send + Sync {
    /// Run inference on one raw frame in `format` at `width` x `height` and
    /// return the detections.
    fn infer(
        &self,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Result<Vec<Detection>>;
}

/// A backend that can load models of the kinds it supports.
pub trait VisionBackend: Send + Sync {
    /// Load `meta` into a runnable model.
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>>;
    /// Short backend name for logs and the registry (`mock`, `onnx`, `rknn`).
    fn name(&self) -> &str;
    /// Whether this backend actually runs inference. The mock backend returns
    /// no detections, so a vision engine wired to it produces a silently-empty
    /// detection stream; a status surface should flag that to the operator
    /// rather than presenting it as a working pipeline. Real backends override
    /// to `true`.
    fn is_inference_capable(&self) -> bool {
        true
    }
}

// --- mock -----------------------------------------------------------------

/// Always-available no-op backend. Loads any model and returns no detections,
/// which keeps the engine and its plugins running while a real backend is
/// unavailable.
#[derive(Debug, Default, Clone)]
pub struct MockBackend;

struct MockModel;

impl LoadedModel for MockModel {
    fn infer(&self, _frame: &[u8], _w: u32, _h: u32, _f: FrameFormat) -> Result<Vec<Detection>> {
        Ok(Vec::new())
    }
}

impl VisionBackend for MockBackend {
    fn load(&self, _meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(Box::new(MockModel))
    }
    fn name(&self) -> &str {
        "mock"
    }
    fn is_inference_capable(&self) -> bool {
        false
    }
}

// --- onnx (opt-in) --------------------------------------------------------

#[cfg(feature = "onnx")]
mod onnx_backend {
    use super::*;

    use crate::yolo;

    /// Default detection thresholds, matching the Python sidecar's defaults.
    const ONNX_CONF_THRESHOLD: f32 = 0.25;
    const ONNX_NMS_IOU: f32 = 0.45;

    /// ONNX Runtime backend. Loads `model_path` into an ORT session and runs it
    /// on each frame. Built only under the `onnx` feature.
    pub struct OnnxBackend;

    struct OnnxModel {
        /// `Session::run` takes `&mut self`, but `LoadedModel::infer` is `&self`
        /// (the engine serializes inference on the accelerator lease anyway), so
        /// the session sits behind a mutex for interior mutability.
        session: Mutex<ort::session::Session>,
        /// The model's first input tensor name (e.g. `images`), captured at load.
        input_name: String,
        meta: ModelMetadata,
    }

    #[allow(clippy::new_without_default)]
    impl OnnxBackend {
        pub fn new() -> Self {
            OnnxBackend
        }
    }

    impl VisionBackend for OnnxBackend {
        fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
            let path = meta
                .model_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("onnx model has no model_path"))?;
            let session = ort::session::Session::builder()?.commit_from_file(path)?;
            let input_name = session
                .inputs()
                .first()
                .map(|i| i.name().to_string())
                .ok_or_else(|| anyhow::anyhow!("onnx model has no inputs"))?;
            Ok(Box::new(OnnxModel {
                session: Mutex::new(session),
                input_name,
                meta: meta.clone(),
            }))
        }
        fn name(&self) -> &str {
            "onnx"
        }
    }

    impl LoadedModel for OnnxModel {
        fn infer(
            &self,
            frame: &[u8],
            w: u32,
            h: u32,
            f: FrameFormat,
        ) -> Result<Vec<Detection>> {
            if f != FrameFormat::Rgb24 {
                return Err(anyhow!(
                    "onnx backend requires rgb24 frames, got {f:?}; \
                     feed an rgb24-converted frame"
                ));
            }
            let iw = self.meta.input_width;
            let ih = self.meta.input_height;
            let chw = yolo::preprocess_rgb24_nchw(frame, w, h, iw, ih)
                .ok_or_else(|| anyhow!("onnx preprocess failed (frame too small?)"))?;

            let input =
                ort::value::Tensor::from_array(([1usize, 3, ih as usize, iw as usize], chw))?;
            let mut session = self
                .session
                .lock()
                .map_err(|_| anyhow!("onnx session lock poisoned"))?;
            let outputs = session.run(ort::inputs![self.input_name.as_str() => input])?;
            let value = outputs
                .iter()
                .next()
                .map(|(_, v)| v)
                .ok_or_else(|| anyhow!("onnx model produced no output"))?;
            let (shape, data) = value.try_extract_tensor::<f32>()?;
            let (rows, cols) = last_two_dims(shape);

            let params = yolo::DecodeParams {
                head: self.meta.head,
                labels: &self.meta.output_classes,
                input_w: iw,
                input_h: ih,
                frame_w: w,
                frame_h: h,
                conf_threshold: ONNX_CONF_THRESHOLD,
                nms_iou: ONNX_NMS_IOU,
            };
            Ok(yolo::decode(data, rows, cols, &params))
        }
    }

    /// The last two dimensions of an output shape (the feature/anchor axes after
    /// the batch dim). A 1-D output is read as a single row; an empty shape is
    /// `(0, 0)` so the decode yields nothing.
    fn last_two_dims(shape: &[i64]) -> (usize, usize) {
        let dims: Vec<usize> = shape.iter().map(|&d| d.max(0) as usize).collect();
        match dims.len() {
            0 => (0, 0),
            1 => (1, dims[0]),
            n => (dims[n - 2], dims[n - 1]),
        }
    }
}

#[cfg(feature = "onnx")]
pub use onnx_backend::OnnxBackend;

// --- rknn sidecar ---------------------------------------------------------

/// An IPC client to the Python accelerator sidecar. The sidecar owns the vendor
/// NPU runtime (RKNN on Rockchip, TensorRT on Jetson); this backend forwards
/// `load_model` and `infer` requests to it and decodes the detection reply.
///
/// The socket path is resolved once at construction. A load or infer call that
/// cannot reach the sidecar returns an error, which the engine treats as a
/// degraded model (it falls back to the mock model for that registration) so a
/// missing sidecar never crashes the engine.
pub struct RknnSidecarBackend {
    socket_path: String,
}

impl RknnSidecarBackend {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }
}

struct RknnModel {
    socket_path: String,
    model_id: String,
    model_path: String,
    input_w: u32,
    input_h: u32,
    input_format: String,
    class_labels: Vec<String>,
    /// The output-head layout the sidecar must decode with (`yolov8` | `yolov5`).
    head: String,
    /// Whether `load_model` has been sent to the sidecar this session. The
    /// sidecar keeps loaded models across connections, so we send the load
    /// handshake once (lazily, on the first infer) and re-send it only if the
    /// sidecar reports the model is no longer loaded (it was restarted).
    loaded: Mutex<bool>,
}

impl VisionBackend for RknnSidecarBackend {
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        // Capture the load parameters; the sidecar handshake is deferred to the
        // first infer so the registry can record the model even when the sidecar
        // is not up yet (a missing model_path degrades to an unreachable sidecar
        // at infer time, which the engine treats as a degraded model).
        Ok(Box::new(RknnModel {
            socket_path: self.socket_path.clone(),
            model_id: meta.id.clone(),
            model_path: meta.model_path.clone().unwrap_or_default(),
            input_w: meta.input_width,
            input_h: meta.input_height,
            input_format: fmt_str(meta.input_format).to_string(),
            class_labels: meta.output_classes.clone(),
            head: head_str(meta.head).to_string(),
            loaded: Mutex::new(false),
        }))
    }
    fn name(&self) -> &str {
        "rknn"
    }
}

impl RknnModel {
    /// Send `load_model` to the sidecar once. Idempotent: a no-op after the
    /// first success until [`mark_unloaded`] resets it.
    fn ensure_loaded(&self) -> Result<()> {
        let mut loaded = self.loaded.lock().expect("rknn load lock");
        if *loaded {
            return Ok(());
        }
        let req = load_request(
            &self.model_id,
            &self.model_path,
            self.input_w,
            self.input_h,
            &self.input_format,
            &self.class_labels,
            &self.head,
        );
        let resp = round_trip(&self.socket_path, &req)?;
        check_ok(&resp).context("sidecar load_model")?;
        *loaded = true;
        Ok(())
    }

    fn mark_unloaded(&self) {
        *self.loaded.lock().expect("rknn load lock") = false;
    }
}

impl LoadedModel for RknnModel {
    fn infer(&self, frame: &[u8], width: u32, height: u32, format: FrameFormat) -> Result<Vec<Detection>> {
        let fmt = fmt_str(format);
        self.ensure_loaded()?;
        let req = infer_request(&self.model_id, frame, width, height, fmt);
        let resp = round_trip(&self.socket_path, &req)?;
        match decode_detections(&resp) {
            Ok(dets) => Ok(dets),
            Err(e) if is_not_loaded(&e) => {
                // The sidecar restarted and dropped the model: reload once and retry.
                self.mark_unloaded();
                self.ensure_loaded()?;
                let resp2 = round_trip(&self.socket_path, &infer_request(&self.model_id, frame, width, height, fmt))?;
                decode_detections(&resp2)
            }
            Err(e) => Err(e),
        }
    }
}

// --- sidecar wire protocol (4-byte BE length + msgpack named map) ----------

fn fmt_str(f: FrameFormat) -> &'static str {
    match f {
        FrameFormat::Rgb24 => "rgb24",
        FrameFormat::Nv12 => "nv12",
        FrameFormat::Yuv420p => "yuv420p",
    }
}

/// The sidecar's lowercase name for a detection head, so it decodes the right
/// output-tensor layout.
fn head_str(h: DetectionHead) -> &'static str {
    match h {
        DetectionHead::Yolo8 => "yolov8",
        DetectionHead::Yolo5 => "yolov5",
    }
}

fn mv(s: &str) -> rmpv::Value {
    rmpv::Value::from(s)
}

#[allow(clippy::too_many_arguments)]
fn load_request(
    model_id: &str,
    path: &str,
    input_w: u32,
    input_h: u32,
    format: &str,
    classes: &[String],
    head: &str,
) -> rmpv::Value {
    rmpv::Value::Map(vec![
        (mv("op"), mv("load_model")),
        (mv("model_id"), mv(model_id)),
        (mv("path"), mv(path)),
        (mv("input_w"), rmpv::Value::from(input_w as u64)),
        (mv("input_h"), rmpv::Value::from(input_h as u64)),
        (mv("format"), mv(format)),
        (
            mv("class_labels"),
            rmpv::Value::Array(classes.iter().map(|c| mv(c)).collect()),
        ),
        (mv("head"), mv(head)),
    ])
}

fn infer_request(model_id: &str, frame: &[u8], width: u32, height: u32, format: &str) -> rmpv::Value {
    rmpv::Value::Map(vec![
        (mv("op"), mv("infer")),
        (mv("model_id"), mv(model_id)),
        // bytes MUST be a msgpack bin, not an int array, so the sidecar can
        // np.frombuffer it.
        (mv("frame"), rmpv::Value::Binary(frame.to_vec())),
        (mv("width"), rmpv::Value::from(width as u64)),
        (mv("height"), rmpv::Value::from(height as u64)),
        (mv("format"), mv(format)),
    ])
}

/// One blocking framed request/response against the sidecar socket. A fresh
/// connection per call; the sidecar keys loaded models by id across connections.
fn round_trip(socket_path: &str, req: &rmpv::Value) -> Result<rmpv::Value> {
    let mut body = Vec::new();
    rmpv::encode::write_value(&mut body, req).context("encode request")?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(anyhow!("request body {} exceeds {MAX_FRAME_BYTES}", body.len()));
    }
    let mut stream =
        UnixStream::connect(socket_path).with_context(|| format!("connect sidecar at {socket_path}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let n = u32::from_be_bytes(len_buf) as usize;
    if n > MAX_FRAME_BYTES {
        return Err(anyhow!("response body {n} exceeds {MAX_FRAME_BYTES}"));
    }
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    rmpv::decode::read_value(&mut &buf[..]).context("decode response")
}

fn map_get<'a>(map: &'a [(rmpv::Value, rmpv::Value)], key: &str) -> Option<&'a rmpv::Value> {
    map.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
}

fn num_f32(v: &rmpv::Value) -> Option<f32> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|i| i as f64))
        .or_else(|| v.as_u64().map(|u| u as f64))
        .map(|f| f as f32)
}

fn parse_lock(s: &str) -> Option<LockState> {
    match s {
        "locked" => Some(LockState::Locked),
        "uncertain" => Some(LockState::Uncertain),
        "lost" => Some(LockState::Lost),
        _ => None,
    }
}

fn check_ok(resp: &rmpv::Value) -> Result<()> {
    let map = resp.as_map().ok_or_else(|| anyhow!("response is not a map"))?;
    match map_get(map, "status").and_then(|v| v.as_str()) {
        Some("ok") => Ok(()),
        _ => {
            let err = map_get(map, "error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            Err(anyhow!("{err}"))
        }
    }
}

fn is_not_loaded(e: &anyhow::Error) -> bool {
    e.to_string().to_ascii_lowercase().contains("not loaded")
}

/// Decode an `{status, detections}` reply into the wire `Detection` shape.
fn decode_detections(resp: &rmpv::Value) -> Result<Vec<Detection>> {
    check_ok(resp)?;
    let map = resp.as_map().ok_or_else(|| anyhow!("response is not a map"))?;
    let dets = match map_get(map, "detections").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(dets.len());
    for d in dets {
        let dm = d.as_map().ok_or_else(|| anyhow!("detection is not a map"))?;
        let bm = map_get(dm, "bbox")
            .and_then(|v| v.as_map())
            .ok_or_else(|| anyhow!("detection has no bbox map"))?;
        let bf = |k: &str| map_get(bm, k).and_then(num_f32).unwrap_or(0.0);
        out.push(Detection {
            bbox: BoundingBox {
                x: bf("x"),
                y: bf("y"),
                width: bf("width"),
                height: bf("height"),
            },
            class_label: map_get(dm, "class_label").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            confidence: map_get(dm, "confidence").and_then(num_f32).unwrap_or(0.0),
            track_id: map_get(dm, "track_id").and_then(|v| v.as_u64()),
            assoc_confidence: map_get(dm, "assoc_confidence").and_then(num_f32),
            lock_state: map_get(dm, "lock_state").and_then(|v| v.as_str()).and_then(parse_lock),
        });
    }
    Ok(out)
}

// --- picker ---------------------------------------------------------------

/// The minimal config view the picker needs (avoids a config dependency cycle).
pub struct BackendPrefs<'a> {
    /// Operator preference: "auto" | "mock" | "onnx" | "rknn".
    pub preference: &'a str,
    /// The accelerator sidecar socket path (for the rknn backend).
    pub rknn_socket_path: String,
}

/// Whether the build carries the ONNX CPU backend.
const ONNX_COMPILED: bool = cfg!(feature = "onnx");

/// Pick the backend for a board.
///
/// "auto" resolves by SoC family: a Rockchip part with an NPU (`rk3576`,
/// `rk3588`, `rk3566`, ...) or a Jetson prefers the accelerator sidecar; a
/// non-NPU board (a Pi-class CPU-only SoC) prefers the ONNX CPU backend when the
/// binary was built with the `onnx` feature, and only falls back to the
/// detection-less mock when no runtime is available. An explicit preference is
/// honoured. The selection is logged at `warn` when it resolves to the mock so
/// an operator who enabled vision is not left with a silently-empty detection
/// stream and no signal that no real inference is running.
pub fn select_backend(board_soc: &str, prefs: &BackendPrefs) -> Box<dyn VisionBackend> {
    let soc = board_soc.to_ascii_lowercase();
    let want = match prefs.preference {
        "auto" => {
            if soc.starts_with("rk") || soc.contains("tegra") || soc.contains("jetson") {
                "rknn"
            } else if ONNX_COMPILED {
                // A non-NPU board with a real CPU runtime compiled in: use it
                // rather than the detection-less mock.
                "onnx"
            } else {
                "mock"
            }
        }
        other => other,
    };
    let backend: Box<dyn VisionBackend> = match want {
        "rknn" => Box::new(RknnSidecarBackend::new(prefs.rknn_socket_path.clone())),
        "onnx" => {
            #[cfg(feature = "onnx")]
            {
                Box::new(OnnxBackend::new())
            }
            #[cfg(not(feature = "onnx"))]
            {
                tracing::warn!("onnx backend requested but not compiled in; using mock");
                Box::new(MockBackend)
            }
        }
        "mock" => Box::new(MockBackend),
        unknown => {
            tracing::warn!(backend = unknown, "unknown vision backend; using mock");
            Box::new(MockBackend)
        }
    };
    if !backend.is_inference_capable() {
        // Loud, not silent: the engine will run but inference is a no-op, so an
        // enabled vision pipeline on this board produces no detections. The
        // engine surfaces the same fact through `is_inference_capable` on its
        // status so the GCS can show "no real inference running".
        tracing::warn!(
            soc = %soc,
            preference = prefs.preference,
            onnx_compiled = ONNX_COMPILED,
            "vision backend resolved to the mock: no real inference will run; \
             build with the onnx feature or attach an accelerator sidecar"
        );
    }
    backend
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::framebus::{ModelExecution, ModelKind};

    fn meta() -> ModelMetadata {
        ModelMetadata {
            id: "com.example.test".into(),
            kind: ModelKind::Detection,
            execution: ModelExecution::EngineRun,
            input_width: 640,
            input_height: 480,
            input_format: FrameFormat::Rgb24,
            output_classes: vec!["a".into()],
            model_path: None,
            head: ados_protocol::framebus::DetectionHead::Yolo8,
        }
    }

    #[test]
    fn mock_backend_loads_and_infers_empty() {
        let b = MockBackend;
        assert_eq!(b.name(), "mock");
        let m = b.load(&meta()).unwrap();
        let out = m.infer(&[0u8; 12], 2, 2, FrameFormat::Rgb24).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn rknn_backend_records_socket_and_errors_without_sidecar() {
        let b = RknnSidecarBackend::new("/nonexistent/ados-vision-rknn.sock");
        assert_eq!(b.name(), "rknn");
        assert_eq!(b.socket_path(), "/nonexistent/ados-vision-rknn.sock");
        let m = b.load(&meta()).unwrap();
        // No sidecar at the socket ⇒ infer returns Err (the engine degrades to the
        // mock model for this registration), never panics.
        assert!(m.infer(&[0u8; 4], 1, 1, FrameFormat::Rgb24).is_err());
    }

    #[test]
    fn rknn_infer_round_trips_with_mock_sidecar() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("ados-vision-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("rknn.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        // A mock sidecar: answer the load handshake, then return one detection for infer.
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut conn, _) = listener.accept().unwrap();
                let mut len = [0u8; 4];
                conn.read_exact(&mut len).unwrap();
                let n = u32::from_be_bytes(len) as usize;
                let mut buf = vec![0u8; n];
                conn.read_exact(&mut buf).unwrap();
                let req = rmpv::decode::read_value(&mut &buf[..]).unwrap();
                let op = req
                    .as_map()
                    .and_then(|m| map_get(m, "op"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let resp = if op == "infer" {
                    let det = rmpv::Value::Map(vec![
                        (
                            mv("bbox"),
                            rmpv::Value::Map(vec![
                                (mv("x"), rmpv::Value::F64(10.0)),
                                (mv("y"), rmpv::Value::F64(20.0)),
                                (mv("width"), rmpv::Value::F64(30.0)),
                                (mv("height"), rmpv::Value::F64(40.0)),
                            ]),
                        ),
                        (mv("class_label"), mv("UAV")),
                        (mv("confidence"), rmpv::Value::F64(0.9)),
                        (mv("track_id"), rmpv::Value::from(7u64)),
                        (mv("lock_state"), mv("locked")),
                    ]);
                    rmpv::Value::Map(vec![
                        (mv("status"), mv("ok")),
                        (mv("detections"), rmpv::Value::Array(vec![det])),
                    ])
                } else {
                    rmpv::Value::Map(vec![(mv("status"), mv("ok"))])
                };
                let mut body = Vec::new();
                rmpv::encode::write_value(&mut body, &resp).unwrap();
                conn.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
                conn.write_all(&body).unwrap();
                conn.flush().unwrap();
            }
        });

        let backend = RknnSidecarBackend::new(sock.to_str().unwrap());
        let mut m = meta();
        m.model_path = Some("/tmp/uav.rknn".into());
        let model = backend.load(&m).unwrap();
        let dets = model.infer(&[1, 2, 3, 4], 1, 1, FrameFormat::Rgb24).unwrap();
        server.join().unwrap();
        let _ = std::fs::remove_file(&sock);

        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].class_label, "UAV");
        assert_eq!(dets[0].track_id, Some(7));
        assert_eq!(dets[0].lock_state, Some(LockState::Locked));
        assert!((dets[0].bbox.x - 10.0).abs() < 1e-3);
        assert!((dets[0].bbox.height - 40.0).abs() < 1e-3);
        assert!((dets[0].confidence - 0.9).abs() < 1e-3);
    }

    #[test]
    fn picker_auto_selects_sidecar_on_rockchip() {
        let prefs = BackendPrefs {
            preference: "auto",
            rknn_socket_path: "/run/ados/vision-rknn.sock".into(),
        };
        assert_eq!(select_backend("rk3576", &prefs).name(), "rknn");
        assert_eq!(select_backend("RK3588S2", &prefs).name(), "rknn");
        assert_eq!(select_backend("tegra234", &prefs).name(), "rknn");
        // A CPU-only SoC under auto prefers the real ONNX CPU backend when it is
        // compiled in, and only falls back to the detection-less mock when no
        // runtime is available. Either way it never silently picks mock when a
        // real CPU backend exists.
        let cpu = select_backend("bcm2711", &prefs);
        #[cfg(feature = "onnx")]
        {
            assert_eq!(cpu.name(), "onnx");
            assert!(cpu.is_inference_capable());
        }
        #[cfg(not(feature = "onnx"))]
        {
            assert_eq!(cpu.name(), "mock");
            // The mock is honestly flagged as not running real inference.
            assert!(!cpu.is_inference_capable());
        }
    }

    #[test]
    fn mock_backend_is_flagged_as_not_inference_capable() {
        // The status surface keys on this to tell the operator no real inference
        // runs; the real backends report capable.
        assert!(!MockBackend.is_inference_capable());
        assert!(RknnSidecarBackend::new("/x").is_inference_capable());
    }

    #[test]
    fn picker_honours_explicit_preference() {
        let prefs = BackendPrefs {
            preference: "mock",
            rknn_socket_path: "/run/ados/vision-rknn.sock".into(),
        };
        assert_eq!(select_backend("rk3576", &prefs).name(), "mock");

        // onnx falls back to mock when not compiled in.
        let prefs_onnx = BackendPrefs {
            preference: "onnx",
            rknn_socket_path: "/run/ados/vision-rknn.sock".into(),
        };
        let name = select_backend("x86", &prefs_onnx).name().to_string();
        #[cfg(feature = "onnx")]
        assert_eq!(name, "onnx");
        #[cfg(not(feature = "onnx"))]
        assert_eq!(name, "mock");
    }
}
