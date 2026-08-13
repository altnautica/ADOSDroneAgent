// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Altnautica — ADOS Drone Agent
//! Back-fill the radio-learned peer device id into the persisted pair state.
//!
//! ## The gap this closes
//!
//! `ados-radio`'s hop supervisor learns the peer's device id from a verified
//! presence beacon and writes `/run/ados/peer-backfill.json` on every newly-heard
//! peer. Nothing read it. Its intended consumer was a Python REST seam calling
//! `pair_manager.update_peer_device_id`, and that function had zero callers — so
//! the learned id never reached `config.yaml`, and
//! `gs_status`'s `paired_drone.device_id` (which reads
//! `video.wfb.paired_with_device_id`, with the legacy
//! `ground_station.paired_drone_id` fallback) returned null across restarts on
//! every rig paired by the unattended auto-bind, because the bind tunnel does not
//! always carry the peer id.
//!
//! ## Why the reader lives here
//!
//! `ados-control` owns every write to `config.yaml` (the pair route, the network
//! writes, the UI writes, the MAC pin overrides all persist through it), so the
//! back-fill belongs on the same side as the rest of the pair state rather than
//! adding a second config writer in `ados-radio`. The sidecar stays the seam
//! between the two processes.
//!
//! ## Why a reconciler and not a hook
//!
//! The id appears when the peer starts beaconing, which may be long after the
//! bind completed, and again on every reboot. Restating the question on a slow
//! tick covers both without the bind FSM knowing anything about config
//! persistence. The write is idempotent, so a re-heard beacon is free.
//!
//! Unlike its sibling reconcilers this applies NO staleness gate: the sidecar is a
//! latch (the last peer id learned), not a live reading, and a peer that has gone
//! quiet is still the peer this node is paired to. Ageing it would drop the very
//! back-fill an operator rebooted to obtain.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// The sidecar `ados-radio`'s hop supervisor writes on a newly-learned peer:
/// `{"peer_device_id": "<id>"}`.
const PEER_BACKFILL_SIDECAR: &str = "peer-backfill.json";

/// How often the front restates the back-fill.
///
/// Deliberately unhurried, matching the sibling fleet reconcilers: the answer
/// changes at most once per pair, and the cost of being one tick late is a status
/// read that still says null for another 30 s on a freshly-bound rig.
const BACKFILL_INTERVAL: Duration = Duration::from_secs(30);

/// Read the peer device id the radio latched, or `None` when the sidecar is
/// absent, unparseable, or carries no usable id.
///
/// An empty-string id is treated as absent: the hop supervisor only writes a
/// verified beacon's id, but a truncated write must not blank a good config value.
fn latched_peer_device_id(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let id = doc.get("peer_device_id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// Set `video.wfb.paired_with_device_id` (and, on a ground station, the legacy
/// `ground_station.paired_drone_id` mirror) without disturbing any other key.
///
/// Returns `Ok(true)` when the config was rewritten, `Ok(false)` when it already
/// held this id (the idempotent no-op a re-heard beacon takes), `Err(message)` on
/// a read/parse/write fault — e.g. the EPERM a non-root front gets on a 0600
/// config, which must not be fatal to the reconciler loop.
///
/// The two keys and the GS-only mirror match `wfb_pair_write::persist_pair_state`
/// exactly; this writes only the peer id, leaving `paired_at` and
/// `auto_pair_enabled` alone, because a beacon is evidence of the peer's identity
/// and of nothing else.
pub(crate) fn persist_peer_device_id(
    config_path: &Path,
    is_ground_station: bool,
    peer_device_id: &str,
) -> Result<bool, String> {
    use serde_norway::{Mapping, Value as Yaml};

    let mut data: Yaml = match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(Mapping::new()),
        },
        Err(_) => Yaml::Mapping(Mapping::new()),
    };

    let already = data
        .get("video")
        .and_then(|v| v.get("wfb"))
        .and_then(|w| w.get("paired_with_device_id"))
        .and_then(Yaml::as_str)
        .map(|s| s == peer_device_id)
        .unwrap_or(false);
    let mirror_already = !is_ground_station
        || data
            .get("ground_station")
            .and_then(|g| g.get("paired_drone_id"))
            .and_then(Yaml::as_str)
            .map(|s| s == peer_device_id)
            .unwrap_or(false);
    if already && mirror_already {
        return Ok(false);
    }

    {
        let root = data
            .as_mapping_mut()
            .ok_or_else(|| "config root is not a mapping".to_string())?;

        let video = section_mut(root, "video")?;
        let wfb = section_mut(video, "wfb")?;
        wfb.insert(
            Yaml::String("paired_with_device_id".to_string()),
            Yaml::String(peer_device_id.to_string()),
        );

        if is_ground_station {
            let gs = section_mut(root, "ground_station")?;
            gs.insert(
                Yaml::String("paired_drone_id".to_string()),
                Yaml::String(peer_device_id.to_string()),
            );
        }
    }

    let body = serde_norway::to_string(&data).map_err(|e| e.to_string())?;
    write_atomic(config_path, body.as_bytes())?;
    Ok(true)
}

/// Navigate/create a nested mapping under `key`. A node that exists but is not a
/// mapping is replaced with an empty mapping, matching every sibling config
/// writer's `_get_section` behaviour.
fn section_mut<'a>(
    parent: &'a mut serde_norway::Mapping,
    key: &str,
) -> Result<&'a mut serde_norway::Mapping, String> {
    use serde_norway::{Mapping, Value as Yaml};
    let k = Yaml::String(key.to_string());
    let entry = parent.entry(k).or_insert(Yaml::Mapping(Mapping::new()));
    if !entry.is_mapping() {
        *entry = Yaml::Mapping(Mapping::new());
    }
    entry
        .as_mapping_mut()
        .ok_or_else(|| format!("config section `{key}` is not a mapping"))
}

/// Write `body` to `path` through a temp file + rename so a reader never observes
/// a half-written config.
fn write_atomic(path: &Path, body: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp-peer-backfill");
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Restate the back-fill on [`BACKFILL_INTERVAL`] for the life of the front.
pub async fn run_peer_backfill_reconciler(config_path: PathBuf, is_ground_station: bool) {
    let sidecar = crate::routes::status_full::run_dir().join(PEER_BACKFILL_SIDECAR);
    let mut tick = tokio::time::interval(BACKFILL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let Some(peer_id) = latched_peer_device_id(&sidecar) else {
            continue;
        };
        let cfg = config_path.clone();
        let id = peer_id.clone();
        // Blocking disk I/O on a 0600 root-owned file; keep it off the reactor.
        let outcome = tokio::task::spawn_blocking(move || {
            persist_peer_device_id(&cfg, is_ground_station, &id)
        })
        .await;
        match outcome {
            Ok(Ok(true)) => {
                tracing::info!(peer_device_id = %peer_id, "peer_device_id_backfilled")
            }
            Ok(Ok(false)) => {}
            Ok(Err(e)) => tracing::warn!(
                peer_device_id = %peer_id,
                error = %e,
                "peer_device_id_backfill_failed"
            ),
            Err(e) => tracing::warn!(error = %e, "peer_device_id_backfill_task_failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_latched_id_is_read_and_a_blank_or_absent_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PEER_BACKFILL_SIDECAR);

        assert_eq!(latched_peer_device_id(&path), None, "absent sidecar");

        std::fs::write(&path, r#"{"peer_device_id":"drone-abc"}"#).unwrap();
        assert_eq!(latched_peer_device_id(&path), Some("drone-abc".to_string()));

        // A blank id must never blank a good config value.
        std::fs::write(&path, r#"{"peer_device_id":"  "}"#).unwrap();
        assert_eq!(latched_peer_device_id(&path), None);

        std::fs::write(&path, b"not json {{{").unwrap();
        assert_eq!(latched_peer_device_id(&path), None);
    }

    #[test]
    fn the_learned_peer_reaches_the_key_the_status_route_reads() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: my-gs\nvideo:\n  wfb:\n    paired_at: '2026-01-01T00:00:00+00:00'\n    auto_pair_enabled: true\n",
        )
        .unwrap();

        assert_eq!(
            persist_peer_device_id(&cfg, true, "drone-abc"),
            Ok(true),
            "the first back-fill rewrites the config"
        );

        let text = std::fs::read_to_string(&cfg).unwrap();
        let doc: serde_norway::Value = serde_norway::from_str(&text).unwrap();
        assert_eq!(
            doc["video"]["wfb"]["paired_with_device_id"].as_str(),
            Some("drone-abc")
        );
        assert_eq!(
            doc["ground_station"]["paired_drone_id"].as_str(),
            Some("drone-abc"),
            "the ground-station profile mirrors onto the legacy key"
        );
        // Untouched neighbours: the beacon proves identity and nothing else.
        assert_eq!(doc["agent"]["name"].as_str(), Some("my-gs"));
        assert_eq!(
            doc["video"]["wfb"]["paired_at"].as_str(),
            Some("2026-01-01T00:00:00+00:00")
        );
        assert_eq!(
            doc["video"]["wfb"]["auto_pair_enabled"].as_bool(),
            Some(true)
        );

        // Idempotent: a re-heard beacon does not rewrite.
        assert_eq!(persist_peer_device_id(&cfg, true, "drone-abc"), Ok(false));
        // A new peer id does.
        assert_eq!(persist_peer_device_id(&cfg, true, "drone-xyz"), Ok(true));
    }

    #[test]
    fn the_drone_profile_writes_only_the_canonical_key() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        assert_eq!(persist_peer_device_id(&cfg, false, "gs-001"), Ok(true));

        let doc: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            doc["video"]["wfb"]["paired_with_device_id"].as_str(),
            Some("gs-001")
        );
        assert!(
            doc.get("ground_station").is_none(),
            "a drone must not grow a ground_station section"
        );
        assert_eq!(persist_peer_device_id(&cfg, false, "gs-001"), Ok(false));
    }

    #[test]
    fn a_non_mapping_section_is_replaced_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "video: 7\n").unwrap();
        assert_eq!(persist_peer_device_id(&cfg, false, "gs-001"), Ok(true));
        let doc: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            doc["video"]["wfb"]["paired_with_device_id"].as_str(),
            Some("gs-001")
        );
    }
}
