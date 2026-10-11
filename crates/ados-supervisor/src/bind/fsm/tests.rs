use super::*;

#[test]
fn state_wire_strings_match_python() {
    assert_eq!(BindState::OpeningTunnel.as_str(), "opening_tunnel");
    assert_eq!(
        BindState::RestartingServices.as_str(),
        "restarting_services"
    );
    // serde rendering must match the manual as_str.
    assert_eq!(
        serde_json::to_value(BindState::TransferringKeys).unwrap(),
        serde_json::json!("transferring_keys")
    );
}

#[test]
fn terminal_set_matches_is_bind_active_semantics() {
    for s in [
        BindState::Idle,
        BindState::Paired,
        BindState::Failed,
        BindState::Aborted,
    ] {
        assert!(s.is_terminal());
        assert!(!s.is_active());
    }
    for s in [
        BindState::OpeningTunnel,
        BindState::WaitingPeer,
        BindState::TransferringKeys,
        BindState::ApplyingKeys,
        BindState::RestartingServices,
    ] {
        assert!(!s.is_terminal());
        assert!(s.is_active());
    }
}

#[test]
fn each_active_phase_has_a_watchdog_budget_terminal_phases_have_none() {
    // The no-progress detector re-arms on every transition using this per-phase
    // budget; every active phase must have one so a wedge in any phase fires,
    // and terminal phases must have None so a finished session is never watched.
    assert_eq!(
        BindState::OpeningTunnel.watchdog_budget(),
        Some(super::super::TUNNEL_WAIT_TIMEOUT)
    );
    assert_eq!(
        BindState::WaitingPeer.watchdog_budget(),
        Some(super::super::WAITING_PEER_WATCHDOG)
    );
    assert_eq!(
        BindState::TransferringKeys.watchdog_budget(),
        Some(super::super::KEY_TRANSFER_TIMEOUT)
    );
    assert_eq!(
        BindState::ApplyingKeys.watchdog_budget(),
        Some(super::super::KEY_TRANSFER_TIMEOUT)
    );
    assert_eq!(
        BindState::RestartingServices.watchdog_budget(),
        Some(super::super::RESTART_TIMEOUT)
    );
    for terminal in [
        BindState::Idle,
        BindState::Paired,
        BindState::Failed,
        BindState::Aborted,
    ] {
        assert_eq!(terminal.watchdog_budget(), None);
    }
}

#[test]
fn phase_timeouts_couple_to_budgets_and_the_watchdog_must_re_arm() {
    use BindState::*;
    // Each active phase's watchdog budget is the SAME constant the
    // orchestrator uses as that phase's explicit tokio timeout, so the
    // honest phase-named error fires at-or-before the generic watchdog (a
    // biased select prefers the in-phase error on a tie). Pin the coupling.
    assert_eq!(
        OpeningTunnel.watchdog_budget(),
        Some(super::super::TUNNEL_WAIT_TIMEOUT)
    );
    assert_eq!(
        TransferringKeys.watchdog_budget(),
        Some(super::super::KEY_TRANSFER_TIMEOUT)
    );
    assert_eq!(
        ApplyingKeys.watchdog_budget(),
        Some(super::super::KEY_TRANSFER_TIMEOUT)
    );
    assert_eq!(
        RestartingServices.watchdog_budget(),
        Some(super::super::RESTART_TIMEOUT)
    );

    // No phase's explicit timeout may EXCEED its watchdog budget, or the
    // generic watchdog could fire before the in-phase honest error.
    assert!(super::super::TUNNEL_WAIT_TIMEOUT <= OpeningTunnel.watchdog_budget().unwrap());
    assert!(super::super::KEY_TRANSFER_TIMEOUT <= TransferringKeys.watchdog_budget().unwrap());
    assert!(super::super::RESTART_TIMEOUT <= RestartingServices.watchdog_budget().unwrap());

    // The worst-case healthy bind runs OpeningTunnel → (key transfer+apply)
    // → RestartingServices in sequence; those budgets SUM past any single
    // phase budget (30+90+60 = 180 > 120). A fixed global timer of one
    // budget would wrongly trip a healthy bind — the historical 120<180
    // wedge — which is exactly why the watchdog re-arms per phase. Guard
    // that the sum still exceeds a single budget so the re-arming design
    // stays necessary and correct.
    let sequential_sum = super::super::TUNNEL_WAIT_TIMEOUT
        + super::super::KEY_TRANSFER_TIMEOUT
        + super::super::RESTART_TIMEOUT;
    assert!(
        sequential_sum > super::super::WAITING_PEER_WATCHDOG,
        "sequential phase budgets ({sequential_sum:?}) must exceed any single \
         budget ({:?}); a fixed global timer would trip a healthy bind",
        super::super::WAITING_PEER_WATCHDOG
    );
}

#[test]
fn re_armed_watchdog_does_not_fire_on_a_phase_within_its_budget() {
    // A transition restamps `phase_entered_at` to now, so a freshly entered
    // phase is nowhere near its budget — the deadline-derived detector must not
    // consider it wedged. (The deadline = phase_entered_at + budget(state).)
    let mut s = BindSession::new(BindRole::Drone, "operator", None);
    s.transition(BindState::TransferringKeys);
    let entered = s.phase_entered_at.expect("transition stamps the clock");
    let budget = BindState::TransferringKeys
        .watchdog_budget()
        .unwrap()
        .as_secs_f64();
    let parked = (now_monotonic() - entered).max(0.0);
    assert!(
        parked < budget,
        "a just-entered phase must be within its budget (parked {parked}s < {budget}s)"
    );
}

#[test]
fn transition_stamps_phase_clock_and_to_json_shape() {
    let mut s = BindSession::new(BindRole::Drone, "operator", None);
    assert!(s.phase_entered_at.is_none());
    s.transition(BindState::OpeningTunnel);
    assert!(s.phase_entered_at.is_some());
    let v = s.to_json();
    // Exact key set the GCS PairingCard reads.
    for k in [
        "session_id",
        "role",
        "state",
        "phase",
        "phase_entered_at",
        "phase_age_s",
        "started_at",
        "finished_at",
        "error",
        "fingerprint",
        "peer_device_id",
        "source",
        "last_frame_at_s",
        "last_rssi_dbm",
        "last_frame_age_s",
    ] {
        assert!(v.get(k).is_some(), "missing key {k}");
    }
    assert_eq!(v["role"], "drone");
    assert_eq!(v["state"], "opening_tunnel");
    assert_eq!(v["source"], "operator");
    assert!(v["phase_age_s"].as_f64().unwrap() >= 0.0);
    // The precheck fields are present and null until the prep stamps them.
    assert!(v.get("bind_precheck_ok").is_some());
    assert!(v["bind_precheck_ok"].is_null());
    assert!(v["injection_mode"].is_null());
    // peer_verified is present and null until the success gate runs.
    assert!(v.get("peer_verified").is_some());
    assert!(v["peer_verified"].is_null());
}

#[test]
fn to_json_renders_peer_verified_when_stamped() {
    let mut s = BindSession::new(BindRole::Gs, "auto", None);
    s.peer_verified = Some(false);
    assert_eq!(s.to_json()["peer_verified"], serde_json::json!(false));
    s.peer_verified = Some(true);
    assert_eq!(s.to_json()["peer_verified"], serde_json::json!(true));
}

#[test]
fn to_json_renders_bind_precheck_when_set() {
    let mut s = BindSession::new(BindRole::Drone, "auto", None);
    s.bind_precheck = Some(BindPrecheck {
        ok: false,
        reason: Some("monitor_unverified"),
        injection_mode: "managed".to_string(),
        nm_enumerable: true,
        iface: Some("wlan1".to_string()),
    });
    let v = s.to_json();
    assert_eq!(v["bind_precheck_ok"], serde_json::json!(false));
    assert_eq!(v["bind_precheck_reason"], "monitor_unverified");
    assert_eq!(v["injection_mode"], "managed");
    assert_eq!(v["nm_enumerable"], serde_json::json!(true));
}

#[test]
fn iso_now_is_second_precision_utc() {
    let ts = iso_now();
    assert!(ts.ends_with("+00:00"), "got {ts}");
    assert_eq!(ts.len(), "2026-05-29T12:34:56+00:00".len());
    assert_eq!(&ts[10..11], "T");
}

#[test]
fn session_id_is_uuid_shaped_and_unique() {
    let a = new_session_id();
    let b = new_session_id();
    assert_ne!(a, b);
    let parts: Vec<&str> = a.split('-').collect();
    assert_eq!(parts.len(), 5);
    assert_eq!(
        parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12]
    );
}
