//! Cache the node descriptions arriving on the auxiliary lane's status and
//! identity channels.
//!
//! The drone pushes a compact status snapshot and an identity frame over the
//! radio because it has no address on which it could be asked. This module is
//! where those land on the ground side: one record per peer device id, holding
//! the newest of each frame and when it arrived.
//!
//! The cache is in the receive process, but the surface an operator reads is
//! served by a different process, so the records are published to a sidecar the
//! way every other cross-process state on this node is.
//!
//! ## Age travels with the data, always
//!
//! Every record carries the wall-clock time each frame arrived, and every reader
//! is handed an age computed against a caller-supplied `now`. A record whose
//! status is older than [`STATUS_STALE_AFTER_S`] is not served as current: the
//! peer is either dropped from the published set, or (when its identity is still
//! fresh) published with its status omitted and a `status_fresh: false` flag.
//!
//! That distinction matters. A drone whose radio went quiet and a drone sitting
//! at 12% CPU look identical if the last snapshot keeps being served, and the
//! operator would be reading a number from a link that is gone. Losing the
//! reading is correct; keeping it is a lie (operating rule 44).
//!
//! ## Bounded
//!
//! The peer map is capped at [`MAX_PEERS`], evicting the stalest, and each record
//! holds one status and one identity rather than a history. Frames arrive off a
//! radio lane this node does not control, so a peer count that grew with the
//! traffic would be a memory fault waiting for a misbehaving or hostile
//! transmitter.
//!
//! ## Ordering
//!
//! Status frames carry a sequence number and travel over an unreliable datagram
//! lane, so they can arrive out of order. A frame older than the one already
//! held is DROPPED rather than allowed to overwrite it, which would make a
//! surface flicker backwards in time. The comparison is wrapping, so a sequence
//! that rolls over is not mistaken for a reordering; a producer that restarted
//! (and so restarted its sequence) is accepted, because by then the held record
//! has aged past the stale window.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::node_status::{NodeIdentity, NodeStatus};
use serde_json::{json, Value};

/// Sidecar schema version, a drift signal for the reader.
pub const AUX_PEERS_SIDECAR_VERSION: u16 = 1;

/// The sidecar filename under the run dir.
pub const AUX_PEERS_SIDECAR: &str = "relayed-status.json";

/// A status snapshot older than this is not served as a current reading.
///
/// Generous relative to the 1 Hz default cadence: a handful of lost datagrams on
/// a radio lane is normal and must not blank the surface, while a link that has
/// genuinely stopped is caught well inside the time an operator would act on it.
pub const STATUS_STALE_AFTER_S: f64 = 15.0;

/// A peer with no frame of any kind inside this window is dropped entirely.
pub const PEER_STALE_AFTER_S: f64 = 120.0;

/// Most peers held at once.
///
/// A fleet is up to `FLEET_MAX_SLOTS` (24) drones on one ground radio, and the
/// cap has to clear that with room for a re-pair that has not yet aged its old
/// entry out. What it still stops is a misbehaving transmitter growing the map
/// without bound.
pub const MAX_PEERS: usize = 64;

/// How often the sidecar is rewritten.
pub const PERSIST_CADENCE: Duration = Duration::from_secs(2);

/// Wall-clock unix seconds.
fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The current wall clock, for tests that need to read the cache with the same
/// clock the recorders used.
#[cfg(test)]
pub fn test_now() -> f64 {
    now_unix()
}

/// One relayed peer: the newest status and identity, and when each arrived.
#[derive(Debug, Clone, Default)]
struct PeerRecord {
    status: Option<NodeStatus>,
    status_at: f64,
    identity: Option<NodeIdentity>,
    identity_at: f64,
    /// Status frames accepted for this peer.
    status_frames: u64,
    /// Status frames dropped for arriving older than the one already held.
    out_of_order: u64,
}

impl PeerRecord {
    /// The newest arrival time of any frame, for the whole-peer prune.
    fn last_seen(&self) -> f64 {
        self.status_at.max(self.identity_at)
    }
}

/// Whether `candidate` is newer than `held` under wrapping sequence arithmetic.
///
/// A datagram lane reorders, so a frame that arrives is not necessarily the
/// latest. Wrapping comparison treats the smaller half of the number space as
/// "ahead", which keeps a sequence rollover from reading as a huge step
/// backwards.
fn seq_is_newer(candidate: u32, held: u32) -> bool {
    candidate != held && candidate.wrapping_sub(held) < u32::MAX / 2
}

/// The relayed-peer cache, shared between the lane reader and the persister.
#[derive(Debug, Default, Clone)]
pub struct AuxPeerCache {
    inner: Arc<Mutex<BTreeMap<String, PeerRecord>>>,
    /// The MAVLink system id most recently seen on each fleet slot.
    ///
    /// Kept apart from the status-derived records above because it comes from a
    /// different source with different evidence: a status frame is a node
    /// describing itself, while this is read off the MAVLink header of traffic
    /// that node's flight controller actually produced. Only the second can
    /// answer whether two aircraft are addressable apart, which is the question
    /// that matters here -- two flight controllers on one system id are ONE
    /// vehicle to a ground station, and a command sent to that id is accepted by
    /// both of them.
    system_ids: Arc<Mutex<BTreeMap<u8, u8>>>,
}

impl AuxPeerCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note the MAVLink system id seen on `slot`.
    ///
    /// Returns the set of OTHER slots already presenting the same id, so the
    /// caller can report a collision the first time it becomes observable. It
    /// returns rather than logs because the decision of how loudly to report
    /// belongs to the consumer, which knows whether this is the first sighting.
    pub fn observe_system_id(&self, slot: u8, system_id: u8) -> Vec<u8> {
        let mut map = self.system_ids.lock().unwrap();
        let previous = map.insert(slot, system_id);
        // Nothing to say when this slot has not changed what it presents; the
        // collision, if any, was already reported when it first appeared.
        if previous == Some(system_id) {
            return Vec::new();
        }
        map.iter()
            .filter(|(other_slot, other_id)| **other_slot != slot && **other_id == system_id)
            .map(|(other_slot, _)| *other_slot)
            .collect()
    }

    /// Every slot's currently-observed MAVLink system id, in slot order.
    pub fn system_ids_by_slot(&self) -> Vec<(u8, u8)> {
        self.system_ids
            .lock()
            .unwrap()
            .iter()
            .map(|(s, i)| (*s, *i))
            .collect()
    }

    /// Record a decoded status frame.
    ///
    /// Returns `false` when the frame was dropped for arriving out of order, so
    /// the caller can count it rather than silently discarding it.
    pub fn record_status(&self, status: NodeStatus) -> bool {
        self.record_status_at(status, now_unix())
    }

    /// [`Self::record_status`] with an injected clock, for tests.
    pub fn record_status_at(&self, status: NodeStatus, now: f64) -> bool {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(status.id.clone()).or_default();

        // Reject a frame older than the one held, UNLESS what is held has aged
        // past the stale window: a producer that restarted also restarted its
        // sequence, and refusing its frames forever would leave the peer
        // permanently blank.
        if let Some(held) = &entry.status {
            let held_is_fresh = now - entry.status_at <= STATUS_STALE_AFTER_S;
            if held_is_fresh && !seq_is_newer(status.sq, held.sq) {
                entry.out_of_order += 1;
                return false;
            }
        }
        entry.status = Some(status);
        entry.status_at = now;
        entry.status_frames += 1;
        Self::prune_locked(&mut map, now);
        true
    }

    /// Record a decoded identity frame.
    pub fn record_identity(&self, identity: NodeIdentity) {
        self.record_identity_at(identity, now_unix());
    }

    /// [`Self::record_identity`] with an injected clock, for tests.
    pub fn record_identity_at(&self, identity: NodeIdentity, now: f64) {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(identity.id.clone()).or_default();
        entry.identity = Some(identity);
        entry.identity_at = now;
        Self::prune_locked(&mut map, now);
    }

    /// Drop peers with no frame inside the window, then enforce the cap by
    /// evicting the stalest. Called under the lock after every insert.
    fn prune_locked(map: &mut BTreeMap<String, PeerRecord>, now: f64) {
        map.retain(|_, r| now - r.last_seen() <= PEER_STALE_AFTER_S);
        while map.len() > MAX_PEERS {
            // Evict the least-recently-heard peer. A live peer is never the
            // stalest, so the cap sheds noise before it sheds signal.
            let Some(oldest) = map
                .iter()
                .min_by(|a, b| {
                    a.1.last_seen()
                        .partial_cmp(&b.1.last_seen())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            map.remove(&oldest);
        }
    }

    /// The device ids of peers whose identity is still fresh, with their names.
    ///
    /// This is what lets the node answer "who is my peer" from the lane, and it
    /// deliberately keys on the IDENTITY frame's freshness: an identity is only
    /// worth reporting if the peer that claimed it is still speaking.
    pub fn fresh_identities(&self, now: f64) -> Vec<(String, Option<String>)> {
        let map = self.inner.lock().unwrap();
        map.values()
            .filter(|r| now - r.identity_at <= PEER_STALE_AFTER_S)
            .filter_map(|r| r.identity.as_ref())
            .map(|i| (i.id.clone(), i.nm.clone()))
            .collect()
    }

    /// Build the published peer list.
    ///
    /// Each entry carries its own ages and an explicit `status_fresh` flag. A
    /// peer whose status has gone stale keeps its identity (which does not
    /// expire the same way) but its `status` block is OMITTED rather than served
    /// as a current reading.
    pub fn peers_payload(&self, now: f64) -> Vec<Value> {
        let map = self.inner.lock().unwrap();
        map.values()
            .filter(|r| now - r.last_seen() <= PEER_STALE_AFTER_S)
            .map(|r| {
                let status_fresh = r.status.is_some() && now - r.status_at <= STATUS_STALE_AFTER_S;
                let mut entry = json!({
                    "status_fresh": status_fresh,
                    "status_frames": r.status_frames,
                    "out_of_order": r.out_of_order,
                });
                let obj = entry.as_object_mut().expect("built as an object");

                if let Some(i) = &r.identity {
                    obj.insert("device_id".into(), json!(i.id));
                    obj.insert("name".into(), json!(i.nm));
                    obj.insert("profile".into(), json!(i.pr));
                    obj.insert("agent_version".into(), json!(i.ver));
                    obj.insert("identity_at_unix".into(), json!(r.identity_at));
                    obj.insert("identity_age_s".into(), json!(round2(now - r.identity_at)));
                }
                if let Some(s) = &r.status {
                    // The device id is on the status frame too, so a peer that
                    // has sent status but no identity yet is still identified.
                    obj.entry("device_id").or_insert(json!(s.id));
                    obj.insert("status_at_unix".into(), json!(r.status_at));
                    obj.insert("status_age_s".into(), json!(round2(now - r.status_at)));
                    obj.insert("seq".into(), json!(s.sq));
                    if status_fresh {
                        obj.insert("status".into(), expand_status(s));
                    }
                }
                entry
            })
            .collect()
    }

    /// The full sidecar document.
    pub fn sidecar_payload(&self, now: f64) -> Value {
        json!({
            "version": AUX_PEERS_SIDECAR_VERSION,
            "wall_time_unix": now,
            "status_stale_after_s": STATUS_STALE_AFTER_S,
            "peers": self.peers_payload(now),
        })
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Expand the wire snapshot's short keys into readable ones for the sidecar.
///
/// The short names exist to fit a radio frame; nothing past the radio should
/// have to know them. An absent wire field stays absent here rather than
/// becoming a null, so "unknown" survives the translation intact.
fn expand_status(s: &NodeStatus) -> Value {
    let mut m = serde_json::Map::new();
    let mut put = |k: &str, v: Value| {
        if !v.is_null() {
            m.insert(k.to_string(), v);
        }
    };
    put("uptime_seconds", json!(s.up));
    put("agent_version", json!(s.ver));
    put("board_name", json!(s.bn));
    put("board_soc", json!(s.bs));
    put("board_tier", json!(s.bt));
    put("fc_connected", json!(s.fc));
    put("mavlink_alive", json!(s.fa));
    put("fc_variant", json!(s.fv));
    put("fc_firmware", json!(s.ff));
    put("services_running", json!(s.sr));
    put("services_failed", json!(s.sf));
    put("services_other", json!(s.so));
    put("failed_units", json!(s.sn));
    put("cpu_percent", json!(s.cp));
    put("memory_percent", json!(s.mp));
    put("disk_percent", json!(s.dp));
    put("temperature_c", json!(s.tc));
    put("camera_state", json!(s.cs));
    put("video_state", json!(s.vs));
    put("video_stream_count", json!(s.vn));
    Value::Object(m)
}

/// Rewrite the sidecar on a fixed cadence until cancelled.
///
/// Writes one document immediately so a reader sees a valid (empty) file before
/// the first frame, rather than an absent one it would have to treat as a fault.
pub async fn persist_loop(
    cache: AuxPeerCache,
    presence: Option<crate::presence::GsPresenceCache>,
    cancel: Arc<tokio::sync::Notify>,
) {
    let path = std::path::PathBuf::from(crate::paths::run_path(AUX_PEERS_SIDECAR));
    loop {
        let now = now_unix();
        let payload = cache.sidecar_payload(now);
        if let Err(e) = crate::sidecars::write_json_atomic(&path, &payload, 0o644) {
            tracing::debug!(error = %e, "aux_peers_persist_failed");
        }
        // Hand the freshly-learned identities to the linked-peers surface, which
        // is beacon-driven and so can carry a signal reading for a peer it cannot
        // name. Pushing the CURRENT fresh set each tick (rather than accumulating)
        // means an identity whose peer went quiet drops out on its own.
        if let Some(p) = &presence {
            p.set_aux_identities(cache.fresh_identities(now));
        }
        tokio::select! {
            _ = cancel.notified() => break,
            _ = tokio::time::sleep(PERSIST_CADENCE) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(id: &str, seq: u32) -> NodeStatus {
        NodeStatus::new(id, seq)
            .with_resources(Some(12.5), None, None, None)
            .with_services(4, 1, 0, &["ados-vision".to_string()])
    }

    #[test]
    fn the_video_leg_count_reaches_the_sidecar_and_absent_stays_absent() {
        // The ground station's camera surfaces read this file, and it is the
        // only place the count exists on this side of the radio. A drone that
        // reported nothing must leave the key OUT, so a reader shows unknown
        // rather than inferring a single camera from a missing field.
        let reported = expand_status(&status("drone-a", 1).with_payload(None, None, Some(2)));
        assert_eq!(reported["video_stream_count"], 2);

        let silent = expand_status(&status("drone-a", 1).with_payload(None, None, None));
        assert!(
            silent.get("video_stream_count").is_none(),
            "an unreported count must be absent, not a value"
        );
    }

    #[test]
    fn a_stale_status_is_never_served_as_current() {
        // The headline honesty rule: a drone whose link died must not keep
        // reporting the last CPU number it managed to send.
        let cache = AuxPeerCache::new();
        let t0 = 1_700_000_000.0;
        cache.record_status_at(status("drone-a", 1), t0);
        cache.record_identity_at(
            NodeIdentity::build("drone-a", Some("Alpha"), Some("drone"), Some("1.2.3")),
            t0,
        );

        let fresh = cache.peers_payload(t0 + 1.0);
        assert_eq!(fresh[0]["status_fresh"], true);
        assert!(fresh[0]["status"].is_object());
        assert_eq!(fresh[0]["status"]["cpu_percent"], 12.5);

        // Past the window: identity survives, the reading does not.
        let stale = cache.peers_payload(t0 + STATUS_STALE_AFTER_S + 1.0);
        assert_eq!(stale[0]["status_fresh"], false);
        assert!(
            stale[0].get("status").is_none(),
            "a stale snapshot must not be served"
        );
        assert_eq!(stale[0]["device_id"], "drone-a");
        assert!(stale[0]["status_age_s"].as_f64().unwrap() > STATUS_STALE_AFTER_S);
    }

    #[test]
    fn every_entry_carries_its_age() {
        let cache = AuxPeerCache::new();
        let t0 = 1_700_000_000.0;
        cache.record_status_at(status("drone-a", 1), t0);
        cache.record_identity_at(NodeIdentity::build("drone-a", None, None, None), t0);
        let p = cache.peers_payload(t0 + 3.0);
        assert_eq!(p[0]["status_age_s"], 3.0);
        assert_eq!(p[0]["identity_age_s"], 3.0);
    }

    #[test]
    fn an_out_of_order_frame_does_not_overwrite_a_newer_one() {
        // A datagram lane reorders. Letting an older frame land would make the
        // surface flicker backwards in time.
        let cache = AuxPeerCache::new();
        let t0 = 1_700_000_000.0;
        assert!(cache.record_status_at(status("drone-a", 10), t0));
        assert!(!cache.record_status_at(status("drone-a", 9), t0 + 0.1));
        assert!(!cache.record_status_at(status("drone-a", 10), t0 + 0.1));
        assert!(cache.record_status_at(status("drone-a", 11), t0 + 0.2));

        let p = cache.peers_payload(t0 + 1.0);
        assert_eq!(p[0]["seq"], 11);
        assert_eq!(p[0]["out_of_order"], 2);
        assert_eq!(p[0]["status_frames"], 2);
    }

    #[test]
    fn a_restarted_producer_is_accepted_once_the_held_frame_is_stale() {
        // A restart resets the sequence to a low number. Treating that as a
        // reordering forever would leave the peer permanently blank.
        let cache = AuxPeerCache::new();
        let t0 = 1_700_000_000.0;
        cache.record_status_at(status("drone-a", 5_000), t0);
        assert!(
            cache.record_status_at(status("drone-a", 1), t0 + STATUS_STALE_AFTER_S + 1.0),
            "a low sequence after the stale window is a restart, not a reordering"
        );
        let p = cache.peers_payload(t0 + STATUS_STALE_AFTER_S + 2.0);
        assert_eq!(p[0]["seq"], 1);
    }

    #[test]
    fn a_wrapping_sequence_is_not_read_as_a_jump_backwards() {
        assert!(seq_is_newer(1, u32::MAX));
        assert!(seq_is_newer(11, 10));
        assert!(!seq_is_newer(10, 11));
        assert!(!seq_is_newer(10, 10));
    }

    #[test]
    fn the_peer_map_is_bounded_and_sheds_the_stalest() {
        // Frames arrive off a lane this node does not control, so the map must
        // not grow with the traffic.
        let cache = AuxPeerCache::new();
        let t0 = 1_700_000_000.0;
        for i in 0..(MAX_PEERS + 5) {
            // Each successive peer is heard slightly later, so the earliest are
            // the ones evicted.
            cache.record_status_at(status(&format!("drone-{i}"), 1), t0 + i as f64);
        }
        let p = cache.peers_payload(t0 + MAX_PEERS as f64 + 5.0);
        assert_eq!(p.len(), MAX_PEERS);
        let ids: Vec<&str> = p.iter().map(|e| e["device_id"].as_str().unwrap()).collect();
        assert!(
            !ids.contains(&"drone-0"),
            "the stalest peer should be evicted"
        );
        assert!(ids.contains(&"drone-12"), "the newest peer must survive");
    }

    #[test]
    fn a_wholly_silent_peer_is_dropped() {
        let cache = AuxPeerCache::new();
        let t0 = 1_700_000_000.0;
        cache.record_status_at(status("drone-a", 1), t0);
        assert!(cache
            .peers_payload(t0 + PEER_STALE_AFTER_S + 1.0)
            .is_empty());
    }

    #[test]
    fn short_wire_keys_are_expanded_and_absent_stays_absent() {
        let cache = AuxPeerCache::new();
        let t0 = 1_700_000_000.0;
        cache.record_status_at(
            NodeStatus::new("drone-a", 1)
                .with_fc(Some(true), Some(false), None, Some("ardupilot"))
                .with_services(4, 1, 2, &["ados-vision".to_string()]),
            t0,
        );
        let p = cache.peers_payload(t0);
        let s = &p[0]["status"];
        assert_eq!(s["fc_connected"], true);
        assert_eq!(s["mavlink_alive"], false);
        assert_eq!(s["fc_firmware"], "ardupilot");
        assert_eq!(s["services_failed"], 1);
        assert_eq!(s["failed_units"][0], "ados-vision");
        // Unknown must survive translation as absent, never as a null or a zero.
        assert!(s.get("fc_variant").is_none());
        assert!(s.get("cpu_percent").is_none());
    }

    #[test]
    fn a_peer_that_sent_only_status_is_still_identified() {
        let cache = AuxPeerCache::new();
        let t0 = 1_700_000_000.0;
        cache.record_status_at(status("drone-a", 1), t0);
        let p = cache.peers_payload(t0);
        assert_eq!(p[0]["device_id"], "drone-a");
        assert!(p[0].get("name").is_none());
    }

    #[test]
    fn fresh_identities_reports_id_and_name() {
        let cache = AuxPeerCache::new();
        let t0 = 1_700_000_000.0;
        cache.record_identity_at(
            NodeIdentity::build("drone-a", Some("Alpha"), Some("drone"), None),
            t0,
        );
        assert_eq!(
            cache.fresh_identities(t0 + 1.0),
            vec![("drone-a".to_string(), Some("Alpha".to_string()))]
        );
        assert!(cache
            .fresh_identities(t0 + PEER_STALE_AFTER_S + 1.0)
            .is_empty());
    }

    #[test]
    fn the_sidecar_document_carries_its_own_clock_and_window() {
        let cache = AuxPeerCache::new();
        let doc = cache.sidecar_payload(1_700_000_000.0);
        assert_eq!(doc["version"], AUX_PEERS_SIDECAR_VERSION);
        assert_eq!(doc["wall_time_unix"], 1_700_000_000.0);
        // The reader must be able to apply the same freshness rule the writer
        // used, without hardcoding it.
        assert_eq!(doc["status_stale_after_s"], STATUS_STALE_AFTER_S);
        assert!(doc["peers"].as_array().unwrap().is_empty());
    }
}
