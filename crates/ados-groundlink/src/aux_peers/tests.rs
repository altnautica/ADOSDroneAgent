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
        NodeIdentity::build("drone-a", Some("Alpha"), Some("drone"), Some("1.2.3"), &[]),
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
    cache.record_identity_at(NodeIdentity::build("drone-a", None, None, None, &[]), t0);
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
        NodeIdentity::build("drone-a", Some("Alpha"), Some("drone"), None, &[]),
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

#[test]
fn two_linked_slots_on_one_system_id_are_published_as_a_conflict() {
    let cache = AuxPeerCache::new();
    let t0 = 1_700_000_000.0;
    assert!(cache.observe_system_id_at(1, 1, t0).is_empty());
    assert_eq!(cache.observe_system_id_at(2, 1, t0 + 1.0), vec![1]);
    cache.observe_system_id_at(3, 3, t0 + 1.0);

    let doc = cache.sidecar_payload(t0 + 2.0);
    assert_eq!(doc["conflicting_system_ids"], json!([1]));
    let ids = slot_system_ids_from_sidecar(&doc, t0 + 2.0, 20.0);
    assert_eq!(
        ids.get(&1),
        Some(&SlotSystemId {
            fc_system_id: 1,
            conflict: true
        })
    );
    assert!(ids[&2].conflict);
    assert_eq!(
        ids.get(&3),
        Some(&SlotSystemId {
            fc_system_id: 3,
            conflict: false
        })
    );
}

#[test]
fn a_slot_that_went_quiet_stops_holding_its_system_id() {
    let cache = AuxPeerCache::new();
    let t0 = 1_700_000_000.0;
    cache.observe_system_id_at(1, 1, t0);
    let later = t0 + SYSTEM_ID_STALE_AFTER_S + 1.0;
    // A different aircraft now on id 1 is not in conflict with a silent slot.
    assert!(cache.observe_system_id_at(2, 1, later).is_empty());
    assert!(cache.conflicting_system_ids(later).is_empty());

    // The reader re-ages rows too: a document read after its rows went
    // stale reports no identity rather than a frozen one.
    let doc = cache.sidecar_payload(later);
    let read_late = later + SYSTEM_ID_STALE_AFTER_S + 1.0;
    assert!(slot_system_ids_from_sidecar(&doc, read_late, 60.0).is_empty());
    // A document whose writer stopped is ignored outright.
    assert!(slot_system_ids_from_sidecar(&doc, later + 30.0, 20.0).is_empty());
}

/// The current wall clock, for tests that need to read the cache with the same
/// clock the recorders used.
pub(crate) fn test_now() -> f64 {
    now_unix()
}
