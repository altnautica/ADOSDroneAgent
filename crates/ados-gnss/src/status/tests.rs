use super::*;

const S: u64 = 1_000_000_000;

fn rel(valid: bool) -> RelPosNed {
    RelPosNed {
        heading_deg: -10.0,
        acc_heading_deg: 0.3,
        length_m: 1.02,
        gnss_fix_ok: true,
        rel_pos_valid: true,
        is_moving: true,
        heading_valid: valid,
    }
}

#[test]
fn rtcm_rate_uses_a_two_second_window() {
    let mut c = RtcmCounter::default();
    for i in 0..6 {
        c.record(10 * S + i * 500_000_000, 100);
    }
    // 10.0 .. 12.5: at 12.5 the window (10.5, 12.5] holds 5 messages.
    let v = c.view(12 * S + 500_000_000, 3);
    assert_eq!(v.bytes_total, 600);
    assert!((v.msgs_per_s - 2.5).abs() < 1e-9);
    assert_eq!(v.targets, 3);
    let idle = c.view(30 * S, 0);
    assert_eq!((idle.msgs_per_s, idle.bytes_total), (0.0, 600));
}

#[test]
fn heading_needs_validity_freshness_and_the_config_gate() {
    let mut h = Heading::default();
    let off = h.view(0, true);
    assert!(!off.available && off.deg.is_none() && off.baseline_m.is_none());
    h.on_relposned(S, rel(true));
    let v = h.view(2 * S, true);
    assert!(v.available);
    assert!((v.deg.unwrap() - 350.0).abs() < 1e-9);
    assert_eq!((v.accuracy_deg, v.baseline_m), (Some(0.3), Some(1.02)));
    assert!(!h.view(2 * S, false).available);
    assert!(!h.view(S + HEADING_STALE_NS + 1, true).available);
    h.on_relposned(3 * S, rel(false));
    assert!(!h.view(3 * S, true).available);
}

#[test]
fn linked_peers_counts_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("linked-peers.json");
    assert_eq!(linked_peer_count(&path), 0);
    std::fs::write(
        &path,
        r#"{"version":1,"wall_time_unix":1,"peers":[{"device_id":"a"},{"device_id":"b"}]}"#,
    )
    .unwrap();
    assert_eq!(linked_peer_count(&path), 2);
    std::fs::write(&path, "not json").unwrap();
    assert_eq!(linked_peer_count(&path), 0);
}
