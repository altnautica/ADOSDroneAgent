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
//! - [`SidecarBackend`] — an IPC client that forwards load + infer requests
//!   to the Python accelerator sidecar over `/run/ados/vision-rknn.sock` using
//!   the same 4-byte big-endian length-prefixed msgpack framing as the other
//!   agent sockets. The NPU vendor runtime (RKNN, or HailoRT on a
//!   Pi AI HAT) is reached only through that sidecar, never linked here — one
//!   sidecar client, the accelerator differs by socket + name.

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
}

// --- mock -----------------------------------------------------------------

mod mock;
#[cfg(feature = "onnx")]
mod onnx_backend;
mod sidecar;

use mock::mock_backend;
pub use mock::{mock_target_at, MockBackend, MockTargetBackend, MOCK_TARGET_TRACK_ID};
#[cfg(feature = "onnx")]
pub use onnx_backend::OnnxBackend;
pub use sidecar::SidecarBackend;

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
}

/// Whether the build carries the ONNX CPU backend.
const ONNX_COMPILED: bool = cfg!(feature = "onnx");

/// Pick the backend for a board.
///
/// "auto" resolves by SoC family: a Rockchip part with an NPU (`rk3576`,
/// `rk3588`, `rk3566`, ...) prefers the accelerator sidecar; any other board
/// (a Pi-class CPU-only SoC, or a Jetson, which has no accelerated backend)
/// prefers the ONNX CPU backend when the
/// binary was built with the `onnx` feature, and only falls back to the
/// detection-less mock when no runtime is available. An explicit preference is
/// honoured. The selection is logged at `warn` when it resolves to the mock so
/// an operator who enabled vision is not left with a silently-empty detection
/// stream and no signal that no real inference is running.
pub fn select_backend(board_soc: &str, prefs: &BackendPrefs) -> Box<dyn VisionBackend> {
    let soc = board_soc.to_ascii_lowercase();
    let want = match prefs.preference {
        "auto" => {
            if soc.starts_with("rk") {
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
        "rknn" => Box::new(SidecarBackend::new(prefs.rknn_socket_path.clone(), "rknn")),
        // The Pi 5 + AI HAT (Hailo-8) is selected explicitly, not by "auto": the HAT is a PCIe device,
        // not the SoC, so the SoC string (bcm2712) does not reveal it. The Python sidecar owns HailoRT.
        "hailo" => Box::new(SidecarBackend::new(
            prefs.hailo_socket_path.clone(),
            "hailo",
        )),
        "onnx" => {
            #[cfg(feature = "onnx")]
            {
                Box::new(OnnxBackend::new())
            }
            #[cfg(not(feature = "onnx"))]
            {
                tracing::warn!("onnx backend requested but not compiled in; using mock");
                mock_backend(prefs.mock_target)
            }
        }
        "mock" => mock_backend(prefs.mock_target),
        unknown => {
            tracing::warn!(backend = unknown, "unknown vision backend; using mock");
            mock_backend(prefs.mock_target)
        }
    };
    if !backend.is_inference_capable() {
        // Loud, not silent: the engine will run but no detection will come out
        // of it — either because the backend is the mock, or because the chosen
        // accelerator sidecar is not answering on its socket. The engine
        // surfaces the same fact through `is_inference_capable` on its status,
        // so `/api/status` reports the offload tier instead of claiming local
        // inference that cannot happen.
        tracing::warn!(
            soc = %soc,
            backend = backend.name(),
            preference = prefs.preference,
            onnx_compiled = ONNX_COMPILED,
            "vision backend cannot run inference: the detection stream will stay \
             empty until an accelerator sidecar answers, or the binary is built \
             with the onnx feature"
        );
    }
    backend
}

#[cfg(test)]
mod tests;
