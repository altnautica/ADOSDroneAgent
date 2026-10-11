use super::*;
use crate::hop::{build_hop_announce, derive_pair_key, parse_hop_announce, HopState, PeerPresence};

/// A decoded ground-station beacon, as the listener records it.
fn gs_beacon() -> PeerPresence {
    PeerPresence {
        device_id: "gs-0001".to_string(),
        role: "gs".to_string(),
        channel: 149,
        rssi_dbm: -50,
    }
}

/// The enabled-channel set helper for the verdict tests: the U-NII-3 home
/// band so a request for 153 is in-set and a request for 36 is out-of-set.
fn unii3_set() -> std::collections::BTreeSet<u8> {
    [149u8, 153, 157, 161, 165].into_iter().collect()
}

#[test]
fn manual_hop_rejected_when_not_paired() {
    // Not paired wins first, regardless of peer/bind/channel.
    let v = manual_hop_verdict(153, false, false, true, Some(&unii3_set()));
    assert_eq!(
        v,
        HopVerdict::Rejected {
            reason: "not paired"
        }
    );
}

#[test]
fn manual_hop_rejected_when_no_peer_or_stale_peer() {
    // Paired but no live peer → no peer (the GS can't ACK the announce).
    let v = manual_hop_verdict(153, true, false, false, Some(&unii3_set()));
    assert_eq!(v, HopVerdict::Rejected { reason: "no peer" });
}

#[test]
fn manual_hop_rejected_mid_bind() {
    // Paired + peer ready but a bind owns the adapter → mid-bind.
    let v = manual_hop_verdict(153, true, true, true, Some(&unii3_set()));
    assert_eq!(v, HopVerdict::Rejected { reason: "mid-bind" });
}

#[test]
fn manual_hop_rejected_channel_not_in_enabled_set() {
    // 36 is a valid WFB channel (the socket accepts the format) but it is not
    // in the U-NII-3 enabled set this rig holds, so the supervisor refuses it
    // to avoid splitting the pair onto a forbidden frequency.
    let v = manual_hop_verdict(36, true, false, true, Some(&unii3_set()));
    assert_eq!(
        v,
        HopVerdict::Rejected {
            reason: "invalid channel"
        }
    );
}

#[test]
fn manual_hop_accepted_when_paired_peer_ready_in_band() {
    let v = manual_hop_verdict(157, true, false, true, Some(&unii3_set()));
    assert_eq!(v, HopVerdict::Accepted { channel: 157 });
}

#[test]
fn manual_hop_unknown_enabled_set_does_not_restrict() {
    // A None or empty enabled set means the wiphy list was unreadable → do
    // not restrict (the same gate the periodic/reactive target filter uses).
    let v = manual_hop_verdict(36, true, false, true, None);
    assert_eq!(v, HopVerdict::Accepted { channel: 36 });
    let empty = std::collections::BTreeSet::new();
    let v = manual_hop_verdict(36, true, false, true, Some(&empty));
    assert_eq!(v, HopVerdict::Accepted { channel: 36 });
}

/// The manual-hop path drives `try_execute_hop` through the SAME announce
/// path the periodic/reactive triggers use, so the GS follows. Rather than
/// stand up the radio (set_channel/respawn fork real binaries), this asserts
/// the exact announce `try_execute_hop` sends for a manual hop — built with
/// `HopTrigger::Manual` — decodes on the GS-side `parse_hop_announce` as the
/// "manual" trigger on the announce wire. The announce is what the GS hears
/// and ACKs; a coordinated follow is impossible if the trigger byte is wrong.
#[tokio::test]
async fn manual_trigger_announce_is_decodable_by_the_gs_follow_path() {
    let key = derive_pair_key(None);
    // The drone-side announce socket and a loopback listener standing in for
    // the GS follow path's control-plane reader (the real GS binds 5803 via
    // wfb_rx; here a plain UDP socket proves the wire).
    let announce = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let gs = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let gs_addr = gs.local_addr().unwrap();

    // Exactly what try_execute_hop builds + sends for a manual hop.
    let pkt = build_hop_announce(
        HOP_COUNTDOWN.as_millis() as u64,
        153,
        HopTrigger::Manual,
        &key,
    );
    announce.send_to(&pkt, gs_addr).await.unwrap();

    let mut buf = [0u8; 128];
    let (n, _) = gs.recv_from(&mut buf).await.unwrap();
    // The GS follow path decodes the announce; a manual trigger must surface
    // as "manual" (so the follow is recorded honestly) on the requested
    // channel.
    assert_eq!(parse_hop_announce(&buf[..n], &key), Some((153, "manual")));
}

fn tripping_link() -> LinkStats {
    // Real data: non-empty timestamp + packets flowing, with loss/RSSI past
    // the default thresholds (loss 20% > 10, rssi -80 < -75).
    LinkStats {
        timestamp: "2026-05-30T00:00:00+00:00".to_string(),
        packets_received: 500,
        loss_percent: 20.0,
        rssi_dbm: -80.0,
        ..LinkStats::default()
    }
}

/// The hop-period floor applied at the hop-supervisor tick build site. A
/// scan locks the radio for several seconds, so the period must never fall
/// below the floor (and a 0 would panic `interval`). Pure to mirror the
/// inline `.max(HOP_PERIOD_FLOOR_SECS)` without standing up the supervisor.
fn floored_hop_period(configured: u32) -> u64 {
    const HOP_PERIOD_FLOOR_SECS: u64 = 15;
    (configured as u64).max(HOP_PERIOD_FLOOR_SECS)
}

#[test]
fn hop_period_floor_clamps_low_values() {
    // A zero (which would panic tokio::time::interval) and any sub-floor
    // value clamp UP to the 15 s floor.
    assert_eq!(floored_hop_period(0), 15);
    assert_eq!(floored_hop_period(1), 15);
    assert_eq!(floored_hop_period(14), 15);
    // Exactly the floor stays the floor.
    assert_eq!(floored_hop_period(15), 15);
}

#[test]
fn hop_period_floor_leaves_longer_periods_untouched() {
    // The default (60 s) and any value above the floor pass through.
    assert_eq!(floored_hop_period(16), 16);
    assert_eq!(floored_hop_period(60), 60);
    assert_eq!(floored_hop_period(300), 300);
}

#[test]
fn default_link_stats_never_fires_reactive() {
    // The default LinkStats has rssi -100 (< -75) but no real data (empty
    // timestamp, 0 packets) so the gate must NOT fire. This is the
    // drone-only-rig case that would otherwise hop every cycle forever.
    let link = LinkStats::default();
    assert!(!reactive_should_fire(true, &link, 10.0, -75.0));
}

#[test]
fn real_data_over_threshold_fires_once_then_cooldown_blocks() {
    let link = tripping_link();
    // First fire: cooldown allowed + real data over threshold.
    assert!(reactive_should_fire(true, &link, 10.0, -75.0));
    // Cooldown not yet met (a hop just happened) so blocked even though the
    // link still trips the thresholds.
    assert!(!reactive_should_fire(false, &link, 10.0, -75.0));
}

#[test]
fn real_data_under_threshold_does_not_fire() {
    // Real data but a healthy link (low loss, strong RSSI) so no reactive hop.
    let link = LinkStats {
        timestamp: "2026-05-30T00:00:00+00:00".to_string(),
        packets_received: 500,
        loss_percent: 2.0,
        rssi_dbm: -55.0,
        ..LinkStats::default()
    };
    assert!(!reactive_should_fire(true, &link, 10.0, -75.0));
}

#[test]
fn reactive_cooldown_observed_through_hop_state() {
    // A freshly recorded hop blocks the next reactive for 30 s. Pair the
    // HopState cooldown (reactive_allowed) with the predicate to confirm the
    // loop's actual gate respects the cooldown.
    let mut s = HopState::new(149);
    s.on_peer_beacon(gs_beacon());
    // Before any hop the cooldown is met (None last_hop_at).
    assert!(s.reactive_allowed());
    assert!(reactive_should_fire(
        s.reactive_allowed(),
        &tripping_link(),
        10.0,
        -75.0
    ));
    // Record a hop so the 30 s cooldown starts and reactive is blocked.
    s.record_hop(153, "reactive", true);
    assert!(!s.reactive_allowed());
    assert!(!reactive_should_fire(
        s.reactive_allowed(),
        &tripping_link(),
        10.0,
        -75.0
    ));
}

#[test]
fn fresh_peer_periodic_skips_scan_unseen_does_not() {
    // The periodic arm skips the scan when the peer is fresh (<60 s); a peer
    // never seen lets the scan run. peer_fresh_within is the gate the arm
    // reads (using only the public HopState surface).
    let never = HopState::new(149);
    assert!(!never.peer_fresh_within(PEER_FRESH_SKIP_SECS));

    let mut seen = HopState::new(149);
    seen.on_peer_beacon(gs_beacon());
    // Just-seen peer is fresh so the periodic scan is skipped.
    assert!(seen.peer_fresh_within(PEER_FRESH_SKIP_SECS));
}

#[test]
fn self_beacon_matches_first_16_bytes() {
    // A loopback copy of this rig's own beacon carries the first 16 bytes of
    // its device-id; the listener must drop it.
    let own = "0123456789abcdef0123"; // 20 chars
    assert!(is_self_beacon(own, "0123456789abcdef"));
    // A different device-id is a real peer, never dropped.
    assert!(!is_self_beacon(own, "fedcba9876543210"));
    // A short own id (≤16) matches itself verbatim.
    assert!(is_self_beacon("abc123", "abc123"));
    // An empty own id can never self-collide.
    assert!(!is_self_beacon("", ""));
    assert!(!is_self_beacon("", "anything"));
}

#[test]
fn periodic_hop_is_opt_in_and_defaults_off() {
    // A default rig has auto_hop_enabled but periodic_hop_enabled off: the
    // unattended periodic-execution path is suppressed with the opt-in reason,
    // and the effective gate the supervisor reads is false.
    let cfg = WfbConfig::default();
    assert!(cfg.auto_hop_enabled);
    assert!(!cfg.periodic_hop_enabled);
    assert_eq!(
        periodic_hop_suppression_reason(&cfg),
        Some("periodic_hop_opt_in_off")
    );
    // The effective gate (auto_hop_enabled && periodic_hop_enabled) is false.
    assert!(!(cfg.auto_hop_enabled && cfg.periodic_hop_enabled));
}

#[test]
fn periodic_hop_suppression_names_the_real_reason() {
    // auto_hop off → the auto-hop reason wins (the whole supervisor is off).
    let auto_off = WfbConfig {
        auto_hop_enabled: false,
        periodic_hop_enabled: true,
        ..WfbConfig::default()
    };
    assert_eq!(
        periodic_hop_suppression_reason(&auto_off),
        Some("auto_hop_disabled")
    );
    // Both on → no suppression (the periodic path is active).
    let both_on = WfbConfig {
        auto_hop_enabled: true,
        periodic_hop_enabled: true,
        ..WfbConfig::default()
    };
    assert_eq!(periodic_hop_suppression_reason(&both_on), None);
}

#[test]
fn periodic_skip_window_is_below_stale_so_the_path_is_reachable() {
    // The periodic hop requires `can_hop()` (peer fresh < PEER_STALE_SECS) AND
    // NOT `peer_fresh_within(PEER_FRESH_SKIP_SECS)`. With the skip window below
    // the stale window there is a real window where both hold: a peer last
    // seen between the skip window and the stale threshold passes can_hop yet
    // is no longer "fresh", so the periodic scan runs (when opted in). If the
    // skip window were >= the stale threshold the path would be unreachable,
    // because a non-stale peer is always inside the (larger) skip window.
    const {
        assert!(
            PEER_FRESH_SKIP_SECS < crate::hop::PEER_STALE_SECS,
            "skip window must be below the stale threshold for the periodic path to be reachable"
        );
    }
    // The reachable in-window peer state (skip < age < stale) is exercised
    // over the public HopState surface in `hop::tests`, which can backdate the
    // peer-seen clock; here the constant ordering is the load-bearing invariant.
}
