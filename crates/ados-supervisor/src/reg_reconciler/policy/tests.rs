use super::*;

// ----- forceable-domain predicate -----

#[test]
fn forceable_domain_predicate() {
    assert!(is_forceable_domain("US"));
    assert!(is_forceable_domain("IN"));
    assert!(is_forceable_domain("BO"));
    // World default is never forced (would cap the radio).
    assert!(!is_forceable_domain("00"));
    assert!(!is_forceable_domain("USA"));
    assert!(!is_forceable_domain(""));
}

// ----- pure reconcile policy -----

#[test]
fn in_sync_no_op() {
    assert_eq!(
        reconcile_decision(Some("US"), "US", true),
        ReconcileDecision::InSync
    );
    assert_eq!(
        reconcile_decision(Some("us"), "US", true),
        ReconcileDecision::InSync
    );
}

#[test]
fn reassert_away_from_bolivia() {
    assert_eq!(
        reconcile_decision(Some("BO"), "US", true),
        ReconcileDecision::Reassert {
            from: Some("BO".to_string()),
            to: "US".to_string(),
        }
    );
}

#[test]
fn reassert_when_live_unreadable() {
    assert_eq!(
        reconcile_decision(None, "IN", true),
        ReconcileDecision::Reassert {
            from: None,
            to: "IN".to_string(),
        }
    );
}

#[test]
fn skip_when_channel_not_permitted_by_wanted() {
    assert_eq!(
        reconcile_decision(Some("BO"), "US", false),
        ReconcileDecision::SkipChannelUnsafe
    );
}

#[test]
fn never_force_world_or_malformed() {
    assert_eq!(
        reconcile_decision(Some("BO"), "00", true),
        ReconcileDecision::NoWanted
    );
    assert_eq!(
        reconcile_decision(Some("BO"), "", true),
        ReconcileDecision::NoWanted
    );
}

#[test]
fn never_forces_bolivia_as_target() {
    // Even if BO is somehow the live value, the reconcile only ever moves
    // TOWARD the configured (sane) wanted domain, never toward BO.
    match reconcile_decision(Some("BO"), "IN", true) {
        ReconcileDecision::Reassert { to, .. } => assert_eq!(to, "IN"),
        other => panic!("expected re-assert to IN, got {other:?}"),
    }
}
