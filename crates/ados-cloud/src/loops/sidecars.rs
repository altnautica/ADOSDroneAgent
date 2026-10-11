//! The `/run/ados` sidecar readers the heartbeat loop folds into the native
//! payload: video streams, linked peers, CRSF and WFB stats, plugin state and
//! config errors. Each reader treats an absent, malformed or stale file as
//! unmeasured.

use std::time::Duration;

use crate::heartbeat::{ConfigErrorEntry, CrsfBlock, LinkedPeerHb, RadioBlock, VideoStreamHb};

pub(crate) const VIDEO_STREAMS_SIDECAR: &str = "/run/ados/video-streams.json";

/// A video-streams sidecar not re-stamped within this window is treated as
/// absent, so a stopped pipeline's lingering tmpfs file stops advertising dead
/// legs. 4x the ~5 s healthy-tick re-stamp cadence.
pub(crate) const VIDEO_STREAMS_STALE_MS: i64 = 20_000;

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct VideoStreamsSidecarDoc {
    #[serde(default)]
    updated_at_unix: f64,
    #[serde(default)]
    streams: Vec<VideoStreamHb>,
}

/// Read + parse the video-streams sidecar, folding its per-leg list onto the
/// heartbeat so a cloud-relayed multi-stream node's legs reach the GCS switcher.
/// `None` when absent, unparseable, empty, or STALE.
///
/// Each leg's `whepUrl` is DERIVED here as the same-origin relative path the
/// agent's media proxy actually serves (`/whep?camera=<id>`), never read from
/// the sidecar and never an absolute `host:port`. The contract this replaces
/// told the GCS to build `:8889/<id>/whep` against the node host: a port the
/// agent does not proxy, unreachable off-LAN, mixed content under an HTTPS
/// ground station, and bypassing the media-auth gate the cockpit relies on.
pub(crate) fn read_video_streams_sidecar_from(
    path: &std::path::Path,
    now_ms: i64,
) -> Option<Vec<VideoStreamHb>> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: VideoStreamsSidecarDoc = serde_json::from_str(&text).ok()?;
    let gen_ms = (doc.updated_at_unix * 1000.0) as i64;
    if gen_ms > 0
        && now_ms.saturating_sub(gen_ms) <= VIDEO_STREAMS_STALE_MS
        && !doc.streams.is_empty()
    {
        Some(
            doc.streams
                .into_iter()
                .map(|mut leg| {
                    leg.whep_url = Some(format!("/whep?camera={}", leg.id));
                    leg
                })
                .collect(),
        )
    } else {
        None
    }
}

pub(crate) fn read_video_streams_sidecar(now_ms: i64) -> Option<Vec<VideoStreamHb>> {
    read_video_streams_sidecar_from(std::path::Path::new(VIDEO_STREAMS_SIDECAR), now_ms)
}

pub(crate) const LINKED_PEERS_SIDECAR: &str = "/run/ados/linked-peers.json";

/// A linked-peer whose last beacon is older than this is dropped, matching the
/// listener's 25 s prune window (`LINKED_PEER_STALE_AFTER_S`, the radio's own
/// peer-stale threshold). Per-entry gating also covers the dead-writer case: a
/// stale file's entries are all old, so the whole list reads absent rather than
/// republishing ghost peers.
pub(crate) const LINKED_PEER_STALE_MS: i64 = 25_000;

/// One raw peer row as the `linked-peers.json` sidecar writes it (snake_case,
/// the receive listener's `LinkedPeer`). Deserialized then remapped to the
/// camelCase [`LinkedPeerHb`] wire shape; the sidecar's `version` /
/// `wall_time_unix` header keys are ignored (unknown fields).
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct LinkedPeersSidecarDoc {
    #[serde(default)]
    peers: Vec<LinkedPeerRow>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct LinkedPeerRow {
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    channel: u8,
    #[serde(default)]
    rssi_dbm: i8,
    #[serde(default)]
    last_seen_unix: f64,
}

/// Read + parse the linked-peers sidecar, folding the fresh peer set onto the
/// heartbeat as `linkedPeers[]` so a GCS paired only to this ground node can
/// transitively enrol each relayed drone. Each entry is freshness-gated on the
/// listener's prune window; `None` when absent, unparseable, or every entry is
/// stale/id-less.
pub(crate) fn read_linked_peers_sidecar_from(
    path: &std::path::Path,
    now_ms: i64,
) -> Option<Vec<LinkedPeerHb>> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: LinkedPeersSidecarDoc = serde_json::from_str(&text).ok()?;
    let out: Vec<LinkedPeerHb> = doc
        .peers
        .into_iter()
        .filter(|p| !p.device_id.is_empty())
        .filter(|p| {
            let seen_ms = (p.last_seen_unix * 1000.0) as i64;
            seen_ms > 0 && now_ms.saturating_sub(seen_ms) <= LINKED_PEER_STALE_MS
        })
        .map(|p| LinkedPeerHb {
            device_id: p.device_id,
            role: p.role,
            channel: p.channel,
            rssi_dbm: p.rssi_dbm,
            seen_at_unix: p.last_seen_unix,
        })
        .collect();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub(crate) fn read_linked_peers_sidecar(now_ms: i64) -> Option<Vec<LinkedPeerHb>> {
    read_linked_peers_sidecar_from(std::path::Path::new(LINKED_PEERS_SIDECAR), now_ms)
}

/// The CRSF RC-lane state sidecar written by `ados-crsf` (~1 Hz while running,
/// a 5 s keep-alive while idling). Absent on a node without the lane.
pub(crate) const CRSF_STATS_SIDECAR: &str = "/run/ados/crsf-stats.json";

/// A CRSF sidecar not re-written within this window is treated as absent, so a
/// dead lane service's lingering tmpfs file never keeps the heartbeat carrying
/// a frozen lane state. The sidecar body carries no write
/// time, so the gate keys on the file mtime (the plugin-state precedent);
/// double the lane's slowest (idle keep-alive) rewrite cadence.
pub(crate) const CRSF_STATS_STALE: Duration = Duration::from_secs(10);

/// Read + parse the CRSF lane sidecar at `path`, mtime-staleness-gated against
/// `now`. `None` when the file is absent, unreadable, unparseable, or stale —
/// the heartbeat then omits the `crsf` block entirely (never an all-null
/// fabrication). A future mtime (clock skew) counts as fresh. The sidecar's
/// own `v` field feeds the shared best-effort version-drift warning.
pub(crate) fn read_crsf_sidecar_from(
    path: &std::path::Path,
    now: std::time::SystemTime,
) -> Option<CrsfBlock> {
    let fresh = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|mtime| {
            now.duration_since(mtime)
                .map(|age| age <= CRSF_STATS_STALE)
                .unwrap_or(true)
        })
        .unwrap_or(false);
    if !fresh {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let block: CrsfBlock = serde_json::from_str(&text).ok()?;
    if let (Some(got), Some(expected)) = (
        block.v,
        ados_protocol::contracts::sidecar_version("crsf-stats"),
    ) {
        ados_protocol::sidecar::check_sidecar_version("crsf-stats", got as u16, expected);
    }
    Some(block)
}

pub(crate) fn read_crsf_sidecar() -> Option<CrsfBlock> {
    read_crsf_sidecar_from(
        std::path::Path::new(CRSF_STATS_SIDECAR),
        std::time::SystemTime::now(),
    )
}

/// The WFB link-stats sidecar the radio writes ~1 Hz. The LAN status routes read
/// the same file; this is the cloud transport's copy of that read.
pub(crate) const WFB_STATS_SIDECAR: &str = "/run/ados/wfb-stats.json";

/// A stats sidecar not re-written within this window is treated as absent, the
/// same ceiling the LAN route applies (`WFB_STALE_AGE_S`). A frozen snapshot
/// renders a dead link as if it were live, and the body
/// carries no write time, so the gate keys on the file mtime.
pub(crate) const WFB_STATS_STALE: Duration = Duration::from_secs(10);

/// The radio block for the heartbeat, derived from the WFB stats sidecar at
/// `path` and mtime-staleness-gated against `now`.
///
/// This closes the gap where the cloud heartbeat hardcoded `RadioBlock::absent()`
/// forever: the data was on disk and the LAN routes read it fine, so a
/// cloud-relayed node rendered the absent skeleton in the GCS link card while its
/// LAN-paired twin showed a live link.
///
/// The derivation is `ados_protocol::wfb_status`'s — the SAME one both LAN routes
/// call — and the block is produced by deserializing its output rather than by a
/// second hand-written projection. A parallel derivation of the same 40 fields is
/// exactly the fork class that cost this codebase a shipped regression.
///
/// `None` when the file is absent, unreadable, stale, or does not deserialize
/// into the block; the caller then emits the honest absent skeleton. Never a
/// partially-populated block.
pub(crate) fn read_radio_sidecar_from(
    path: &std::path::Path,
    now: std::time::SystemTime,
) -> Option<RadioBlock> {
    let fresh = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|mtime| {
            now.duration_since(mtime)
                .map(|age| age <= WFB_STATS_STALE)
                .unwrap_or(true)
        })
        .unwrap_or(false);
    if !fresh {
        return None;
    }

    let cfg = ados_protocol::wfb_status::WfbStatusConfig::load(&crate::config::config_path());
    let status = ados_protocol::wfb_status::build_status_from_stats_file_at(&cfg, path);
    let status_map = status.as_object()?;
    let block = ados_protocol::wfb_status::build_radio_block(Some(status_map));

    match serde_json::from_value::<RadioBlock>(block) {
        Ok(b) => Some(b),
        Err(e) => {
            // A type the block declares differently (a float where the struct
            // says i64) must not ship half a reading. Say so loudly once per
            // tick and fall back to the absent skeleton.
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "wfb_stats_sidecar_did_not_match_the_radio_block_shape"
            );
            None
        }
    }
}

pub(crate) fn read_radio_sidecar() -> Option<RadioBlock> {
    read_radio_sidecar_from(
        std::path::Path::new(WFB_STATS_SIDECAR),
        std::time::SystemTime::now(),
    )
}

/// Local epoch ms for the staleness gate.
pub(crate) fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The directory every plugin writes its own state sidecar into
/// (`<id>-state.json`) through the plugin host. The heartbeat ferries each
/// slice opaquely under `pluginState[<id>]`.
pub(crate) const PLUGIN_STATE_DIR: &str = "/run/ados/plugins";

/// A plugin sidecar not re-written within this window is treated as absent, so a
/// dead/hung producer (whose tmpfs file persists) never makes the relay fold a
/// frozen-but-live slice forever. Mirrors the on-box
/// `/api/plugins/{id}/state` 10 s gate, with a little slack.
pub(crate) const PLUGIN_STATE_STALE: Duration = Duration::from_secs(15);

/// Read every fresh plugin/feature state sidecar in `dir` into a map keyed by id
/// (the filename minus `-state.json`), each value the sidecar's JSON verbatim.
/// The core never inspects a slice's shape — each plugin owns + validates its
/// own. Staleness is gated on the file mtime (uniform across every producer's
/// sidecar format, whatever its payload looks like), so a producer that stopped
/// writing drops out. An absent dir (a non-plugin device) yields an empty map.
/// `now` is the reference instant the mtime ages against (injected for tests).
pub(crate) fn read_plugin_state_sidecars_from(
    dir: &std::path::Path,
    now: std::time::SystemTime,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix("-state.json"))
        else {
            continue;
        };
        // mtime staleness gate. A future mtime (clock skew) counts as fresh.
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|mtime| {
                now.duration_since(mtime)
                    .map(|age| age <= PLUGIN_STATE_STALE)
                    .unwrap_or(true)
            })
            .unwrap_or(false);
        if !fresh {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        out.insert(id.to_string(), val);
    }
    out
}

pub(crate) fn read_plugin_state_sidecars() -> serde_json::Map<String, serde_json::Value> {
    read_plugin_state_sidecars_from(
        std::path::Path::new(PLUGIN_STATE_DIR),
        std::time::SystemTime::now(),
    )
}

/// The directory services publish their config-status sidecar into
/// (`config-status-<service>.json`) at startup. Honors the `ADOS_RUN_DIR`
/// override (default `/run/ados`), matching the `ados_config::write_config_status`
/// writer, so a redirected runtime layout (a non-root dev host or a test) reads
/// the same dir it wrote.
pub(crate) fn config_status_dir() -> std::path::PathBuf {
    std::env::var_os("ADOS_RUN_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/run/ados"))
}

/// The readable slice of a `config-status-<service>.json` sidecar. The writer
/// also stamps `generated_at_ms`; the heartbeat only needs the service label and
/// its current error.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct ConfigStatusSidecar {
    /// The sidecar schema version. `#[serde(default)]` makes a file written by an
    /// older agent (no `version` key) read back as `0`, a best-effort drift
    /// signal rather than a parse failure.
    #[serde(default)]
    version: u16,
    service: Option<String>,
    error: Option<String>,
}

/// Read every `config-status-<service>.json` sidecar in `dir` and collect the
/// ones whose current error is non-null into a `{service, error}` list. A service
/// with a valid config publishes `error: null` and is omitted, so the list
/// carries only LIVE config faults. The atomic writer's `.json.tmp.<pid>` staging
/// files do not end in `.json`, so they are skipped, as is any unrelated file.
/// The result is sorted by service so the wire is deterministic (`read_dir` order
/// is unspecified). An absent dir (a node with no config-status sidecars) yields
/// an empty vec.
pub(crate) fn read_config_error_sidecars_from(dir: &std::path::Path) -> Vec<ConfigErrorEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_status = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("config-status-") && n.ends_with(".json"))
            .unwrap_or(false);
        if !is_status {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(status) = serde_json::from_str::<ConfigStatusSidecar>(&text) else {
            continue;
        };
        // Best-effort schema-drift signal: an older file (version 0) warns but is
        // still used. Never a reject.
        ados_protocol::sidecar::check_sidecar_version(
            "config-status",
            status.version,
            ados_config::CONFIG_STATUS_SIDECAR_VERSION,
        );
        if let (Some(service), Some(error)) = (status.service, status.error) {
            out.push(ConfigErrorEntry { service, error });
        }
    }
    out.sort_by(|a, b| a.service.cmp(&b.service));
    out
}

pub(crate) fn read_config_error_sidecars() -> Vec<ConfigErrorEntry> {
    read_config_error_sidecars_from(&config_status_dir())
}
