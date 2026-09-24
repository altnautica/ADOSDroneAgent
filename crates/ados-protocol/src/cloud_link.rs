//! The cloud-link sidecar: whether this node's cloud relay is actually talking
//! to the cloud.
//!
//! `ados-cloud` rewrites it on every heartbeat tick with the MQTT broker
//! session state (the transport's ConnAck-driven flag, never "the task is
//! alive") and the outcome of the last `/agent/status` POST. `ados-control`
//! serves it at `GET /api/cloud/link` for the dashboard. A file not rewritten
//! within [`CLOUD_LINK_STALE_MS`] reads as absent, so a dead relay process never
//! leaves a frozen "connected" behind.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The cloud-link sidecar path (tmpfs, cleared on boot).
pub const CLOUD_LINK_SIDECAR: &str = "/run/ados/cloud-link.json";

/// The sidecar schema version, stamped on write and checked (warn-only) on read.
pub const CLOUD_LINK_SIDECAR_VERSION: u16 = 1;

/// Four heartbeat ticks (5 s each): past this the writer is gone.
pub const CLOUD_LINK_STALE_MS: i64 = 20_000;

/// The relay's live link state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CloudLink {
    /// The sidecar schema version (absent means `0`, an older writer).
    #[serde(default)]
    pub version: u16,
    /// The writer's clock at write time (epoch ms).
    pub generated_at_ms: Option<i64>,
    /// The node holds a pairing key.
    #[serde(default)]
    pub paired: bool,
    /// A cloud URL is configured. Without one the relay is local-only.
    #[serde(default)]
    pub cloud_url_set: bool,
    /// The broker session is confirmed up. `None` when no session is running
    /// (unpaired, local-only, or no broker in this posture).
    pub broker_connected: Option<bool>,
    /// When the last `/agent/status` POST was answered with a 2xx (epoch ms).
    pub last_heartbeat_ok_ms: Option<i64>,
    /// The HTTP status of the last `/agent/status` answer, 2xx or not.
    pub last_heartbeat_status: Option<u16>,
    /// The transport error of the last POST that got no answer at all.
    pub last_heartbeat_error: Option<String>,
}

/// Read the sidecar at `path`, or `None` when absent, unparseable, undated or
/// stale at `now_ms`.
pub fn read_cloud_link_from(path: &Path, now_ms: i64) -> Option<CloudLink> {
    let text = std::fs::read_to_string(path).ok()?;
    let link: CloudLink = serde_json::from_str(&text).ok()?;
    let generated = link.generated_at_ms?;
    // A future stamp is a clock fault, not a fresh file.
    if generated > now_ms || now_ms - generated > CLOUD_LINK_STALE_MS {
        return None;
    }
    crate::sidecar::check_sidecar_version("cloud-link", link.version, CLOUD_LINK_SIDECAR_VERSION);
    Some(link)
}

/// Read the sidecar from its default path.
pub fn read_cloud_link(now_ms: i64) -> Option<CloudLink> {
    read_cloud_link_from(Path::new(CLOUD_LINK_SIDECAR), now_ms)
}

/// Atomically write `link` to `path` (tmp + rename).
pub fn write_cloud_link_to(path: &Path, link: &CloudLink) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(link).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Atomically write `link` to the default path.
pub fn write_cloud_link(link: &CloudLink) -> std::io::Result<()> {
    write_cloud_link_to(Path::new(CLOUD_LINK_SIDECAR), link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_link_reads_back_and_a_stale_or_future_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-link.json");
        let now = 1_700_000_000_000i64;
        let link = CloudLink {
            version: CLOUD_LINK_SIDECAR_VERSION,
            generated_at_ms: Some(now),
            paired: true,
            cloud_url_set: true,
            broker_connected: Some(true),
            last_heartbeat_ok_ms: Some(now - 2_000),
            last_heartbeat_status: Some(200),
            last_heartbeat_error: None,
        };
        write_cloud_link_to(&path, &link).unwrap();
        assert_eq!(read_cloud_link_from(&path, now + 1_000), Some(link));
        assert!(read_cloud_link_from(&path, now + CLOUD_LINK_STALE_MS + 1).is_none());
        assert!(read_cloud_link_from(&path, now - 1).is_none());
    }
}
