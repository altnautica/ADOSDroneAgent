//! Query API request/response types.

use super::*;

/// A read request against the query API. All fields are optional; an empty
/// request returns the most recent page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueryRequest {
    /// Inclusive lower bound, microsecond epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    /// Exclusive upper bound, microsecond epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<i64>,
    /// Restrict to these dotted metric keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric: Vec<String>,
    /// Restrict to these emitting sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source: Vec<String>,
    /// Minimum severity level (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    /// Free-text substring match against the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Restrict to one session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<i64>,
    /// Page size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Opaque keyset cursor returned by the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Pagination block: the cursor for the next page and the count in this page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Page {
    /// Opaque keyset cursor for the next page, absent when exhausted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Number of rows in this page.
    pub count: u32,
}

/// Response metadata: which plane served it, the envelope version, the server
/// time, and how far the writer is behind real time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    /// The serving plane (e.g. `local` for the unix socket, `lan` for TCP).
    pub source: String,
    /// Envelope version.
    pub v: u8,
    /// Server time, microsecond epoch.
    pub ts: i64,
    /// Writer lag behind real time, milliseconds. Zero when fully caught up.
    pub db_lag_ms: i64,
}

/// The success envelope shared by every read endpoint. `data` is the typed
/// payload (rows, buckets, sessions, stats), `page` carries pagination, `meta`
/// carries serving metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryResponse<T> {
    /// The result payload.
    pub data: T,
    /// Pagination block.
    pub page: Page,
    /// Serving metadata.
    pub meta: Meta,
}

/// The error body returned for a failed request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiError {
    /// Stable machine-readable error code.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

/// The error envelope: `{ "error": { "code", "message" } }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorEnvelope {
    /// The error body.
    pub error: ApiError,
}

impl ErrorEnvelope {
    /// Build an error envelope.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ApiError {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

// --- redaction ----------------------------------------------------------
