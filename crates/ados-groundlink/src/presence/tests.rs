use super::listen::{
    write_listener_health_to, LISTEN_RETRY_INTERVAL, PRESENCE_LISTENER_SIDECAR_NAME,
};
use super::*;
use ados_radio::hop::derive_pair_key;

/// The cold-start pair key, which a dev host without `/etc/drone.key` runs on.
fn test_key() -> [u8; 32] {
    derive_pair_key(None)
}

/// A frame decoded on slot 1 of a single-drone station.
const SOLO: ControlContext = ControlContext {
    slot: 1,
    fleet_slots: 1,
};

#[test]
fn presence_ports_are_asymmetric() {
    // The trap: emit to 5810 (the single tx_control ingress), listen on the
    // per-slot control egress. If a listen port ever equalled the emit port
    // the GS would self-pair with its own device-id over loopback, so the
    // whole per-slot range must stay clear of it.
    assert_eq!(PRESENCE_EMIT_PORT, 5810);
    for slot in 0..=ados_radio::config::FLEET_MAX_SLOTS {
        assert_ne!(PRESENCE_EMIT_PORT, presence_listen_port(slot));
        assert_ne!(HOP_ACK_ECHO_PORT, presence_listen_port(slot));
    }
    // Distinct per slot: two slots sharing a listen port would collide on
    // the bind and leave one drone's control lane unheard.
    let ports: std::collections::BTreeSet<u16> = (1..=ados_radio::config::FLEET_MAX_SLOTS)
        .map(presence_listen_port)
        .collect();
    assert_eq!(ports.len(), ados_radio::config::FLEET_MAX_SLOTS as usize);
}

#[test]
fn cold_start_pair_key_matches_radio_crate() {
    // With no key file on disk the resolver must produce the same cold-start
    // key the radio crate derives, so a pre-bind beacon round-trips.
    let (resolved, status) = resolve_pair_key();
    // On a dev host /etc/drone.key is absent, so this is the cold path.
    if ados_radio::paths::load_shared_key() == ados_radio::paths::SharedKey::Absent {
        assert_eq!(resolved, Some(derive_pair_key(None)));
        assert_eq!(status, "cold_start");
    }
}

#[test]
fn cache_age_none_until_first_beacon() {
    let cache = GsPresenceCache::new();
    assert!(cache.presence_age_s().is_none());
    assert!(cache.announced_channel().is_none());
    assert!(cache.peer_last_seen_unix().is_none());
}

#[test]
fn cache_records_peer_and_exposes_channel_and_fresh_age() {
    let cache = GsPresenceCache::new();
    cache.record_peer("drone-abc".into(), "drone".into(), 157, -48, 1);
    assert_eq!(cache.announced_channel(), Some(157));
    assert_eq!(cache.peer_channel(), Some(157));
    assert!(cache.peer_last_seen_unix().is_some());
    // Just recorded: age is small and non-negative.
    let age = cache.presence_age_s().expect("age present after record");
    assert!((0.0..5.0).contains(&age), "age {age} not fresh");
    // Fresh within the watchdog's 30 s window → peer_present() true.
    assert!(cache.peer_present());
}

#[test]
fn linked_peers_empty_until_a_beacon_is_decoded() {
    let cache = GsPresenceCache::new();
    assert!(cache.linked_peers().is_empty());
}

#[test]
fn linked_peers_lists_every_decoded_drone_newest_first() {
    // A ground station relaying two drones must report BOTH — the scalar
    // fields only ever track the last-heard one, but the list carries all.
    let cache = GsPresenceCache::new();
    cache.record_peer("drone-a".into(), "drone".into(), 149, -60, 1);
    cache.record_peer("drone-b".into(), "drone".into(), 157, -48, 2);
    let peers = cache.linked_peers();
    assert_eq!(peers.len(), 2);
    // Newest decode (drone-b) is first.
    assert_eq!(peers[0].device_id, "drone-b");
    assert_eq!(peers[0].channel, 157);
    assert_eq!(peers[0].rssi_dbm, -48);
    assert_eq!(peers[0].role, "drone");
    assert!(peers[0].last_seen_unix > 0.0);
    assert_eq!(peers[1].device_id, "drone-a");
    // The scalar watchdog surface still tracks the freshest peer only.
    assert_eq!(cache.peer_channel(), Some(157));
}

#[test]
fn linked_peers_upserts_by_device_id_not_duplicates() {
    // Re-hearing the same drone refreshes its entry (channel/rssi/last-seen)
    // rather than appending a duplicate.
    let cache = GsPresenceCache::new();
    cache.record_peer("drone-a".into(), "drone".into(), 149, -60, 1);
    cache.record_peer("drone-a".into(), "drone".into(), 157, -45, 1);
    let peers = cache.linked_peers();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].channel, 157);
    assert_eq!(peers[0].rssi_dbm, -45);
}

#[test]
fn linked_peers_payload_has_version_wall_time_and_snake_case_entries() {
    // The heartbeat producers read this file's snake_case keys and remap to
    // the camelCase wire shape; pin the on-disk keys so a producer reads the
    // right ones.
    let cache = GsPresenceCache::new();
    cache.record_peer("drone-a".into(), "drone".into(), 149, -60, 3);
    let payload = linked_peers_payload(&cache.linked_peers());
    assert_eq!(payload["version"], LINKED_PEERS_SIDECAR_VERSION);
    assert!(payload["wall_time_unix"].as_f64().unwrap() > 0.0);
    let entry = &payload["peers"][0];
    assert_eq!(entry["device_id"], "drone-a");
    assert_eq!(entry["role"], "drone");
    assert_eq!(entry["channel"], 149);
    assert_eq!(entry["rssi_dbm"], -60);
    assert!(entry["last_seen_unix"].as_f64().unwrap() > 0.0);
}

#[test]
fn linked_peers_empty_payload_is_a_valid_empty_list() {
    // Before any beacon, the persister still writes a valid file so the
    // heartbeat reads an empty list, never a missing/garbage file.
    let payload = linked_peers_payload(&[]);
    assert_eq!(payload["version"], LINKED_PEERS_SIDECAR_VERSION);
    assert!(payload["peers"].as_array().unwrap().is_empty());
}

#[test]
fn hop_snapshot_shape_matches_listener_keys() {
    // An untouched cache snapshots an empty, valid listener shape: source
    // "listener", thresholds null, history empty, last_hop_at 0.
    let cache = GsPresenceCache::new();
    let snap = cache.hop_snapshot("u-nii-3");
    assert!(snap.enabled);
    assert_eq!(snap.band, "u-nii-3");
    assert!(snap.hop_period_seconds.is_none());
    assert!(snap.loss_threshold_percent.is_none());
    assert!(snap.rssi_threshold_dbm.is_none());
    assert_eq!(snap.last_hop_at, 0.0);
    assert!(snap.history.is_empty());
    assert_eq!(snap.source, "listener");

    // The serialized payload carries the wall_time_unix stamp + null
    // thresholds (the JSON shape a cross-process reader sees).
    let v = hop_supervisor_payload(&snap);
    assert_eq!(v["source"], "listener");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["band"], "u-nii-3");
    assert!(v["hop_period_seconds"].is_null());
    assert!(v["loss_threshold_percent"].is_null());
    assert!(v["rssi_threshold_dbm"].is_null());
    assert!(v["history"].as_array().unwrap().is_empty());
    assert!(v["wall_time_unix"].as_f64().unwrap() > 0.0);
}

#[test]
fn record_peer_appends_a_follow_entry_only_on_channel_change() {
    let cache = GsPresenceCache::new();
    // First beacon: a follow from 0 (unknown prior) to 157.
    cache.record_peer("drone-1".into(), "drone".into(), 157, -50, 1);
    let s = cache.hop_snapshot("u-nii-3");
    assert_eq!(s.history.len(), 1);
    assert_eq!(s.history[0].from, 0);
    assert_eq!(s.history[0].to, 157);
    assert_eq!(s.history[0].trigger, "periodic");
    assert!(s.history[0].ok);
    assert!(s.last_hop_at > 0.0);

    // Same channel again: no new entry.
    cache.record_peer("drone-1".into(), "drone".into(), 157, -47, 1);
    assert_eq!(cache.hop_snapshot("u-nii-3").history.len(), 1);

    // New channel: a follow from 157 to 149.
    cache.record_peer("drone-1".into(), "drone".into(), 149, -45, 1);
    let s = cache.hop_snapshot("u-nii-3");
    assert_eq!(s.history.len(), 2);
    assert_eq!(s.history[1].from, 157);
    assert_eq!(s.history[1].to, 149);
}

#[test]
fn hop_history_is_capped_at_thirty_two() {
    let cache = GsPresenceCache::new();
    // Alternate between two channels so every beacon is a change; drive well
    // past the 32-entry cap and confirm only the last 32 survive.
    for i in 0..50u8 {
        let ch = if i % 2 == 0 { 149 } else { 153 };
        cache.record_peer("drone-1".into(), "drone".into(), ch, -50, 1);
    }
    let s = cache.hop_snapshot("u-nii-3");
    assert_eq!(s.history.len(), HOP_HISTORY_CAP);
    // The last recorded channel is the most recent `to`.
    let last = s.history.last().unwrap();
    let expected_last = if 49 % 2 == 0 { 149 } else { 153 };
    assert_eq!(last.to, expected_last);
}

#[tokio::test]
async fn emit_and_listen_round_trip_over_loopback() {
    // Wire the listener to a custom port so the emit hits it directly
    // (in production the wfb_tx_control bridge sits between the two ports;
    // here we point a local sender straight at the listener's port to prove
    // the verify + cache-update path). Use the real listen port via a
    // sender that targets it.
    let cache = GsPresenceCache::new();
    let listener_cache = cache.clone();

    // Bind the listener on an ephemeral port to avoid colliding with a real
    // 5803 on the dev host or a parallel test.
    let sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let listen_addr = sock.local_addr().unwrap();

    // Drive one decode by hand using the same verify path the listener uses.
    let pair_key = test_key();
    let beacon = build_presence_beacon("drone-xyz", true, 161, -55, 123_456, &pair_key);

    let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    sender.send_to(&beacon, listen_addr).await.unwrap();

    let mut buf = [0u8; 256];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("listener recv timed out")
        .unwrap();
    let peer = parse_presence_beacon(&buf[..len], &pair_key).expect("beacon verifies");
    listener_cache.record_peer(peer.device_id, peer.role, peer.channel, peer.rssi_dbm, 1);

    assert_eq!(cache.announced_channel(), Some(161));
    assert_eq!(cache.peer_channel(), Some(161));
}

// ---- ports + frame-length constants -----------------------------------

#[test]
fn hop_ack_echo_targets_tx_control_ingress() {
    // The ACK must go to wfb_tx_control's loopback ingress (5810), the same
    // port the presence emit uses, so the ACK is transmitted over RF.
    assert_eq!(HOP_ACK_ECHO_PORT, 5810);
    assert_eq!(HOP_ACK_ECHO_PORT, PRESENCE_EMIT_PORT);
    // The two frame classes that share the control port have distinct lengths
    // so the length gate is unambiguous.
    assert_eq!(HOP_FRAME_LEN, 51);
    assert_eq!(PRESENCE_FRAME_LEN, 68);
    assert_ne!(HOP_FRAME_LEN, PRESENCE_FRAME_LEN);
}

// ---- HopAnnounce → follow with real trigger ---------------------------

#[test]
fn record_hop_announce_uses_real_trigger_and_preserves_presence_identity() {
    let cache = GsPresenceCache::new();
    // Seed a prior verified beacon: identity + liveness are now set.
    cache.record_peer("drone-1".into(), "drone".into(), 149, -50, 1);
    let seeded_age = cache.peer_last_seen_unix();
    assert!(seeded_age.is_some());

    // A reactive HopAnnounce to a new channel: the follow is recorded with
    // the REAL trigger, the channel updates, but the beacon-driven liveness
    // stamp and identity are untouched (an announce is not a presence beacon).
    cache.record_hop_announce(157, "reactive");
    assert_eq!(cache.peer_channel(), Some(157));
    let s = cache.hop_snapshot("u-nii-3");
    // First entry from the beacon (0 → 149, periodic), second from the
    // announce (149 → 157, reactive).
    assert_eq!(s.history.len(), 2);
    assert_eq!(s.history[1].from, 149);
    assert_eq!(s.history[1].to, 157);
    assert_eq!(s.history[1].trigger, "reactive");
    // Presence liveness stamp is unchanged by the announce (still the beacon's).
    assert_eq!(cache.peer_last_seen_unix(), seeded_age);
}

#[test]
fn record_hop_announce_same_channel_is_a_noop() {
    let cache = GsPresenceCache::new();
    cache.record_hop_announce(149, "periodic");
    // First announce records a follow from unknown (0) → 149.
    assert_eq!(cache.hop_snapshot("u-nii-3").history.len(), 1);
    // Re-announcing the same channel adds no new follow entry.
    cache.record_hop_announce(149, "reactive");
    assert_eq!(cache.hop_snapshot("u-nii-3").history.len(), 1);
    assert_eq!(cache.peer_channel(), Some(149));
}

// ---- HopAnnounce decode → HopAck echo over loopback -------------------

#[tokio::test]
async fn hop_announce_decodes_and_echoes_verbatim_hop_ack() {
    use ados_radio::hop::{build_hop_announce, HopTrigger};

    let cache = GsPresenceCache::new();
    let pair_key = test_key();

    // The listener's socket (sends the echo from here) + a stand-in for
    // wfb_tx_control's loopback ingress (receives the ACK).
    let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_addr = ack_recv.local_addr().unwrap();
    let ack_target = match ack_addr {
        std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
        _ => unreachable!("ipv4 loopback"),
    };
    let follower = HopFollower::new(
        RecordingSetter::ok(),
        Arc::new(tokio::sync::Mutex::new(vec!["wlan1".to_string()])),
    );

    let announce = build_hop_announce(0, 157, HopTrigger::Reactive, &pair_key);
    handle_control_frame(
        &listen_sock,
        &announce,
        &pair_key,
        &cache,
        "", // own device id irrelevant to the hop path
        ack_target,
        Some(&follower),
        SOLO,
    )
    .await;

    // The ACK that landed at the tx-control ingress is the verbatim announce.
    let mut buf = [0u8; 256];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), ack_recv.recv_from(&mut buf))
        .await
        .expect("hop ack not echoed")
        .unwrap();
    assert_eq!(&buf[..len], &announce[..], "echoed ack must be verbatim");
    // The follow was recorded with the announce's real trigger + channel.
    assert_eq!(cache.peer_channel(), Some(157));
    let s = cache.hop_snapshot("u-nii-3");
    assert_eq!(s.history.last().unwrap().trigger, "reactive");
    assert_eq!(s.history.last().unwrap().to, 157);
}

/// Dispatch `announce` and report whether an ack reached the tx ingress.
async fn acked(
    announce: &[u8],
    cache: &GsPresenceCache,
    follower: Option<&HopFollower>,
    ctx: ControlContext,
) -> bool {
    let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_target = match ack_recv.local_addr().unwrap() {
        std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
        _ => unreachable!("ipv4 loopback"),
    };
    handle_control_frame(
        &listen_sock,
        announce,
        &test_key(),
        cache,
        "",
        ack_target,
        follower,
        ctx,
    )
    .await;
    let mut buf = [0u8; 64];
    tokio::time::timeout(Duration::from_millis(200), ack_recv.recv_from(&mut buf))
        .await
        .is_ok()
}

#[tokio::test]
async fn a_fleet_station_neither_acks_nor_follows_one_drone_s_hop() {
    // One receive radio serves every slot. Following drone A's hop would
    // strand drones B..N on the old channel.
    use ados_radio::hop::{build_hop_announce, HopTrigger};
    let cache = GsPresenceCache::new();
    cache.record_peer("drone-a".into(), "drone".into(), 149, -50, 1);
    let setter = RecordingSetter::ok();
    let follower = HopFollower::new(
        setter.clone(),
        Arc::new(tokio::sync::Mutex::new(vec!["wlan1".to_string()])),
    );
    let announce = build_hop_announce(0, 157, HopTrigger::Reactive, &test_key());
    let fleet = ControlContext {
        slot: 1,
        fleet_slots: 2,
    };
    assert!(!acked(&announce, &cache, Some(&follower), fleet).await);
    assert_eq!(
        cache.peer_channel(),
        Some(149),
        "the receive hint stays put"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(setter.calls().is_empty());
    let refusal = cache
        .hop_snapshot("u-nii-3")
        .last_refusal
        .expect("recorded");
    assert_eq!(refusal.reason, "fleet_hop_refused");
    assert_eq!(refusal.channel, 157);
}

#[tokio::test]
async fn a_station_that_cannot_retune_does_not_ack() {
    // An ack commits the drone. With no receive interface resolved there is
    // nothing to retune, so acking would split the pair.
    use ados_radio::hop::{build_hop_announce, HopTrigger};
    let cache = GsPresenceCache::new();
    let follower = HopFollower::new(
        RecordingSetter::ok(),
        Arc::new(tokio::sync::Mutex::new(Vec::new())),
    );
    let announce = build_hop_announce(0, 157, HopTrigger::Periodic, &test_key());
    assert!(!acked(&announce, &cache, Some(&follower), SOLO).await);
    assert_eq!(
        cache.hop_snapshot("u-nii-3").last_refusal.unwrap().reason,
        "no_receive_iface"
    );
}

#[test]
fn the_listener_rebind_is_a_flat_retry_in_the_recovery_band() {
    // Never zero (a hard-failing bind must not busy-spin) and never longer
    // than the recovery band, because the presence input is what tells this
    // ground station where the drone actually is.
    assert!(!LISTEN_RETRY_INTERVAL.is_zero());
    assert!(LISTEN_RETRY_INTERVAL >= Duration::from_secs(2));
    assert!(LISTEN_RETRY_INTERVAL <= Duration::from_secs(5));
}

#[test]
fn listener_health_sidecar_round_trips_with_expected_keys() {
    // Write the sidecar into a temp tree via the explicit-path seam. No env
    // mutation: the temp path is threaded in directly, so this test cannot
    // race any other test under the parallel runner.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(PRESENCE_LISTENER_SIDECAR_NAME);

    let health = ListenerHealth {
        starts: 3,
        restarts: 2,
        panics: 1,
        last_exit: "panic: task panicked".to_string(),
        started_at_unix: now_unix(),
    };
    write_listener_health_to(&path, &health);

    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["starts"], 3);
    assert_eq!(v["restarts"], 2);
    assert_eq!(v["panics"], 1);
    assert_eq!(v["last_exit"], "panic: task panicked");
    assert!(v["started_at_unix"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn listener_survives_a_transient_recv_error_and_keeps_feeding_the_cache() {
    // The listener must keep updating the presence cache after a recv hiccup.
    // Drive a verified beacon straight at the bound listener socket through
    // the same dispatch path the loop uses; a recv error in between would, in
    // the buggy version, have ended the loop and frozen the cache. Here we
    // assert the cache is fed and stays fed across repeated dispatches,
    // proving the per-frame handler is independent of any single recv outcome.
    let cache = GsPresenceCache::new();
    let pair_key = test_key();
    let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_addr = ack_recv.local_addr().unwrap();
    let ack_target = match ack_addr {
        std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
        _ => unreachable!("ipv4 loopback"),
    };

    // First good frame.
    let b1 = build_presence_beacon("drone-aaa", true, 149, -50, 1, &pair_key);
    handle_control_frame(
        &listen_sock,
        &b1,
        &pair_key,
        &cache,
        "",
        ack_target,
        None,
        SOLO,
    )
    .await;
    assert_eq!(cache.peer_channel(), Some(149));
    let first_seen = cache.peer_last_seen_unix();
    assert!(first_seen.is_some());

    // A garbage frame (stand-in for the kind of bad input a recv might hand
    // up) must be dropped without disturbing the established cache state.
    handle_control_frame(
        &listen_sock,
        &[0u8; 10],
        &pair_key,
        &cache,
        "",
        ack_target,
        None,
        SOLO,
    )
    .await;
    assert_eq!(cache.peer_channel(), Some(149));

    // A later good frame still updates the cache: the writer is alive.
    let b2 = build_presence_beacon("drone-aaa", true, 157, -45, 2, &pair_key);
    handle_control_frame(
        &listen_sock,
        &b2,
        &pair_key,
        &cache,
        "",
        ack_target,
        None,
        SOLO,
    )
    .await;
    assert_eq!(cache.peer_channel(), Some(157));
    assert!(cache.peer_present());
}

#[tokio::test]
async fn presence_beacon_is_not_misrouted_to_the_hop_path() {
    // A 68-byte PresenceBeacon must NOT be echoed as a HopAck (no ACK lands)
    // and MUST be recorded as a peer via the presence path.
    let cache = GsPresenceCache::new();
    let pair_key = test_key();

    let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_addr = ack_recv.local_addr().unwrap();
    let ack_target = match ack_addr {
        std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
        _ => unreachable!("ipv4 loopback"),
    };

    let beacon = build_presence_beacon("drone-xyz", true, 161, -55, 9, &pair_key);
    handle_control_frame(
        &listen_sock,
        &beacon,
        &pair_key,
        &cache,
        "",
        ack_target,
        None,
        SOLO,
    )
    .await;

    // The presence path ran: the peer is recorded.
    assert_eq!(cache.peer_channel(), Some(161));
    assert!(cache.peer_present());
    // No HopAck was emitted onto the ACK ingress.
    let mut buf = [0u8; 256];
    let echoed =
        tokio::time::timeout(Duration::from_millis(200), ack_recv.recv_from(&mut buf)).await;
    assert!(echoed.is_err(), "a presence beacon must not echo a hop ack");
}

/// Records the channel each retune requested, for the hop-follow assertions.
#[derive(Default)]
struct RecordingSetter {
    calls: std::sync::Mutex<Vec<(String, u8)>>,
    result: bool,
}

impl RecordingSetter {
    fn ok() -> Arc<Self> {
        Arc::new(Self {
            calls: std::sync::Mutex::new(Vec::new()),
            result: true,
        })
    }
    fn calls(&self) -> Vec<(String, u8)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ChannelSetter for RecordingSetter {
    fn set_channel<'a>(
        &'a self,
        interface: &'a str,
        channel: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((interface.to_string(), channel));
            self.result
        })
    }
}

#[tokio::test]
async fn a_follow_that_carries_traffic_holds() {
    let setter = RecordingSetter::ok();
    let iface = Arc::new(tokio::sync::Mutex::new(vec!["wlan1".to_string()]));
    let follower =
        HopFollower::new(setter.clone(), iface).with_verify_window(Duration::from_millis(50));
    let counter = SharedValidCounter::new();
    follower.set_valid_counter(counter.clone());
    let cache = GsPresenceCache::new();
    let feeding = {
        let counter = counter.clone();
        tokio::spawn(async move {
            for _ in 0..20 {
                counter.add(10);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    let outcome = follower
        .follow("wlan1", 157, Duration::ZERO, Some(149), &cache)
        .await;
    feeding.abort();
    assert_eq!(outcome, FollowOutcome::Followed);
    assert_eq!(setter.calls(), vec![("wlan1".to_string(), 157)]);
}

#[tokio::test]
async fn a_follow_with_no_traffic_on_the_new_channel_goes_back() {
    // The drone acked path failed on its side: nothing arrives on the target.
    // The station returns to the channel it left instead of staying deaf.
    let setter = RecordingSetter::ok();
    let iface = Arc::new(tokio::sync::Mutex::new(vec!["wlan1".to_string()]));
    let follower =
        HopFollower::new(setter.clone(), iface).with_verify_window(Duration::from_millis(20));
    follower.set_valid_counter(SharedValidCounter::new());
    let cache = GsPresenceCache::new();
    let outcome = follower
        .follow("wlan1", 157, Duration::ZERO, Some(149), &cache)
        .await;
    assert_eq!(outcome, FollowOutcome::RevertedNoTraffic);
    assert_eq!(
        setter.calls(),
        vec![("wlan1".to_string(), 157), ("wlan1".to_string(), 149)]
    );
    assert_eq!(
        cache.peer_channel(),
        Some(149),
        "the hint follows the revert"
    );
}

#[test]
fn the_follow_gate_names_each_refusal() {
    let any = std::collections::BTreeSet::new();
    let unii3: std::collections::BTreeSet<u8> = [149, 153, 157].into_iter().collect();
    assert_eq!(hop_follow_refusal(1, true, &any, 36), None);
    assert_eq!(hop_follow_refusal(0, true, &unii3, 157), None);
    assert_eq!(
        hop_follow_refusal(2, true, &any, 157),
        Some("fleet_hop_refused")
    );
    assert_eq!(
        hop_follow_refusal(1, false, &any, 157),
        Some("no_receive_iface")
    );
    assert_eq!(
        hop_follow_refusal(1, true, &unii3, 36),
        Some("channel_not_permitted")
    );
}

#[tokio::test]
async fn hop_announce_drives_the_coordinated_follow() {
    // End-to-end through the dispatch: a verified HopAnnounce echoes the ack,
    // records the channel into the cache (the watchdog's hint), AND drives the
    // follower to retune the receive iface to the announced channel.
    use ados_radio::hop::{build_hop_announce, HopTrigger};
    let cache = GsPresenceCache::new();
    let pair_key = test_key();

    let listen_sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_recv = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let ack_addr = ack_recv.local_addr().unwrap();
    let ack_target = match ack_addr {
        std::net::SocketAddr::V4(a) => (*a.ip(), a.port()),
        _ => unreachable!("ipv4 loopback"),
    };

    let setter = RecordingSetter::ok();
    let iface = Arc::new(tokio::sync::Mutex::new(vec!["wlan1".to_string()]));
    let follower = HopFollower::new(setter.clone(), iface);

    // A zero countdown so the spawned retune fires immediately.
    let announce = build_hop_announce(0, 161, HopTrigger::Reactive, &pair_key);
    handle_control_frame(
        &listen_sock,
        &announce,
        &pair_key,
        &cache,
        "",
        ack_target,
        Some(&follower),
        SOLO,
    )
    .await;

    // The watchdog's channel hint was updated.
    assert_eq!(cache.peer_channel(), Some(161));

    // The retune is spawned, so give it a moment to land, then assert it tuned
    // the resolved iface to the announced channel.
    for _ in 0..50 {
        if !setter.calls().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        setter.calls(),
        vec![("wlan1".to_string(), 161)],
        "the coordinated follow must retune the receive iface to the announced channel"
    );
}

#[test]
fn an_aux_identity_fills_an_idless_beacon_row() {
    // The null-peer case. A node with no persistent id emits an empty one in
    // its beacon, and every downstream reader drops an id-less row, so an
    // audibly-present peer vanishes. The identity frame is the only place the
    // real id can come from.
    let cache = GsPresenceCache::new();
    cache.record_peer(String::new(), "drone".into(), 149, -55, 1);
    assert_eq!(cache.linked_peers()[0].device_id, "");

    cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
    let peers = cache.linked_peers();
    assert_eq!(peers[0].device_id, "drone-a");
    assert_eq!(peers[0].name.as_deref(), Some("Alpha"));
    // The measured radio values are untouched: the identity frame carries no
    // signal information and must not be allowed to invent any.
    assert_eq!(peers[0].channel, 149);
    assert_eq!(peers[0].rssi_dbm, -55);
}

#[test]
fn an_aux_identity_names_a_matching_beacon_row() {
    let cache = GsPresenceCache::new();
    cache.record_peer("drone-a".into(), "drone".into(), 157, -48, 1);
    cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
    let peers = cache.linked_peers();
    assert_eq!(peers[0].device_id, "drone-a");
    assert_eq!(peers[0].name.as_deref(), Some("Alpha"));
}

#[test]
fn an_idless_row_is_not_guessed_at_when_several_peers_are_known() {
    // With more than one identity and no id on the beacon there is no way to
    // tell which is which, and a guess would attach one node's identity to
    // another node's signal.
    let cache = GsPresenceCache::new();
    cache.record_peer(String::new(), "drone".into(), 149, -55, 1);
    cache.set_aux_identities(vec![
        ("drone-a".into(), Some("Alpha".into())),
        ("drone-b".into(), Some("Bravo".into())),
    ]);
    let peers = cache.linked_peers();
    assert_eq!(
        peers[0].device_id, "",
        "an ambiguous id must not be guessed"
    );
    assert_eq!(peers[0].name, None);
}

#[test]
fn an_identity_for_an_unheard_peer_does_not_invent_a_row() {
    // An identity proves the peer is speaking, not that this receiver decoded
    // its beacon. Publishing a row would require a channel and an RSSI that
    // were never measured.
    let cache = GsPresenceCache::new();
    cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
    assert!(cache.linked_peers().is_empty());
}

#[test]
fn a_withdrawn_identity_stops_naming_the_peer() {
    // The setter takes the current fresh set, so a peer that went quiet loses
    // its label rather than keeping it as a confident stale claim.
    let cache = GsPresenceCache::new();
    cache.record_peer("drone-a".into(), "drone".into(), 157, -48, 1);
    cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
    assert_eq!(cache.linked_peers()[0].name.as_deref(), Some("Alpha"));

    cache.set_aux_identities(vec![]);
    assert_eq!(cache.linked_peers()[0].name, None);
}

#[test]
fn the_name_is_omitted_from_the_sidecar_when_absent() {
    // Additive by construction: a peer with no name serializes exactly the
    // shape existing readers already parse.
    let cache = GsPresenceCache::new();
    cache.record_peer("drone-a".into(), "drone".into(), 157, -48, 1);
    let payload = linked_peers_payload(&cache.linked_peers());
    let entry = &payload["peers"][0];
    assert!(entry.get("name").is_none());
    assert_eq!(entry["device_id"], "drone-a");

    cache.set_aux_identities(vec![("drone-a".into(), Some("Alpha".into()))]);
    let payload = linked_peers_payload(&cache.linked_peers());
    assert_eq!(payload["peers"][0]["name"], "Alpha");
}

impl HopFollower {
    /// A follower whose verification window is `window`, for tests.
    fn with_verify_window(mut self, window: Duration) -> Self {
        self.verify_window = window;
        self
    }
}
