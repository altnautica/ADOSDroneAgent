//! The backend the picker returns when the requested one cannot run here.

use super::*;

/// A requested backend that cannot exist in this build or on this board (the
/// ONNX runtime not compiled in, an unknown backend name). It keeps the engine
/// up (frames, the socket, plugin-side models) but loads nothing, so every
/// engine-run model registers without a backend and the status reports
/// `backend_state = "unavailable"` with [`VisionBackend::unavailable_reason`].
#[derive(Debug, Clone)]
pub struct UnavailableBackend {
    requested: String,
    reason: String,
}

impl UnavailableBackend {
    /// `requested` is the backend name the configuration asked for; `reason`
    /// says why it cannot run.
    pub fn new(requested: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            requested: requested.into(),
            reason: reason.into(),
        }
    }
}

impl VisionBackend for UnavailableBackend {
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Err(anyhow!(
            "vision backend {} unavailable, model {} not loaded: {}",
            self.requested,
            meta.id,
            self.reason
        ))
    }
    fn name(&self) -> &str {
        &self.requested
    }
    fn is_inference_capable(&self) -> bool {
        false
    }
    fn state(&self) -> BackendState {
        BackendState::Unavailable
    }
    fn unavailable_reason(&self) -> Option<&str> {
        Some(&self.reason)
    }
}
