use super::*;

#[test]
fn trigger_arms_after_hold_and_resets_on_release() {
    let hold = Duration::from_secs(30);
    let mut t = RehomeTrigger::with_hold(hold);
    let t0 = Instant::now();
    // Holding but inside the window: not armed.
    assert!(!t.observe(true, t0));
    assert!(!t.observe(true, t0 + Duration::from_secs(15)));
    // Past the window: armed (a level), stays armed while the fault holds.
    assert!(t.observe(true, t0 + Duration::from_secs(30)));
    assert!(t.observe(true, t0 + Duration::from_secs(40)));
    // Condition releases → disarms; a fresh onset must re-run the window.
    assert!(!t.observe(false, t0 + Duration::from_secs(45)));
    assert!(!t.observe(true, t0 + Duration::from_secs(46)));
    assert!(t.observe(true, t0 + Duration::from_secs(76)));
}

#[test]
fn trigger_does_not_arm_on_a_transient() {
    let hold = Duration::from_secs(30);
    let mut t = RehomeTrigger::with_hold(hold);
    let t0 = Instant::now();
    assert!(!t.observe(true, t0));
    // Released before the window: never armed.
    assert!(!t.observe(false, t0 + Duration::from_secs(10)));
    assert!(!t.observe(true, t0 + Duration::from_secs(11)));
}

const COOLDOWN: Duration = Duration::from_secs(10);
const HEALTHY_RESET: Duration = Duration::from_secs(120);

/// One step with the fixed cooldown and reset window every test uses.
fn step_at(m: &mut RehomeMachine, armed: bool, healthy: bool, at: Instant) -> RehomeStep {
    m.step(armed, healthy, COOLDOWN, HEALTHY_RESET, at)
}

#[test]
fn fires_one_attempt_then_cools_down() {
    let mut m = RehomeMachine::new();
    let t0 = Instant::now();
    // Armed + faulty + no prior attempt → fire attempt 1 immediately.
    assert_eq!(
        step_at(&mut m, true, false, t0),
        RehomeStep::Attempt { index: 1 }
    );
    // Inside the cooldown: wait.
    match step_at(&mut m, true, false, t0 + Duration::from_secs(3)) {
        RehomeStep::Cooldown { .. } => {}
        other => panic!("expected cooldown, got {other:?}"),
    }
    // After the cooldown: attempt 2.
    assert_eq!(
        step_at(&mut m, true, false, t0 + Duration::from_secs(11)),
        RehomeStep::Attempt { index: 2 }
    );
}

#[test]
fn a_persistent_fault_never_stops_being_retried() {
    // This replaces a test called `budget_caps_to_exhausted_once`, whose
    // comment read "it stays exhausted (no re-loop)". That was the defect,
    // pinned: the budget latched a state whose only exit was a sustained
    // healthy window, the adapter could only become healthy after a
    // successful rehome, and no rehome was attempted while latched. A
    // wedged radio was abandoned after ~90 s until someone rebooted.
    let mut m = RehomeMachine::new();
    let t0 = Instant::now();

    // Walk far past any budget the old code had (3) and keep walking.
    let mut attempts = Vec::new();
    for tick in 0..200u64 {
        let at = t0 + Duration::from_secs(tick * 11);
        if let RehomeStep::Attempt { index } = step_at(&mut m, true, false, at) {
            attempts.push(index);
        }
    }

    assert_eq!(
        attempts.len(),
        200,
        "recovery stopped attempting after {} tries",
        attempts.len()
    );
    // The count keeps climbing so an operator can see how long this has
    // been going on; it is diagnostic, never a ceiling.
    assert_eq!(attempts.first(), Some(&1));
    assert_eq!(attempts.last(), Some(&200));
}

#[test]
fn the_wait_between_attempts_never_grows() {
    // The other half of the old design: an escalating [10, 30, 60] s
    // schedule. On a board recovering its own peripheral there is no herd
    // to spread, so growth only delays recovery.
    let mut m = RehomeMachine::new();
    let t0 = Instant::now();
    for tick in 0..20u64 {
        let fired = t0 + Duration::from_secs(tick * 11);
        assert!(
            matches!(
                step_at(&mut m, true, false, fired),
                RehomeStep::Attempt { .. }
            ),
            "attempt {tick} did not fire 11s after the previous one, so the \
             cooldown grew past the fixed {COOLDOWN:?}"
        );
    }
}

#[test]
fn not_armed_is_idle() {
    let mut m = RehomeMachine::new();
    let t0 = Instant::now();
    assert_eq!(step_at(&mut m, false, false, t0), RehomeStep::Idle);
}

#[test]
fn sustained_health_recovers_and_closes_the_episode() {
    let mut m = RehomeMachine::new();
    let t0 = Instant::now();
    // Spend an attempt.
    step_at(&mut m, true, false, t0);
    // Healthy but not yet for the reset window → Idle, count not reset.
    assert_eq!(
        step_at(&mut m, false, true, t0 + Duration::from_secs(60)),
        RehomeStep::Idle
    );
    // Healthy past the reset window → Recovered + episode closed.
    assert_eq!(
        step_at(&mut m, false, true, t0 + Duration::from_secs(181)),
        RehomeStep::Recovered
    );
    assert_eq!(m.attempts(), 0);
    // A fresh fault attempts again from index 1.
    assert_eq!(
        step_at(&mut m, true, false, t0 + Duration::from_secs(200)),
        RehomeStep::Attempt { index: 1 }
    );
}

#[test]
fn a_long_running_episode_still_recovers_when_the_adapter_comes_back() {
    // The property the latch destroyed. After a hundred failed attempts,
    // sustained health must still close the episode — under the old design
    // this path was unreachable, because reaching health required the
    // attempts the latch had stopped.
    let mut m = RehomeMachine::new();
    let t0 = Instant::now();
    for tick in 0..100u64 {
        step_at(&mut m, true, false, t0 + Duration::from_secs(tick * 11));
    }
    assert_eq!(m.attempts(), 100);
    let healed = t0 + Duration::from_secs(100 * 11);
    step_at(&mut m, false, true, healed);
    assert_eq!(
        step_at(&mut m, false, true, healed + HEALTHY_RESET),
        RehomeStep::Recovered
    );
    assert_eq!(m.attempts(), 0);
}

#[test]
fn a_brief_healthy_blip_does_not_close_the_episode() {
    let mut m = RehomeMachine::new();
    let t0 = Instant::now();
    step_at(&mut m, true, false, t0);
    // Brief health (under the reset window), then faulty again: the healthy
    // timer resets so the next sustained-health window starts fresh, and
    // the attempt count is preserved (not reset by the blip) — otherwise a
    // flapping adapter would hide how long it has been failing.
    step_at(&mut m, false, true, t0 + Duration::from_secs(20));
    assert_eq!(m.attempts(), 1);
    assert_eq!(
        step_at(&mut m, true, false, t0 + Duration::from_secs(40)),
        RehomeStep::Attempt { index: 2 }
    );
}

#[test]
fn a_refunded_attempt_is_not_counted_and_clears_the_cooldown() {
    // The guard can refuse an attempt after the machine authorised it. That
    // must not consume anything, and must not leave a phantom cooldown.
    let mut m = RehomeMachine::new();
    let t0 = Instant::now();
    assert_eq!(
        step_at(&mut m, true, false, t0),
        RehomeStep::Attempt { index: 1 }
    );
    m.refund_attempt();
    assert_eq!(m.attempts(), 0);
    assert_eq!(
        step_at(&mut m, true, false, t0 + Duration::from_secs(1)),
        RehomeStep::Attempt { index: 1 }
    );
}

#[test]
fn detail_is_bland_and_omits_absent_fields() {
    let d = usb_rehome_detail("rehoming", "wlan1", "1-1", 1, 60, Some(12), None, None);
    assert_eq!(d.get("state").and_then(|v| v.as_str()), Some("rehoming"));
    assert_eq!(d.get("bind_id").and_then(|v| v.as_str()), Some("1-1"));
    assert_eq!(d.get("cooldown_s").and_then(|v| v.as_u64()), Some(60));
    assert_eq!(
        d.get("before_usb_speed_mbps").and_then(|v| v.as_u64()),
        Some(12)
    );
    assert!(!d.contains_key("after_usb_speed_mbps"));
    assert!(!d.contains_key("reason"));
    // The old field named a budget that no longer exists.
    assert!(!d.contains_key("max_attempts"));
    let g = usb_rehome_detail(
        "guard_blocked",
        "wlan1",
        "1-1",
        0,
        60,
        None,
        None,
        Some("shares_device"),
    );
    assert_eq!(
        g.get("reason").and_then(|v| v.as_str()),
        Some("shares_device")
    );
}
