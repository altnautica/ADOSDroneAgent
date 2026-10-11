use super::*;

#[tokio::test]
async fn reconcile_global_domain_runs_without_a_logd_socket() {
    // The shared reconcile (used by both the periodic tick and the post-bind
    // immediate re-assert) must never panic when the logd socket is absent.
    // On a non-Linux dev host it is an inert no-op; on Linux CI it shells
    // `iw` read-only and falls through safely when the wanted domain is
    // already in sync or the tools are unavailable. Either way: no panic.
    let events = EventEmitter::with_socket("ados-test", "/nonexistent/ados/logd.sock");
    reconcile_global_domain(&events).await;
}

#[test]
fn reassert_detail_is_bland_and_complete() {
    let d = reg_reassert_detail(Some("BO"), "US", 149, true);
    assert_eq!(d.get("source").and_then(|v| v.as_str()), Some("supervisor"));
    assert_eq!(d.get("from_country").and_then(|v| v.as_str()), Some("BO"));
    assert_eq!(d.get("to_country").and_then(|v| v.as_str()), Some("US"));
    assert_eq!(d.get("wfb_channel").and_then(|v| v.as_u64()), Some(149));
    assert_eq!(
        d.get("channel_permitted").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn reassert_detail_omits_from_when_unreadable() {
    let d = reg_reassert_detail(None, "US", 149, true);
    assert!(!d.contains_key("from_country"));
}
