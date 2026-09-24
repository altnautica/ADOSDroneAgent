//! `GET /api/cloud/link` — whether the cloud relay is actually talking to the
//! cloud.
//!
//! Serves the cloud-link sidecar `ados-cloud` rewrites every heartbeat tick:
//! the broker session (confirmed by ConnAck, not by a live task) and the last
//! `/agent/status` POST outcome. An absent or stale sidecar is a `404`: the
//! relay process is not running, so nothing is reported rather than a frozen
//! "connected".

use std::path::Path;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use ados_protocol::cloud_link::{read_cloud_link_from, CLOUD_LINK_SIDECAR};

use crate::routes::detail;

/// `GET /api/cloud/link` → the cloud relay's live link state.
pub async fn get_cloud_link() -> Response {
    cloud_link_at(Path::new(CLOUD_LINK_SIDECAR), now_epoch_ms())
}

fn cloud_link_at(path: &Path, now_ms: i64) -> Response {
    match read_cloud_link_from(path, now_ms) {
        Some(link) => (StatusCode::OK, Json(link)).into_response(),
        None => detail(
            StatusCode::NOT_FOUND,
            "the cloud relay is not reporting (service stopped or not yet started)".to_string(),
        ),
    }
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::cloud_link::{write_cloud_link_to, CloudLink, CLOUD_LINK_STALE_MS};

    #[test]
    fn a_relay_that_stopped_writing_reads_as_not_reporting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-link.json");
        let now = 1_700_000_000_000i64;
        let link = CloudLink {
            version: 1,
            generated_at_ms: Some(now),
            paired: true,
            cloud_url_set: true,
            broker_connected: Some(true),
            ..CloudLink::default()
        };
        write_cloud_link_to(&path, &link).unwrap();
        assert_eq!(cloud_link_at(&path, now + 1_000).status(), StatusCode::OK);
        assert_eq!(
            cloud_link_at(&path, now + CLOUD_LINK_STALE_MS + 1).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            cloud_link_at(&dir.path().join("absent.json"), now).status(),
            StatusCode::NOT_FOUND
        );
    }
}
