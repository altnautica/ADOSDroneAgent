//! Constructors and trait impls for the recoverable bind failure.

use super::BindError;
use crate::bind::bind_event::BindFailReason;

impl BindError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            phase: None,
            reason: None,
        }
    }

    pub fn with_phase(message: impl Into<String>, phase: &str) -> Self {
        Self {
            message: message.into(),
            phase: Some(phase.to_string()),
            reason: None,
        }
    }

    /// Build a failure that carries its precise, pre-classified cause. The
    /// emitter surfaces this `reason` verbatim rather than re-deriving it from
    /// the message, so a gate that already knows exactly why it refused to pair
    /// reports that cause honestly.
    pub fn with_reason(message: impl Into<String>, phase: &str, reason: BindFailReason) -> Self {
        Self {
            message: message.into(),
            phase: Some(phase.to_string()),
            reason: Some(reason),
        }
    }
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BindError {}
