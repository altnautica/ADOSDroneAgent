//! The perception offload-link sidecar: the drone's live "a workstation is
//! paired + reachable, and I am offloading to it" state.
//!
//! The offload reconciler (on the drone) writes this file each tick; the status
//! surfaces read it staleness-gated and feed a REAL `compute_node_paired` /
//! `bearer_acceptable` into [`crate`]'s tier decision, so an NPU-less drone with
//! a reachable workstation reports (and runs) tier `offload` instead of the
//! hardcoded "no node" default. Absent / stale ⇒ no offload link ⇒ the drone
//! reports `none` (honest, operating rule 44 — never a fabricated paired node).
//!
//! Producer: the `ados-cloud` offload reconciler. Consumers: `ados-control`
//! (`/api/status`) and `ados-cloud` (the cloud heartbeat). Single writer, many
//! readers — the same sidecar shape as compute-heartbeat / atlas-forward.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The offload-link sidecar path (tmpfs, cleared on boot).
pub const OFFLOAD_LINK_SIDECAR: &str = "/run/ados/offload-link.json";

/// The sidecar schema version, stamped on write and checked (warn-only) on read.
pub const OFFLOAD_LINK_SIDECAR_VERSION: u16 = 1;

/// A link file not re-written within this window is treated as absent, so a dead
/// / hung reconciler (whose tmpfs file persists) never keeps a drone reporting a
/// frozen `offload` tier after the workstation is gone (operating rule 44). 4x
/// the reconciler's ~5 s tick.
pub const OFFLOAD_LINK_STALE_MS: i64 = 20_000;

/// The drone's live offload-link state, written by the reconciler each tick.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OffloadLink {
    /// The sidecar schema version (absent ⇒ `0` from an older writer).
    #[serde(default)]
    pub version: u16,
    /// The reconciler's write time (epoch ms); absent/stale ⇒ treated as gone.
    pub generated_at_ms: Option<i64>,
    /// A `profile=workstation` node resolved + is reachable this tick.
    #[serde(default)]
    pub paired: bool,
    /// The LAN bearer to that node is healthy enough to offload over.
    #[serde(default)]
    pub bearer_acceptable: bool,
    /// The node's job-API address (`host:port`) the drone is offloading to.
    pub target: Option<String>,
    /// The node's device id, when discovery reported one.
    pub device_id: Option<String>,
    /// The detector model id the offload session runs (labels the source).
    pub model_id: Option<String>,
}

impl OffloadLink {
    /// A link stamped at the current schema version + `now_ms` write time. The
    /// reconciler builds one of these each tick and writes it; a not-paired tick
    /// writes `paired: false` (an honest "no link right now") rather than leaving
    /// a stale paired file behind.
    pub fn stamped(
        paired: bool,
        bearer_acceptable: bool,
        target: Option<String>,
        device_id: Option<String>,
        model_id: Option<String>,
        now_ms: i64,
    ) -> Self {
        Self {
            version: OFFLOAD_LINK_SIDECAR_VERSION,
            generated_at_ms: Some(now_ms),
            paired,
            bearer_acceptable,
            target,
            device_id,
            model_id,
        }
    }

    /// Whether this link should count as an offload path for the tier decision:
    /// a paired node on an acceptable bearer. (The staleness gate is applied by
    /// the reader, so a link this method sees is already fresh.)
    pub fn is_offload_path(&self) -> bool {
        self.paired && self.bearer_acceptable
    }
}

/// Read + parse the offload-link sidecar at `path`, or `None` when it is absent,
/// unparseable, missing its write-time, or STALE (older than
/// [`OFFLOAD_LINK_STALE_MS`] at `now_ms`). `now_ms` is injected so the staleness
/// gate is testable without touching the clock.
pub fn read_offload_link_from(path: &Path, now_ms: i64) -> Option<OffloadLink> {
    let text = std::fs::read_to_string(path).ok()?;
    let link: OffloadLink = serde_json::from_str(&text).ok()?;
    match link.generated_at_ms {
        Some(gen) if now_ms.saturating_sub(gen) <= OFFLOAD_LINK_STALE_MS => {
            // Warn (never reject) on a producer/reader version mismatch, then
            // fold the link in anyway — the same best-effort drift signal the
            // other sidecars use.
            crate::sidecar::check_sidecar_version(
                "offload-link",
                link.version,
                OFFLOAD_LINK_SIDECAR_VERSION,
            );
            Some(link)
        }
        _ => None,
    }
}

/// Read the offload-link sidecar from the default path, staleness-gated at
/// `now_ms`.
pub fn read_offload_link(now_ms: i64) -> Option<OffloadLink> {
    read_offload_link_from(Path::new(OFFLOAD_LINK_SIDECAR), now_ms)
}

/// Atomically write `link` to `path` (tmp + rename), creating the parent dir if
/// needed. The reconciler stamps the link ([`OffloadLink::stamped`]) before
/// calling this.
pub fn write_offload_link_to(path: &Path, link: &OffloadLink) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(link).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Atomically write `link` to the default sidecar path.
pub fn write_offload_link(link: &OffloadLink) -> std::io::Result<()> {
    write_offload_link_to(Path::new(OFFLOAD_LINK_SIDECAR), link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_paired_link_round_trips_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("ados-offload-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("offload-link.json");
        let now = 1_700_000_000_000i64;
        let link = OffloadLink::stamped(
            true,
            true,
            Some("192.168.1.5:8092".into()),
            Some("dev-abc".into()),
            Some("coco-yolov8n".into()),
            now,
        );
        write_offload_link_to(&path, &link).unwrap();
        // No leftover temp file.
        assert!(!dir.join("offload-link.json.tmp").exists());

        let back = read_offload_link_from(&path, now + 1_000).unwrap();
        assert_eq!(back, link);
        assert!(back.is_offload_path());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_link_reads_as_absent() {
        let dir = std::env::temp_dir().join(format!("ados-offload-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("offload-link.json");
        let gen = 1_700_000_000_000i64;
        let link = OffloadLink::stamped(true, true, Some("h:8092".into()), None, None, gen);
        write_offload_link_to(&path, &link).unwrap();

        // Just inside the window ⇒ present; just past it ⇒ absent.
        assert!(read_offload_link_from(&path, gen + OFFLOAD_LINK_STALE_MS).is_some());
        assert!(read_offload_link_from(&path, gen + OFFLOAD_LINK_STALE_MS + 1).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absent_file_reads_as_none() {
        let p = Path::new("/run/ados/does-not-exist-offload-link.json");
        assert!(read_offload_link_from(p, 1_700_000_000_000).is_none());
    }

    #[test]
    fn a_not_paired_link_is_not_an_offload_path() {
        let link = OffloadLink::stamped(false, false, None, None, None, 1);
        assert!(!link.is_offload_path());
    }
}
