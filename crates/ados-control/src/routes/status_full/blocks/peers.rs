//! Linked WFB peers: the relay peer list + freshest-peer scalars.

use super::super::*;
use ados_protocol::paths::run_dir;
use serde_json::json;
use serde_json::Value;
use std::path::Path;

/// The linked-WFB-peers snapshot sidecar filename under the run dir. The
/// receive-side presence listener rewrites it ~every 5 s with every drone whose
/// PresenceBeacon it currently decodes (the sibling the cloud heartbeat folds).
pub(crate) const LINKED_PEERS_SIDECAR: &str = "linked-peers.json";

/// A linked peer whose last decoded beacon is older than this reads as stale and
/// is dropped: the receive listener's own prune window, which is the radio's
/// peer-stale threshold. Per-entry gating also covers the dead-writer case: a
/// stale file's entries are all old, so the whole fold reads absent rather than
/// republishing ghost peers.
pub(crate) const LINKED_PEER_STALE_S: f64 = ados_groundlink::presence::LINKED_PEER_STALE_AFTER_S;

/// One raw peer row as the linked-peers sidecar writes it (snake_case, the
/// receive listener's `LinkedPeer`). The sidecar's `version` / `wall_time_unix`
/// header keys are ignored as unknown fields, exactly like the cloud fold.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub(crate) struct LinkedPeerRow {
    #[serde(default)]
    pub(crate) device_id: String,
    #[serde(default)]
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) channel: u8,
    #[serde(default)]
    pub(crate) rssi_dbm: i8,
    #[serde(default)]
    pub(crate) last_seen_unix: f64,
    /// The fleet slot whose receive chain decoded the beacon.
    #[serde(default)]
    pub(crate) slot: Option<u8>,
}

/// The linked-peers sidecar document (the peer list plus header keys we ignore).
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct LinkedPeersDoc {
    #[serde(default)]
    pub(crate) peers: Vec<LinkedPeerRow>,
}

/// The linked-WFB-peers fold for the consolidated body: the fresh peer set as the
/// top-level `linked_peers` array plus the freshest peer's `peerDeviceId` /
/// `peerRssiDbm` scalars, so a GCS paired LOCAL-FIRST to a ground node can
/// transitively enrol each drone that node relays over WFB — the exact signal the
/// cloud heartbeat carries, now on the PRIMARY LAN path.
///
/// Each array entry is served in the SNAKE_CASE shape the GCS
/// `FullStatusResponse.linked_peers` reader consumes: `device_id` / `role` /
/// `channel` / `rssi_dbm` / `seen_at_unix`. The sidecar writes the last-decode
/// time as `last_seen_unix`; the reader keys on `seen_at_unix`, so it is REMAPPED
/// on emit. The scalars `peerDeviceId` / `peerRssiDbm` are camelCase, the
/// single-peer fallback the GCS reads until it prefers the list.
///
/// PROFILE-AGNOSTIC by construction — it reads the sidecar, not the profile — and
/// staleness-gated per entry, so the fold is absent (no keys) on a drone / a
/// peerless ground node / a node whose relay lane went quiet, never a fabricated
/// block.
pub(crate) fn read_linked_peers() -> Vec<(String, Value)> {
    read_linked_peers_in(&run_dir().join(LINKED_PEERS_SIDECAR), now_unix_secs())
}

/// The path + now injectable core of [`read_linked_peers`], so a test drives the
/// freshness gate deterministically against a tempdir. Absent / unreadable /
/// unparseable → nothing folded; each entry gated on the prune window (a future
/// timestamp counts as fresh, tolerating clock skew, matching the cloud fold);
/// the whole fold omitted when no fresh, id-bearing peer survives.
/// Every fresh, id-bearing peer row the sidecar currently holds.
///
/// Split out so the fleet-enrolment reconciler gates on the SAME freshness
/// window as this status fold rather than carrying its own copy. Two
/// independently-maintained staleness gates over one sidecar drift apart, and
/// the direction that matters here is the silent one: a slot issued to a peer
/// this fold already considers gone.
pub(crate) fn fresh_linked_peer_rows_in(path: &Path, now: f64) -> Vec<LinkedPeerRow> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<LinkedPeersDoc>(&text) else {
        return Vec::new();
    };
    doc.peers
        .into_iter()
        .filter(|p| !p.device_id.is_empty())
        .filter(|p| p.last_seen_unix > 0.0 && (now - p.last_seen_unix) <= LINKED_PEER_STALE_S)
        .collect()
}

pub(crate) fn read_linked_peers_in(path: &Path, now: f64) -> Vec<(String, Value)> {
    let fresh = fresh_linked_peer_rows_in(path, now);
    if fresh.is_empty() {
        return Vec::new();
    }

    // The freshest peer (max last-seen) backs the single-peer scalar fallback.
    let freshest = fresh
        .iter()
        .max_by(|a, b| {
            a.last_seen_unix
                .partial_cmp(&b.last_seen_unix)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("fresh is non-empty");

    let entries: Vec<Value> = fresh
        .iter()
        .map(|p| {
            json!({
                "device_id": p.device_id,
                "role": p.role,
                "channel": p.channel,
                "rssi_dbm": p.rssi_dbm,
                // The sidecar writes `last_seen_unix`; the GCS reader keys on
                // `seen_at_unix`. Remap on emit so this LAN producer stays in
                // lockstep with the FullStatusResponse.linked_peers reader.
                "seen_at_unix": p.last_seen_unix,
                // Which fleet slot's receiver decoded the beacon, so a peer
                // heard on a slot other than its own is visible.
                "slot": p.slot,
            })
        })
        .collect();

    vec![
        ("linked_peers".to_string(), Value::Array(entries)),
        ("peerDeviceId".to_string(), json!(freshest.device_id)),
        ("peerRssiDbm".to_string(), json!(freshest.rssi_dbm)),
    ]
}
