use super::*;

#[test]
fn drone_server_args_match_python() {
    assert_eq!(
        drone_server_args(),
        vec![
            "-d".to_string(),
            "TCP4-LISTEN:5555,bind=10.5.99.2,reuseaddr,crlf".to_string(),
            "EXEC:/usr/bin/wfb_bind_server.sh".to_string(),
        ]
    );
}

#[test]
fn gs_client_retry_is_bounded_to_the_session_budget() {
    // Today's constants (90 s budget + 5 s margin) still produce retry=95,
    // but it is now DERIVED so it tracks the budget instead of drifting.
    assert_eq!(
        gs_client_args(),
        vec![
            "-d".to_string(),
            "TCP4:10.5.99.2:5555,crlf,retry=95,interval=1".to_string(),
            "EXEC:/usr/bin/wfb_bind_client.sh".to_string(),
        ]
    );
}

#[test]
fn gs_client_retry_couples_to_the_transfer_budget_and_stays_bounded() {
    // The roaming-client guard: the retry must OUTLAST the transfer budget
    // (so a real, slightly-late peer still connects) yet stay bounded to a
    // handful of seconds past it — never the predecessor's 24 h (86_400 s)
    // that let a leaked client phantom-EOF a LATER drone's listener.
    let retry = gs_client_retry();
    let budget = KEY_TRANSFER_TIMEOUT.as_secs();
    assert!(
        retry > budget,
        "retry ({retry}) must outlast the transfer budget ({budget})"
    );
    assert!(
        retry <= budget + 30,
        "retry ({retry}) must die promptly after the budget, not roam"
    );
}
