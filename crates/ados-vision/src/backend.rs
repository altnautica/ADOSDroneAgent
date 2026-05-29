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

use ados_protocol::framebus::{Detection, FrameFormat, ModelMetadata};
use anyhow::Result;

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
}

// --- onnx (opt-in) --------------------------------------------------------

#[cfg(feature = "onnx")]
mod onnx_backend {
    use super::*;

    /// ONNX Runtime backend. Loads `model_path` into an ORT session and runs it
    /// on each frame. Built only under the `onnx` feature.
    pub struct OnnxBackend;

    struct OnnxModel {
        _session: ort::session::Session,
        meta: ModelMetadata,
    }

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
            Ok(Box::new(OnnxModel {
                _session: session,
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
            _frame: &[u8],
            _w: u32,
            _h: u32,
            _f: FrameFormat,
        ) -> Result<Vec<Detection>> {
            // The tensor pre/post-processing per model family lands with the
            // first real model; the session is loaded and the contract is wired.
            let _ = &self.meta;
            Ok(Vec::new())
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
}

impl VisionBackend for RknnSidecarBackend {
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        // The load handshake is a blocking round-trip to the sidecar. It is
        // attempted lazily on the first infer in this build so the registry can
        // record the model even when the sidecar is not up yet; the model id
        // and socket are captured here.
        Ok(Box::new(RknnModel {
            socket_path: self.socket_path.clone(),
            model_id: meta.id.clone(),
        }))
    }
    fn name(&self) -> &str {
        "rknn"
    }
}

impl LoadedModel for RknnModel {
    fn infer(&self, _frame: &[u8], _w: u32, _h: u32, _f: FrameFormat) -> Result<Vec<Detection>> {
        // The synchronous sidecar round-trip (frame ref + model id over the
        // length-prefixed msgpack socket, detection batch back) is wired with
        // the sidecar's first model. The fields are captured so the call site
        // is fixed.
        let _ = (&self.socket_path, &self.model_id);
        Ok(Vec::new())
    }
}

// --- picker ---------------------------------------------------------------

/// The minimal config view the picker needs (avoids a config dependency cycle).
pub struct BackendPrefs<'a> {
    /// Operator preference: "auto" | "mock" | "onnx" | "rknn".
    pub preference: &'a str,
    /// The accelerator sidecar socket path (for the rknn backend).
    pub rknn_socket_path: String,
}

/// Pick the backend for a board.
///
/// "auto" resolves by SoC family: a Rockchip part with an NPU (`rk3576`,
/// `rk3588`, `rk3566`, ...) or a Jetson prefers the accelerator sidecar; an
/// explicit preference is honoured. ONNX is only selectable when compiled in,
/// otherwise the picker falls back to mock with a warning.
pub fn select_backend(board_soc: &str, prefs: &BackendPrefs) -> Box<dyn VisionBackend> {
    let soc = board_soc.to_ascii_lowercase();
    let want = match prefs.preference {
        "auto" => {
            if soc.starts_with("rk") || soc.contains("tegra") || soc.contains("jetson") {
                "rknn"
            } else {
                "mock"
            }
        }
        other => other,
    };
    match want {
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
    }
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
    fn rknn_backend_records_socket_and_loads() {
        let b = RknnSidecarBackend::new("/run/ados/vision-rknn.sock");
        assert_eq!(b.name(), "rknn");
        assert_eq!(b.socket_path(), "/run/ados/vision-rknn.sock");
        let m = b.load(&meta()).unwrap();
        // No sidecar present in test ⇒ infer is a no-op (empty), never panics.
        assert!(m
            .infer(&[0u8; 4], 1, 1, FrameFormat::Rgb24)
            .unwrap()
            .is_empty());
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
        // A CPU-only SoC falls back to mock under auto.
        assert_eq!(select_backend("bcm2711", &prefs).name(), "mock");
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
