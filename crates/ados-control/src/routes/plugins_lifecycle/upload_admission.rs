//! Upload admission for the archive routes: one archive in memory at a time,
//! and the `X-Content-SHA256` digest a digest-signed upload must match.

use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::super::jobs::JobSidecar;
use super::super::{json_response, Refusal};

/// The upload header carrying the archive's hex sha256. When an HMAC edge
/// signs a streaming route it signs this digest instead of the body, so the
/// route must prove the streamed bytes match it.
const CONTENT_SHA256_HEADER: &str = "x-content-sha256";

/// One archive in memory at a time across parse and install, multipart and
/// URL alike: each buffers up to the archive cap and then decompresses it, and
/// the unit's memory cap is sized for one. A second request is refused with
/// 429 rather than queued, so it cannot pin a connection holding its body.
#[cfg(not(test))]
static ARCHIVE_SLOTS: std::sync::LazyLock<Arc<Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(Semaphore::new(1)));

#[cfg(not(test))]
fn archive_slots() -> Arc<Semaphore> {
    ARCHIVE_SLOTS.clone()
}

// Tests run in parallel inside one process; each test thread gets its own
// slot so one test's upload never reads as another's concurrent install.
#[cfg(test)]
thread_local! {
    static ARCHIVE_SLOTS: Arc<Semaphore> = Arc::new(Semaphore::new(1));
}

#[cfg(test)]
fn archive_slots() -> Arc<Semaphore> {
    ARCHIVE_SLOTS.with(Arc::clone)
}

/// Take the archive slot, or `None` when another install holds it.
pub(super) fn try_archive_slot() -> Option<OwnedSemaphorePermit> {
    archive_slots().try_acquire_owned().ok()
}

/// The 429 answered while another archive is in flight.
pub(super) fn busy() -> Response {
    json_response(
        StatusCode::TOO_MANY_REQUESTS,
        &serde_json::json!({"error": "E_BUSY", "message": "another install is running"}),
    )
}

/// Why an upload was not read.
pub(super) enum UploadFault {
    /// A lifecycle refusal in the plugin envelope.
    Refused(Refusal),
    /// The streamed bytes do not hash to the signed `X-Content-SHA256`.
    DigestMismatch,
}

impl From<Refusal> for UploadFault {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

impl UploadFault {
    /// The response, after recording the failure on `job` when there is one.
    pub(super) fn respond(self, job: Option<&JobSidecar>) -> Response {
        match self {
            Self::Refused(r) => match job {
                Some(job) => job.fail(r).into_response(),
                None => r.into_response(),
            },
            Self::DigestMismatch => {
                if let Some(job) = job {
                    job.fail(Refusal::new(
                        10,
                        "digest_mismatch",
                        "archive sha256 did not match X-Content-SHA256",
                        StatusCode::UNAUTHORIZED,
                    ));
                }
                json_response(
                    StatusCode::UNAUTHORIZED,
                    &serde_json::json!({"error": "E_DIGEST_MISMATCH"}),
                )
            }
        }
    }
}

/// The lowercase hex digest from `X-Content-SHA256`, `None` when absent. A
/// present value that is not 64 hex digits is a malformed request.
pub(super) fn expected_digest(headers: &HeaderMap) -> Result<Option<String>, Refusal> {
    let Some(raw) = headers.get(CONTENT_SHA256_HEADER) else {
        return Ok(None);
    };
    let value = raw.to_str().unwrap_or_default().trim().to_ascii_lowercase();
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(Some(value))
    } else {
        Err(Refusal::usage(
            "usage_error",
            "X-Content-SHA256 must be 64 hex digits",
        ))
    }
}
