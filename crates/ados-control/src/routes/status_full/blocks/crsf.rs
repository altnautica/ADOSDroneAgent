//! CRSF RC-lane block: the lane's state sidecar, mtime-staleness-gated.

use ados_protocol::paths;
use ados_protocol::paths::run_dir;
use ados_protocol::sidecar::read_fresh_at;
use serde_json::Map;
use serde_json::Value;
use std::path::Path;

/// The CRSF/ExpressLRS RC control-lane state sidecar filename under the run dir.
pub(crate) const CRSF_STATS_SIDECAR: &str = paths::CRSF_STATS_JSON;

/// A CRSF sidecar not re-written within this window reads as absent, so a dead
/// lane's lingering tmpfs file never keeps the status carrying a frozen state.
/// One window for every reader of the sidecar: the ground-station CRSF route's.
pub(crate) const CRSF_STATS_STALE: std::time::Duration = crate::routes::gs_crsf::CRSF_STATS_STALE;

/// The CRSF RC-lane status block for the consolidated body, or `None` (the key
/// omitted) when the lane service is not running, its sidecar is stale, or the
/// body is malformed — never a fabricated all-null block for a node without the
/// lane. PROFILE-AGNOSTIC by construction: it reads the sidecar, not the profile,
/// so a drone running the ELRS relay lane carries it exactly like a ground
/// station. The full sidecar field set is served verbatim (the richer set incl.
/// `flyable` + `pic` + `fc_command_down_gated` the cloud heartbeat projection
/// drops); the LAN path can carry it and the GCS reads those snake_case keys.
pub(crate) fn read_crsf_status() -> Option<Value> {
    read_crsf_status_in(
        &run_dir().join(CRSF_STATS_SIDECAR),
        std::time::SystemTime::now(),
    )
}

/// The path + now injectable core of [`read_crsf_status`], so a test drives the
/// mtime staleness gate deterministically against a tempdir. Absent / unreadable
/// / unparseable / non-object / stale / future-dated all read `None`. Emits the
/// shared best-effort version-drift warning (never rejects), then serves the
/// object verbatim.
pub(crate) fn read_crsf_status_in(path: &Path, now: std::time::SystemTime) -> Option<Value> {
    let doc = read_fresh_at::<Map<String, Value>>(path, Some(CRSF_STATS_STALE), now).fresh()?;
    let doc = Value::Object(doc);
    // Best-effort drift signal (never reject): warn on a producer/reader version
    // mismatch, then serve the sidecar anyway. The writer const lives in the
    // ados-crsf crate, so compare against the shared registry.
    let got = doc.get("v").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("crsf-stats") {
        ados_protocol::sidecar::check_sidecar_version("crsf-stats", got, ours);
    }
    Some(doc)
}
