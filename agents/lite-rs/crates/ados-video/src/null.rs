//! No-op encoder used when the running board has no supported video
//! pipeline. Returned by [`crate::encoder_for_board`] for boards whose
//! `video.encoder_api_lite` is `"none"` or an unrecognized value.
//!
//! `start()` returns [`EncoderError::NotImplemented`] so the caller can
//! distinguish "video deliberately disabled on this board" from a real
//! encoder fault.

use crate::{EncodedFrame, Encoder, EncoderConfig, EncoderError};

/// Stub encoder. Always reports not-implemented; never produces frames.
#[derive(Debug, Default)]
pub struct NullEncoder {
    _private: (),
}

impl NullEncoder {
    /// Build a fresh null encoder.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl Encoder for NullEncoder {
    async fn start(&mut self, _config: EncoderConfig) -> Result<(), EncoderError> {
        tracing::debug!("null encoder start called; video subsystem disabled on this board");
        Err(EncoderError::NotImplemented)
    }

    async fn next_frame(&mut self) -> Option<EncodedFrame> {
        None
    }

    async fn stop(&mut self) {
        // Nothing to release; the null encoder holds no state.
    }
}
