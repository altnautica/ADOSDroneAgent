use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A manually advanced clock. `monotonic()` is the stored seconds value;
/// `wall_ms()` is derived from it so emitted timestamps move with the test.
#[derive(Clone, Default)]
struct FakeClock {
    secs_millis: Arc<AtomicU64>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            secs_millis: Arc::new(AtomicU64::new(0)),
        }
    }
    fn advance(&self, secs: f64) {
        let add = (secs * 1000.0) as u64;
        self.secs_millis.fetch_add(add, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn monotonic(&self) -> f64 {
        self.secs_millis.load(Ordering::SeqCst) as f64 / 1000.0
    }
    fn wall_ms(&self) -> i64 {
        self.secs_millis.load(Ordering::SeqCst) as i64
    }
}

fn arbiter() -> (PicArbiter, FakeClock) {
    let clock = FakeClock::new();
    let arb = PicArbiter::with_clock(Box::new(clock.clone()));
    (arb, clock)
}

#[test]
fn case1_fresh_claim_from_unclaimed() {
    let (mut a, _c) = arbiter();
    let out = a.claim("op-a", None, false);
    assert_eq!(
        out,
        ClaimOutcome::Fresh {
            claimed_by: "op-a".into(),
            claim_counter: 1,
        }
    );
    assert!(out.claimed());
    assert_eq!(out.http_status(), 200);
    assert_eq!(a.get_state().state, PicState::Claimed);
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
}

#[test]
fn case2_same_client_is_idempotent() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    let out = a.claim("op-a", None, false);
    assert_eq!(
        out,
        ClaimOutcome::Idempotent {
            claimed_by: "op-a".into(),
            claim_counter: 1,
        }
    );
    // Counter did NOT advance on the idempotent re-claim.
    assert_eq!(a.get_state().claim_counter, 1);
}

#[test]
fn case3_force_takeover_wins() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    let out = a.claim("op-b", None, true);
    assert_eq!(
        out,
        ClaimOutcome::Forced {
            claimed_by: "op-b".into(),
            claim_counter: 2,
            previous_pic: Some("op-a".into()),
        }
    );
    assert!(out.claimed());
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-b"));
}

#[test]
fn case4_confirm_token_match_transfers() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    let token = a.create_confirm_token("op-b");
    assert_eq!(token.len(), 32);
    let out = a.claim("op-b", Some(&token), false);
    assert_eq!(
        out,
        ClaimOutcome::Transferred {
            claimed_by: "op-b".into(),
            claim_counter: 2,
            transferred_from: Some("op-a".into()),
        }
    );
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-b"));
    // Token is single-use: a second attempt with the same token fails.
    a.claim("op-a", None, true); // op-a takes it back via force
    let out2 = a.claim("op-b", Some(&token), false);
    assert!(matches!(out2, ClaimOutcome::InvalidConfirmToken { .. }));
}

#[test]
fn case5_already_claimed_no_token_409() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    let out = a.claim("op-b", None, false);
    assert_eq!(
        out,
        ClaimOutcome::AlreadyClaimed {
            current_pic: Some("op-a".into()),
            status: 409,
        }
    );
    assert!(!out.claimed());
    assert!(out.needs_confirm());
    assert_eq!(out.http_status(), 409);
    // op-a still holds it.
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
}

#[test]
fn confirm_token_expires_after_ttl() {
    let (mut a, c) = arbiter();
    a.claim("op-a", None, false);
    let token = a.create_confirm_token("op-b");
    // Past the TTL window the token no longer transfers.
    c.advance(CONFIRM_TTL_SECONDS + 0.01);
    let out = a.claim("op-b", Some(&token), false);
    assert_eq!(
        out,
        ClaimOutcome::InvalidConfirmToken {
            current_pic: Some("op-a".into()),
            status: 409,
        }
    );
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
}

#[test]
fn confirm_token_valid_at_edge_of_ttl() {
    let (mut a, c) = arbiter();
    a.claim("op-a", None, false);
    let token = a.create_confirm_token("op-b");
    // Just inside the window: expires_at >= now still holds.
    c.advance(CONFIRM_TTL_SECONDS - 0.01);
    let out = a.claim("op-b", Some(&token), false);
    assert!(matches!(out, ClaimOutcome::Transferred { .. }));
}

#[test]
fn release_by_holder_succeeds() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    let out = a.release("op-a");
    assert_eq!(
        out,
        ReleaseOutcome::Released {
            previous_pic: Some("op-a".into()),
        }
    );
    assert_eq!(a.get_state().state, PicState::Unclaimed);
}

#[test]
fn release_by_non_holder_403() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    let out = a.release("op-b");
    assert_eq!(
        out,
        ReleaseOutcome::NotCurrentPic {
            current_pic: Some("op-a".into()),
            status: 403,
        }
    );
    // op-a still holds it.
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
}

#[test]
fn heartbeat_by_holder_ok_by_other_410() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    match a.heartbeat("op-a") {
        HeartbeatOutcome::Ok {
            claimed_by,
            claim_counter,
            ..
        } => {
            assert_eq!(claimed_by.as_deref(), Some("op-a"));
            assert_eq!(claim_counter, 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let out = a.heartbeat("op-b");
    assert_eq!(
        out,
        HeartbeatOutcome::NoActiveClaim {
            current_pic: Some("op-a".into()),
            status: 410,
        }
    );
}

#[test]
fn heartbeat_when_unclaimed_410() {
    let (mut a, _c) = arbiter();
    let out = a.heartbeat("op-a");
    assert_eq!(
        out,
        HeartbeatOutcome::NoActiveClaim {
            current_pic: None,
            status: 410,
        }
    );
}

#[test]
fn watchdog_auto_releases_after_timeout() {
    let (mut a, c) = arbiter();
    a.claim("op-a", None, false);
    // Within timeout: no release.
    c.advance(HEARTBEAT_TIMEOUT_SECONDS - 1.0);
    assert_eq!(a.watchdog_tick(), None);
    assert_eq!(a.get_state().state, PicState::Claimed);
    // Past timeout: auto-release.
    c.advance(2.0);
    assert_eq!(a.watchdog_tick().as_deref(), Some("op-a"));
    assert_eq!(a.get_state().state, PicState::Unclaimed);
    // A second tick has nothing to release.
    assert_eq!(a.watchdog_tick(), None);
}

#[test]
fn heartbeat_keeps_claim_alive_through_watchdog() {
    let (mut a, c) = arbiter();
    a.claim("op-a", None, false);
    c.advance(HEARTBEAT_TIMEOUT_SECONDS - 1.0);
    a.heartbeat("op-a"); // refreshes last_heartbeat_ts to "now"
    c.advance(HEARTBEAT_TIMEOUT_SECONDS - 1.0);
    // Total elapsed since claim is > timeout, but the heartbeat reset the
    // clock, so the holder survives.
    assert_eq!(a.watchdog_tick(), None);
    assert_eq!(a.get_state().state, PicState::Claimed);
}

#[test]
fn gamepad_connect_auto_claims_when_unclaimed() {
    let (mut a, _c) = arbiter();
    a.on_gamepad_connected("evdev-0", "hdmi-kiosk");
    assert_eq!(a.get_state().state, PicState::Claimed);
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("hdmi-kiosk"));
    assert_eq!(a.primary_gamepad_id(), Some("evdev-0"));
}

#[test]
fn gamepad_connect_is_noop_when_claimed() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    a.on_gamepad_connected("evdev-0", "hdmi-kiosk");
    // op-a stays PIC; the gamepad did not steal it.
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
    assert_eq!(a.get_state().claim_counter, 1);
}

#[test]
fn pic_disconnect_clears_state() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    assert_eq!(
        a.on_pic_disconnected("op-a"),
        DisconnectOutcome::Disconnected
    );
    assert_eq!(a.get_state().state, PicState::Unclaimed);
    assert!(a.get_state().claimed_by.is_none());
    // Idempotent when already unclaimed.
    assert_eq!(
        a.on_pic_disconnected("op-a"),
        DisconnectOutcome::Disconnected
    );
    assert_eq!(a.get_state().state, PicState::Unclaimed);
}

#[test]
fn disconnect_requires_holder() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false);
    assert_eq!(
        a.on_pic_disconnected("op-b"),
        DisconnectOutcome::NotHolder {
            current_pic: Some("op-a".into()),
            status: 409,
        }
    );
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
}

#[test]
fn force_claim_then_kiosk_disconnect_keeps_holder() {
    let (mut a, _c) = arbiter();
    a.on_gamepad_connected("evdev-0", "hdmi-kiosk");
    assert_eq!(a.primary_gamepad_id(), Some("evdev-0"));
    a.claim("op-b", None, true);
    // The takeover unbinds the kiosk gamepad.
    assert_eq!(a.primary_gamepad_id(), None);
    assert!(matches!(
        a.on_pic_disconnected("hdmi-kiosk"),
        DisconnectOutcome::NotHolder { .. }
    ));
    assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-b"));
}

#[test]
fn release_and_disconnect_unbind_the_primary_gamepad() {
    let (mut a, _c) = arbiter();
    a.on_gamepad_connected("evdev-0", "hdmi-kiosk");
    a.release("hdmi-kiosk");
    assert_eq!(a.primary_gamepad_id(), None);
    a.on_gamepad_connected("evdev-1", "hdmi-kiosk");
    a.on_pic_disconnected("hdmi-kiosk");
    assert_eq!(a.primary_gamepad_id(), None);
}

#[test]
fn claim_counter_is_monotonic_across_transitions() {
    let (mut a, _c) = arbiter();
    a.claim("op-a", None, false); // 1
    a.claim("op-b", None, true); // 2 (force)
    a.release("op-b"); // counter unchanged
    a.claim("op-c", None, false); // 3
    assert_eq!(a.get_state().claim_counter, 3);
}

#[tokio::test]
async fn claim_emits_a_bus_event() {
    let (mut a, _c) = arbiter();
    let mut rx = a.bus().subscribe();
    a.claim("op-a", None, false);
    let ev = rx.recv().await.unwrap();
    assert_eq!(ev.kind, PicEventKind::Claimed);
    assert_eq!(ev.client_id.as_deref(), Some("op-a"));
    assert_eq!(ev.claim_counter, 1);
}

#[test]
fn confirm_tokens_are_unique() {
    let (mut a, _c) = arbiter();
    let t1 = a.create_confirm_token("op-a");
    let t2 = a.create_confirm_token("op-b");
    assert_ne!(t1, t2);
    assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
}
