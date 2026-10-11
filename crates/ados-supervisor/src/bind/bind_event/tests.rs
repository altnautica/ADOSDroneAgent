use super::*;

#[test]
fn reason_codes_are_bland_and_stable() {
    assert_eq!(BindFailReason::NoTxKey.as_str(), "no_tx_key");
    assert_eq!(BindFailReason::RegBlocked.as_str(), "reg_blocked");
    assert_eq!(BindFailReason::NoPeer.as_str(), "no_peer");
    assert_eq!(BindFailReason::NoPeerProof.as_str(), "no_peer_proof");
    assert_eq!(BindFailReason::StaleKey.as_str(), "stale_key");
    assert_eq!(BindFailReason::Timeout.as_str(), "timeout");
    assert_eq!(BindFailReason::Interrupted.as_str(), "interrupted");
    assert_eq!(BindFailReason::Other.as_str(), "other");
}

#[test]
fn classify_error_maps_recognisable_causes() {
    assert_eq!(
        BindFailReason::classify_error("upstream wfb-ng artifact missing: /etc/bind.key", None),
        BindFailReason::NoTxKey
    );
    assert_eq!(
        BindFailReason::classify_error("no progress", Some("waiting_peer")),
        BindFailReason::NoPeer
    );
    assert_eq!(
        BindFailReason::classify_error("peer did not connect over the tunnel", None),
        BindFailReason::NoPeer
    );
    assert_eq!(
        BindFailReason::classify_error("socat client exited rc=1", Some("transferring_keys")),
        BindFailReason::Other
    );
}

#[test]
fn started_detail_omits_peer_when_absent() {
    let d = bind_started_detail("drone", "auto", "abcd1234", None);
    assert_eq!(d.get("role").and_then(|v| v.as_str()), Some("drone"));
    assert_eq!(d.get("source").and_then(|v| v.as_str()), Some("auto"));
    assert_eq!(
        d.get("session_id").and_then(|v| v.as_str()),
        Some("abcd1234")
    );
    assert!(!d.contains_key("peer_device_id"));

    let with_peer = bind_started_detail("gs", "operator", "ef56", Some("dev-7"));
    assert_eq!(
        with_peer.get("peer_device_id").and_then(|v| v.as_str()),
        Some("dev-7")
    );
}

#[test]
fn precheck_detail_carries_mode_and_omits_absent_optionals() {
    let ok = bind_precheck_detail("drone", true, None, "monitor", true, Some("wlan1"));
    assert_eq!(ok.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        ok.get("injection_mode").and_then(|v| v.as_str()),
        Some("monitor")
    );
    assert_eq!(ok.get("iface").and_then(|v| v.as_str()), Some("wlan1"));
    assert!(!ok.contains_key("reason"));

    let bad = bind_precheck_detail(
        "gs",
        false,
        Some("monitor_unverified"),
        "managed",
        false,
        None,
    );
    assert_eq!(bad.get("ok").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(
        bad.get("reason").and_then(|v| v.as_str()),
        Some("monitor_unverified")
    );
    assert_eq!(
        bad.get("nm_enumerable").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert!(!bad.contains_key("iface"));
}

#[test]
fn failed_detail_carries_reason_phase_and_elapsed() {
    let d = bind_failed_detail(
        "gs",
        BindFailReason::Timeout,
        Some("waiting_peer"),
        "watchdog fired after 120s with no progress",
        "abcd1234",
        120,
    );
    assert_eq!(d.get("role").and_then(|v| v.as_str()), Some("gs"));
    assert_eq!(d.get("reason").and_then(|v| v.as_str()), Some("timeout"));
    assert_eq!(
        d.get("phase").and_then(|v| v.as_str()),
        Some("waiting_peer")
    );
    assert_eq!(d.get("elapsed_s").and_then(|v| v.as_u64()), Some(120));
    assert!(d.get("message").and_then(|v| v.as_str()).is_some());
}
