//! Transport errors.

/// A failure sending an Atlas event over a bearer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The bearer's channel/socket is closed (the receiver is gone).
    #[error("transport closed")]
    Closed,
    /// The bearer is not currently available (no carrier / unreachable).
    #[error("bearer unavailable")]
    Unavailable,
    /// No bearer in the ladder could carry the event.
    #[error("no available bearer")]
    NoBearer,
    /// The peer returned a non-success HTTP status.
    #[error("http status {0}")]
    Http(u16),
    /// The HTTP request itself failed (connect / timeout).
    #[error("request: {0}")]
    Request(String),
    /// The event failed to (de)serialize to msgpack.
    #[error("encode: {0}")]
    Encode(String),
}

impl TransportError {
    /// Whether another bearer is worth trying for this failure. A transient
    /// carrier failure (unreachable, timed out, a 5xx) is retriable: the ladder
    /// falls over to the next bearer. A client error (a 4xx — the event itself
    /// is malformed or too large) is NOT retriable: every bearer would reject
    /// the same payload, so the ladder returns it instead of burning down to the
    /// cloud lane on every send. An encode failure is the local event's fault,
    /// never retriable.
    pub fn is_retriable(&self) -> bool {
        match self {
            TransportError::Http(code) => !(400..500).contains(code),
            TransportError::Encode(_) => false,
            TransportError::Closed
            | TransportError::Unavailable
            | TransportError::NoBearer
            | TransportError::Request(_) => true,
        }
    }
}

impl From<rmp_serde::encode::Error> for TransportError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        TransportError::Encode(e.to_string())
    }
}
