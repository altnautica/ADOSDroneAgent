//! Inference backends.
//!
//! A [`VisionBackend`] loads a model into a [`LoadedModel`] that runs inference
//! on one frame at a time. Four backends share the trait:
//!
//! - [`MockBackend`] — returns no detections (or, with `vision.mock_target`, one
//!   synthetic target). Selected only by an explicit `backend: mock`, for unit
//!   tests and hardware-free benches; never a silent stand-in for a real one.
//! - `OnnxBackend` — an ONNX Runtime backend, compiled in only under the `onnx`
//!   cargo feature so the default build stays free of the heavy native runtime.
//! - [`SidecarBackend`] — an IPC client that forwards load + infer requests
//!   to the Python accelerator sidecar over `/run/ados/vision-rknn.sock` using
//!   the same 4-byte big-endian length-prefixed msgpack framing as the other
//!   agent sockets. The NPU vendor runtime (RKNN, or HailoRT on a
//!   Pi AI HAT) is reached only through that sidecar, never linked here — one
//!   sidecar client, the accelerator differs by socket + name.
//! - [`UnavailableBackend`] — what the picker returns when the requested real
//!   backend cannot exist in this build (ONNX not compiled in, an unknown
//!   name). It loads nothing, and the engine reports `backend_state =
//!   "unavailable"` with the reason instead of pretending to run inference.

use ados_protocol::framebus::{
    BoundingBox, Detection, DetectionHead, FrameFormat, LockState, ModelMetadata,
};
use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

    /// Extract a re-id appearance embedding from a pre-cropped, model-input-sized
    /// `crop` (the engine crops + resizes a detection box to this model's input
    /// before calling). Returns the embedding as a flat f32 vector, or `None`
    /// when this model is a detector / the backend cannot embed (the default).
    /// The caller L2-normalizes the result, so an embedder need not.
    fn embed(
        &self,
        _crop: &[u8],
        _width: u32,
        _height: u32,
        _format: FrameFormat,
    ) -> Result<Option<Vec<f32>>> {
        Ok(None)
    }

    /// The metadata the model actually runs with, when loading refined the
    /// registered one (a self-describing file's head, classes or input size;
    /// see [`model_props`]). `None` keeps the registered metadata.
    fn resolved_metadata(&self) -> Option<&ModelMetadata> {
        None
    }
}

/// What the selected backend can do right now, as `GET /api/vision/status`
/// reports it (`backend_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    /// A real backend that can run inference now.
    Ready,
    /// The explicitly configured mock: runs, but nothing it reports is measured.
    Mock,
    /// The requested backend cannot run inference: not compiled in, unknown,
    /// or its accelerator sidecar is not answering.
    Unavailable,
}

impl BackendState {
    /// The wire form: `ready` | `mock` | `unavailable`.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendState::Ready => "ready",
            BackendState::Mock => "mock",
            BackendState::Unavailable => "unavailable",
        }
    }
}

/// A backend that can load models of the kinds it supports.
pub trait VisionBackend: Send + Sync {
    /// Load `meta` into a runnable model.
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>>;
    /// Short backend name for logs and the registry (`mock`, `onnx`, `rknn`, `hailo`).
    fn name(&self) -> &str;
    /// Whether this backend is actually able to run inference right now. The
    /// mock backend returns no detections, and a sidecar backend whose sidecar
    /// is not answering returns none either; a vision engine wired to one
    /// produces a silently-empty detection stream, which `/api/status` must
    /// report as such rather than as a working accelerator. An in-process
    /// runtime that is linked in is always capable, so the default is `true`;
    /// a backend whose runtime lives somewhere that can be absent overrides.
    fn is_inference_capable(&self) -> bool {
        true
    }
    /// The operator-facing state. Derived from [`Self::is_inference_capable`]
    /// by default; the mock overrides it to [`BackendState::Mock`].
    fn state(&self) -> BackendState {
        if self.is_inference_capable() {
            BackendState::Ready
        } else {
            BackendState::Unavailable
        }
    }
    /// Why the backend cannot run inference, when the backend knows. `None`
    /// for a working backend (and for a sidecar, whose reason is its socket).
    fn unavailable_reason(&self) -> Option<&str> {
        None
    }
}

// --- mock -----------------------------------------------------------------

mod mock;
pub mod model_props;
#[cfg(feature = "onnx")]
mod onnx_backend;
mod sidecar;
mod unavailable;

use mock::mock_backend;
pub use mock::{mock_target_at, MockBackend, MockTargetBackend, MOCK_TARGET_TRACK_ID};
#[cfg(feature = "onnx")]
pub use onnx_backend::OnnxBackend;
pub use sidecar::SidecarBackend;
pub use unavailable::UnavailableBackend;

// --- picker ---------------------------------------------------------------

/// The minimal config view the picker needs (avoids a config dependency cycle).
pub struct BackendPrefs<'a> {
    /// Operator preference: "auto" | "mock" | "onnx" | "rknn" | "hailo".
    pub preference: &'a str,
    /// The accelerator sidecar socket path for the rknn backend.
    pub rknn_socket_path: String,
    /// The accelerator sidecar socket path for the hailo backend (Pi + AI HAT).
    pub hailo_socket_path: String,
    /// `vision.mock_target`: a resolved mock backend emits the orbiting
    /// synthetic target instead of no detections.
    pub mock_target: bool,
    /// `vision.detector.threads`: intra-op threads an ONNX detection session
    /// runs on (the re-id model always runs on one).
    pub onnx_threads: usize,
}

/// Whether the build carries the ONNX CPU backend.
const ONNX_COMPILED: bool = cfg!(feature = "onnx");

/// Pick the backend for a board.
///
/// "auto" resolves by SoC family: a Rockchip part with an NPU (`rk3576`,
/// `rk3588`, `rk3566`, ...) prefers the accelerator sidecar; any other board
/// (a Pi-class CPU-only SoC, or a Jetson, which has no accelerated backend)
/// takes the ONNX CPU backend. An explicit preference is honoured.
///
/// A requested real backend that cannot exist in this build (ONNX without the
/// `onnx` feature, an unknown name) resolves to an [`UnavailableBackend`], never
/// to the mock: the engine still runs, but it logs `vision_backend_unavailable`
/// at error level and reports `backend_state = "unavailable"` so an operator
/// who enabled vision sees that no inference is running. The mock is selected
/// only by an explicit `mock` preference.
pub fn select_backend(board_soc: &str, prefs: &BackendPrefs) -> Box<dyn VisionBackend> {
    let soc = board_soc.to_ascii_lowercase();
    let want = match prefs.preference {
        "auto" => {
            if soc.starts_with("rk") {
                "rknn"
            } else {
                "onnx"
            }
        }
        other => other,
    };
    let backend: Box<dyn VisionBackend> = match want {
        "rknn" => Box::new(SidecarBackend::new(prefs.rknn_socket_path.clone(), "rknn")),
        // The Pi 5 + AI HAT (Hailo-8) is selected explicitly, not by "auto": the HAT is a PCIe device,
        // not the SoC, so the SoC string (bcm2712) does not reveal it. The Python sidecar owns HailoRT.
        "hailo" => Box::new(SidecarBackend::new(
            prefs.hailo_socket_path.clone(),
            "hailo",
        )),
        "onnx" => onnx_backend_or_unavailable(prefs.onnx_threads),
        "mock" => mock_backend(prefs.mock_target),
        unknown => Box::new(UnavailableBackend::new(
            unknown,
            format!("unknown vision backend {unknown:?}"),
        )),
    };
    match backend.state() {
        BackendState::Ready => {}
        BackendState::Mock => tracing::warn!(
            soc = %soc,
            mock_target = prefs.mock_target,
            "vision backend is the configured mock: detections are synthetic or empty"
        ),
        // Loud, not silent: the engine runs but no detection will come out of
        // it. The same state rides on the engine's status reply, so
        // `/api/vision/status` says `unavailable` and `/api/status` reports the
        // offload tier instead of claiming local inference that cannot happen.
        BackendState::Unavailable => tracing::error!(
            event = "vision_backend_unavailable",
            soc = %soc,
            backend = backend.name(),
            preference = prefs.preference,
            onnx_compiled = ONNX_COMPILED,
            reason = backend
                .unavailable_reason()
                .unwrap_or("the accelerator sidecar is not answering on its socket"),
            "vision_backend_unavailable"
        ),
    }
    backend
}

/// The ONNX CPU backend when the build carries it, else an unavailable backend
/// that says so.
fn onnx_backend_or_unavailable(
    #[allow(unused_variables)] threads: usize,
) -> Box<dyn VisionBackend> {
    #[cfg(feature = "onnx")]
    {
        Box::new(OnnxBackend::new(threads))
    }
    #[cfg(not(feature = "onnx"))]
    {
        Box::new(UnavailableBackend::new(
            "onnx",
            "this build does not include the onnx inference runtime (cargo feature `onnx`)",
        ))
    }
}

#[cfg(test)]
mod tests;
