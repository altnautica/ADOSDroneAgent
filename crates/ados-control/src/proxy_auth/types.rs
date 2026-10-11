//! The typed request headers, the decision outcome and its fixed messages.

use super::*;

/// True when `value` is a SHA-256 hex digest (64 hex characters).
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The headers the decision reads off a request, captured before the body is
/// touched so the decision is a pure function of typed inputs (the caller pulls
/// them out of the live `HeaderMap` once).
#[derive(Debug, Clone, Default)]
pub struct RequestHeaders {
    pub origin: Option<String>,
    pub referer: Option<String>,
    pub host: Option<String>,
    pub x_ados_key: Option<String>,
    pub x_ados_setup_token: Option<String>,
    /// The dashboard-access session token (`X-ADOS-Dashboard-Session`). Not read
    /// by [`ProxiedAuth::decide_api_key`] itself — the serve edge consults it to
    /// reverse a rejection, so the session is an alternative credential on proxied
    /// routes too.
    pub x_ados_dashboard_session: Option<String>,
    pub x_timestamp: Option<String>,
    pub x_nonce: Option<String>,
    pub x_hmac_signature: Option<String>,
}

/// The outcome of the proxied-route auth decision. `Accept` forwards the
/// request to the residual upstream; `Reject` is turned into a response by the
/// caller with the exact status + `{"detail"|"error": ...}` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Forward the request to the proxy upstream.
    Accept,
    /// Stop and answer with this status and JSON body.
    Reject {
        status: StatusCode,
        /// The error message text. `field` selects whether it renders under the
        /// `detail` key (the API-key middleware) or `error` (the HMAC
        /// middleware), matching each Python middleware's body shape.
        field: BodyField,
        message: &'static str,
    },
}

/// Which JSON key the rejection body renders the message under, mirroring the
/// two Python middlewares: the API-key middleware uses `{"detail": ...}`, the
/// HMAC middleware uses `{"error": ...}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyField {
    Detail,
    Error,
}

/// The fixed message strings, kept verbatim from the Python so a GCS reading the
/// body sees identical text against either surface.
pub(super) mod messages {
    pub const CLOUD_POSTURE: &str = "This setup route changes cloud posture and \
requires the API key (X-ADOS-Key) or the setup token (X-ADOS-Setup-Token, \
printed by `ados status`).";
    pub const SETUP_TOKEN: &str = "Missing or invalid X-ADOS-Setup-Token header. \
Setup token is printed by the local CLI.";
    pub const MISSING_KEY: &str = "Missing X-ADOS-Key header. This agent is paired \
and requires authentication. Run `ados status` on the agent to print the API \
key, then enter it in the dashboard.";
    pub const INVALID_KEY: &str = "Invalid API key";
    pub const MISSING_SECURITY_HEADERS: &str =
        "Missing security headers (X-Timestamp, X-Nonce, X-HMAC-Signature)";
    pub const BAD_TIMESTAMP: &str = "Invalid X-Timestamp format";
    pub const REPLAY: &str = "Request rejected (replay detected or timestamp expired)";
    pub const INVALID_SIGNATURE: &str = "Invalid HMAC signature";
}
