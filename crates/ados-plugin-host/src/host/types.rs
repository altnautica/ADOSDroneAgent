//! The result and error types every host method returns.

use rmpv::Value;

use crate::dispatch::errors;

/// A msgpack map the dispatcher returns to the plugin as the response `args`.
pub type HostResult = Value;

/// The receiving end of an auxiliary-stream push: `(channel, datagram)` pairs.
pub type AuxDatagramRx = tokio::sync::broadcast::Receiver<(u8, Vec<u8>)>;

/// A soft host-method failure that becomes the response envelope `error` field.
///
/// The three failure kinds the dispatch loop converts to the wire `error`
/// string, which the SDK parses back: a plain RPC error (the
/// message verbatim), `CapabilityDenied` (`capability_denied: <cap>`, the
/// inline pose-inject / VIO-component / driver-kind / component-kind gates),
/// and `AllowlistViolation` (`allowlist_violation: <basename>`). [`body`](Self::body)
/// renders the exact wire string for each, so a Rust host is byte-identical to
/// the Python host. A real host returns these; [`super::NoopHost`] never does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// An arbitrary handler failure; the body is the message verbatim.
    Rpc(String),
    /// An inline capability gate refused the call; the body renders
    /// `capability_denied: <cap>` (the stored string is the capability).
    CapabilityDenied(String),
    /// A `process.spawn` basename outside the manifest allowlist; the body
    /// renders `allowlist_violation: <basename>` (the stored string is the
    /// basename).
    AllowlistViolation(String),
}

impl HostError {
    /// The exact wire `error` body the Python server emits for this failure.
    pub fn body(&self) -> String {
        match self {
            HostError::Rpc(msg) => msg.clone(),
            HostError::CapabilityDenied(cap) => errors::capability_denied(cap),
            HostError::AllowlistViolation(basename) => errors::allowlist_violation(basename),
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.body())
    }
}

impl std::error::Error for HostError {}

/// Build the `{"error": "not_implemented", "method": <method>}` result the
/// Python `_handle_*` stub bodies return verbatim, so a plugin sees the same
/// shape from the Rust host as it did from the Python host.
pub fn not_implemented(method: &str) -> HostResult {
    Value::Map(vec![
        (Value::from("error"), Value::from("not_implemented")),
        (Value::from("method"), Value::from(method)),
    ])
}
