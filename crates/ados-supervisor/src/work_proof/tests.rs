use super::*;

#[test]
fn a_flat_counter_is_reported_stalled_once_the_window_elapses() {
    let mut wp = WorkProof::new();
    let t0 = Instant::now();
    // First sample only establishes the baseline.
    assert_eq!(
        wp.observe("ados-mavlink", Some(1_000), t0),
        WorkVerdict::Progressing
    );
    // Still flat, but inside the window: not yet a verdict.
    assert_eq!(
        wp.observe("ados-mavlink", Some(1_000), t0 + Duration::from_secs(20)),
        WorkVerdict::Progressing
    );
    // Flat across the whole window: the router is alive and moving nothing.
    assert_eq!(
        wp.observe("ados-mavlink", Some(1_000), t0 + STALL_WINDOW),
        WorkVerdict::Stalled {
            flat_for: STALL_WINDOW
        }
    );
}

#[test]
fn a_moving_counter_restarts_the_window() {
    let mut wp = WorkProof::new();
    let t0 = Instant::now();
    wp.observe("ados-crsf", Some(10), t0);
    assert_eq!(
        wp.observe("ados-crsf", Some(11), t0 + Duration::from_secs(29)),
        WorkVerdict::Progressing
    );
    // The window runs from the move, not from the first sample.
    assert_eq!(
        wp.observe("ados-crsf", Some(11), t0 + Duration::from_secs(31)),
        WorkVerdict::Progressing
    );
    assert_eq!(
        wp.observe(
            "ados-crsf",
            Some(11),
            t0 + Duration::from_secs(29) + STALL_WINDOW
        ),
        WorkVerdict::Stalled {
            flat_for: STALL_WINDOW
        }
    );
}

#[test]
fn an_unreadable_counter_never_condemns_and_does_not_accumulate() {
    let mut wp = WorkProof::new();
    let t0 = Instant::now();
    wp.observe("ados-vision", Some(5), t0);
    // A read failure mid-window.
    assert_eq!(
        wp.observe("ados-vision", None, t0 + Duration::from_secs(10)),
        WorkVerdict::Unknown
    );
    // The window restarted: the 10 s already served does not carry over,
    // so the very next reading cannot be a stall however old the process is.
    assert_eq!(
        wp.observe("ados-vision", Some(5), t0 + Duration::from_secs(11)),
        WorkVerdict::Progressing
    );
    assert_eq!(
        wp.observe("ados-vision", Some(5), t0 + Duration::from_secs(35)),
        WorkVerdict::Progressing
    );
}

#[test]
fn a_counter_that_went_backwards_rebaselines_instead_of_stalling() {
    // The unit was restarted out from under us: the replacement process
    // starts at zero. Comparing against the dead process's total would
    // otherwise hold the old `flat_since` and condemn a fresh, healthy
    // process at the next window.
    let mut wp = WorkProof::new();
    let t0 = Instant::now();
    wp.observe("ados-swarmbus", Some(9_000_000), t0);
    assert_eq!(
        wp.observe("ados-swarmbus", Some(12), t0 + Duration::from_secs(5)),
        WorkVerdict::Progressing
    );
    assert_eq!(
        wp.observe("ados-swarmbus", Some(12), t0 + Duration::from_secs(20)),
        WorkVerdict::Progressing
    );
}

#[test]
fn forget_clears_the_baseline_so_a_restart_starts_a_fresh_window() {
    let mut wp = WorkProof::new();
    let t0 = Instant::now();
    wp.observe("ados-mavlink", Some(1), t0);
    wp.forget("ados-mavlink");
    assert_eq!(
        wp.observe("ados-mavlink", Some(1), t0 + STALL_WINDOW * 3),
        WorkVerdict::Progressing
    );
}

#[test]
fn units_are_judged_independently() {
    let mut wp = WorkProof::new();
    let t0 = Instant::now();
    wp.observe("ados-mavlink", Some(1), t0);
    wp.observe("ados-crsf", Some(1), t0);
    wp.observe("ados-crsf", Some(2), t0 + Duration::from_secs(10));
    assert_eq!(
        wp.observe("ados-mavlink", Some(1), t0 + STALL_WINDOW),
        WorkVerdict::Stalled {
            flat_for: STALL_WINDOW
        }
    );
    assert_eq!(
        wp.observe("ados-crsf", Some(3), t0 + STALL_WINDOW),
        WorkVerdict::Progressing
    );
}

#[test]
fn the_work_proven_set_is_the_four_lanes_whose_silence_is_invisible() {
    assert!(requires_work_proof("ados-mavlink"));
    assert!(requires_work_proof("ados-swarmbus"));
    assert!(requires_work_proof("ados-crsf"));
    assert!(requires_work_proof("ados-vision"));
    // Units with their own in-daemon delta assertions are not double-judged.
    assert!(!requires_work_proof("ados-video"));
    assert!(!requires_work_proof("ados-wfb"));
    assert!(!requires_work_proof("ados-logd"));
}

#[test]
fn the_router_is_judged_on_decoded_frames_only_while_its_fc_link_is_open() {
    use serde_json::json;
    let open = json!({"transport_open": true, "fc_variant": null, "fc_frames_decoded": 120});
    assert_eq!(router_work_counter(&open), Some(120));
    // Between reconnect attempts: the router's own loop owns that, no verdict.
    let closed = json!({"transport_open": false, "fc_frames_decoded": 120});
    assert_eq!(router_work_counter(&closed), None);
    // An MSP board answers only when polled, so a flat count is idle, not wedged.
    let msp = json!({"transport_open": true, "fc_variant": "betaflight", "fc_frames_decoded": 0});
    assert_eq!(router_work_counter(&msp), None);
    // A snapshot without the counter is no reading.
    assert_eq!(router_work_counter(&json!({"transport_open": true})), None);
}

#[tokio::test]
async fn a_router_whose_frame_count_stops_is_stalled_while_it_keeps_publishing() {
    // The wedged-reader case: the router keeps answering on its state
    // socket (its process I/O never goes flat), but the frame count does.
    use tokio::io::AsyncWriteExt;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("state.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let frame = ados_protocol::state::encode_v2(&serde_json::json!({
        "transport_open": true,
        "fc_variant": null,
        "fc_frames_decoded": 4096,
    }))
    .unwrap();
    tokio::spawn(async move {
        while let Ok((mut peer, _)) = listener.accept().await {
            let _ = peer.write_all(&frame).await;
        }
    });

    let mut wp = WorkProof::new();
    let t0 = Instant::now();
    let mut verdict = WorkVerdict::Unknown;
    for pass in 0..=6u32 {
        let reading = read_router_work_counter(&sock).await;
        assert_eq!(reading, Some(4096));
        verdict = wp.observe(ROUTER_UNIT, reading, t0 + Duration::from_secs(5) * pass);
    }
    assert_eq!(
        verdict,
        WorkVerdict::Stalled {
            flat_for: STALL_WINDOW
        }
    );
    // No socket at all is no reading, never a stall.
    assert_eq!(
        read_router_work_counter(&dir.path().join("absent.sock")).await,
        None
    );
}

#[tokio::test]
async fn the_vision_engine_is_judged_on_frames_consumed_while_its_input_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("vision-status.json");
    let write = |frames: u64, live: u32| {
        std::fs::write(
            &sidecar,
            format!(r#"{{"version":1,"frames_consumed":{frames},"live_inputs":{live}}}"#),
        )
        .unwrap();
    };
    let mut wp = WorkProof::new();
    let t0 = Instant::now();

    // A fed engine: its process I/O is flat (every frame arrives over a
    // socket) but the consumed-frame count moves, so it is never stalled.
    let mut verdict = WorkVerdict::Unknown;
    for pass in 0..=8u32 {
        write(100 + u64::from(pass) * 50, 1);
        let reading = read_vision_work_counter(&sidecar).await;
        assert_eq!(reading, Some(100 + u64::from(pass) * 50));
        verdict = wp.observe(VISION_UNIT, reading, t0 + Duration::from_secs(5) * pass);
    }
    assert_eq!(verdict, WorkVerdict::Progressing);

    // No input delivering (no camera, or the tap went silent): no verdict,
    // however long it lasts.
    write(500, 0);
    assert_eq!(read_vision_work_counter(&sidecar).await, None);

    // A live input with a frozen count: the engine stopped consuming.
    let mut wp = WorkProof::new();
    write(900, 1);
    let mut verdict = WorkVerdict::Unknown;
    for pass in 0..=6u32 {
        let reading = read_vision_work_counter(&sidecar).await;
        verdict = wp.observe(VISION_UNIT, reading, t0 + Duration::from_secs(5) * pass);
    }
    assert_eq!(
        verdict,
        WorkVerdict::Stalled {
            flat_for: STALL_WINDOW
        }
    );

    // No sidecar at all (vision off, or not started yet): no reading.
    assert_eq!(
        read_vision_work_counter(&dir.path().join("absent.json")).await,
        None
    );
}

/// A published swarm table with the fields the counter reads.
fn swarm_table(slot: u8, open: bool, tx: u64, rx: u64, neighbors: u64) -> Value {
    serde_json::json!({
        "fleet_id": 1,
        "slot": slot,
        "counters": {"beacons_tx": tx, "beacons_rx": rx, "neighbors_now": neighbors},
        "radio": {"open": open, "iface": if open { Some("wlan1") } else { None }},
    })
}

#[test]
fn the_swarm_bus_is_judged_on_beacons_only_while_it_has_beacons_to_move() {
    // A drone with its radio open beacons at 2 Hz: sent plus heard.
    assert_eq!(
        swarm_work_counter(&swarm_table(3, true, 40, 12, 1)),
        Some(52)
    );
    assert_eq!(
        swarm_work_counter(&swarm_table(3, true, 40, 0, 0)),
        Some(40)
    );
    // No radio: the radio supervisor is reopening it; no verdict.
    assert_eq!(swarm_work_counter(&swarm_table(3, false, 40, 0, 0)), None);
    // A ground station only listens: with nobody in range it is idle.
    assert_eq!(swarm_work_counter(&swarm_table(0, true, 0, 900, 0)), None);
    // Hearing a neighbour, it has frames to accept.
    assert_eq!(
        swarm_work_counter(&swarm_table(0, true, 0, 900, 2)),
        Some(900)
    );
    // The empty table an absent bus is served as carries no verdict.
    assert_eq!(
        swarm_work_counter(&serde_json::json!({"radio": null, "counters": {}})),
        None
    );
}

#[tokio::test]
async fn a_drone_bus_whose_beacons_stop_is_stalled_while_it_keeps_publishing() {
    // The bus keeps answering on its table socket at 2 Hz (its process I/O
    // never says anything useful: beacons leave over a packet socket), but
    // its beacon count is frozen.
    use tokio::io::AsyncWriteExt;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("swarm.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let line = format!("{}\n", swarm_table(2, true, 310, 0, 0));
    tokio::spawn(async move {
        while let Ok((mut peer, _)) = listener.accept().await {
            let _ = peer.write_all(line.as_bytes()).await;
        }
    });

    let mut wp = WorkProof::new();
    let t0 = Instant::now();
    let mut verdict = WorkVerdict::Unknown;
    for pass in 0..=6u32 {
        let reading = read_swarm_work_counter(&sock).await;
        assert_eq!(reading, Some(310));
        verdict = wp.observe(SWARMBUS_UNIT, reading, t0 + Duration::from_secs(5) * pass);
    }
    assert_eq!(
        verdict,
        WorkVerdict::Stalled {
            flat_for: STALL_WINDOW
        }
    );
    assert_eq!(
        read_swarm_work_counter(&dir.path().join("absent.sock")).await,
        None
    );
}

#[tokio::test]
async fn an_idle_crsf_lane_is_never_judged_and_a_live_one_is_judged_on_rc_frames() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("crsf-stats.json");
    let write = |source: Option<&str>, total: Option<u64>| {
        std::fs::write(
            &sidecar,
            serde_json::json!({
                "v": 1, "state": "ready", "channel_source": source,
                "tx_frames_total": total, "tx_frames_per_s": 0.0,
            })
            .to_string(),
        )
        .unwrap();
    };

    // No live source: the lane sends no RC frames so the receiver's
    // failsafe runs. A flat count however long is idle, never a stall.
    let mut wp = WorkProof::new();
    let t0 = Instant::now();
    write(None, Some(5_000));
    for pass in 0..=12u32 {
        let reading = read_crsf_work_counter(&sidecar).await;
        assert_eq!(reading, None);
        assert_eq!(
            wp.observe(CRSF_UNIT, reading, t0 + Duration::from_secs(5) * pass),
            WorkVerdict::Unknown
        );
    }
    // A lane standing by for another mode carries no count at all.
    write(Some("hid"), None);
    assert_eq!(read_crsf_work_counter(&sidecar).await, None);

    // A live source: the RC frame count is the verdict, and a frozen one
    // across the window is a stalled lane.
    write(Some("hid"), Some(5_000));
    let mut verdict = WorkVerdict::Unknown;
    for pass in 0..=6u32 {
        let reading = read_crsf_work_counter(&sidecar).await;
        assert_eq!(reading, Some(5_000));
        verdict = wp.observe(CRSF_UNIT, reading, t0 + Duration::from_secs(5) * pass);
    }
    assert_eq!(
        verdict,
        WorkVerdict::Stalled {
            flat_for: STALL_WINDOW
        }
    );
    assert_eq!(
        read_crsf_work_counter(&dir.path().join("absent.json")).await,
        None
    );
}
