//! Error types for the plugin lifecycle controller.
//!
//! Mirrors the Python `ados.plugins.errors` hierarchy. [`SignatureError`]
//! carries a structured [`SignatureErrorKind`] so a caller (CLI / REST) can map
//! a failure to the right exit code without string matching, exactly as the
//! Python `SignatureError.kind` attribute is consumed.

use thiserror::Error;

/// Sub-classification of a signature failure. The string values are
/// byte-identical to the Python `KIND_*` constants so the wire and CLI exit
/// codes stay stable across the two implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureErrorKind {
    /// Archive is unsigned and the install path requires signed.
    Missing,
    /// Signature does not verify against the trusted key, or is malformed.
    Invalid,
    /// Signature verifies but the signer id is on the revocation list.
    Revoked,
    /// Signer id is not present in the trusted-keys store.
    UnknownSigner,
}

impl SignatureErrorKind {
    /// The stable string form, matching the Python `KIND_*` constants.
    pub fn as_str(self) -> &'static str {
        match self {
            SignatureErrorKind::Missing => "missing",
            SignatureErrorKind::Invalid => "invalid",
            SignatureErrorKind::Revoked => "revoked",
            SignatureErrorKind::UnknownSigner => "unknown_signer",
        }
    }
}

/// Raised when a manifest fails to load, parse, or validate.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ManifestError(pub String);

/// Raised on malformed `.adosplug` archives: bad zip, missing manifest,
/// path-traversal or symlink entries, oversized payload.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ArchiveError(pub String);

/// Raised when an archive signature is missing, malformed, invalid, revoked,
/// or signed by an unknown signer. Carries the [`SignatureErrorKind`].
#[derive(Debug, Error)]
#[error("{message}")]
pub struct SignatureError {
    pub kind: SignatureErrorKind,
    pub message: String,
}

impl SignatureError {
    pub fn new(kind: SignatureErrorKind, message: impl Into<String>) -> Self {
        SignatureError {
            kind,
            message: message.into(),
        }
    }
}

/// Raised on lifecycle transitions that are illegal or fail to apply:
/// enabling an uninstalled plugin, an incompatible version or board, an
/// `inprocess`/`inline` request from a non-first-party signer, a manifest-hash
/// mismatch, or a `systemctl` failure.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct SupervisorError(pub String);

/// Top-level lifecycle error: any of the above. The controller methods return
/// this so a caller can match on the concrete cause.
#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Signature(#[from] SignatureError),
    #[error(transparent)]
    Supervisor(#[from] SupervisorError),
    #[error("state io error: {0}")]
    Io(#[from] std::io::Error),
}
