use super::*;
use std::time::Duration;

// ----- the threshold + cooldown state machine -----

fn healer() -> WifiSelfHeal {
    // The emitter points at an absent socket; emits are wait-free no-ops in
    // tests (the state machine under test never inspects shipped events).
    let events = EventEmitter::with_socket("ados-test", "/nonexistent/ados/logd.sock");
    WifiSelfHeal::new(events)
}

#[tokio::test]
async fn single_failure_does_not_heal() {
    let mut h = healer();
    let now = Instant::now();
    let d = h.step("home", false, 2, Duration::from_secs(60), now);
    assert_eq!(
        d,
        HealDecision::Wait {
            consecutive_failures: 1
        }
    );
}

#[tokio::test]
async fn threshold_reached_heals() {
    let mut h = healer();
    let now = Instant::now();
    assert_eq!(
        h.step("home", false, 2, Duration::from_secs(60), now),
        HealDecision::Wait {
            consecutive_failures: 1
        }
    );
    assert_eq!(
        h.step("home", false, 2, Duration::from_secs(60), now),
        HealDecision::Heal {
            consecutive_failures: 2
        }
    );
}

#[tokio::test]
async fn healthy_tick_clears_the_count() {
    let mut h = healer();
    let now = Instant::now();
    h.step("home", false, 2, Duration::from_secs(60), now);
    assert_eq!(
        h.step("home", true, 2, Duration::from_secs(60), now),
        HealDecision::Healthy
    );
    // A fresh failure starts counting from 1 again.
    assert_eq!(
        h.step("home", false, 2, Duration::from_secs(60), now),
        HealDecision::Wait {
            consecutive_failures: 1
        }
    );
}

#[tokio::test]
async fn cooldown_blocks_a_second_heal_then_lifts() {
    let mut h = healer();
    let t0 = Instant::now();
    let cooldown = Duration::from_secs(60);
    // Cross the threshold and heal at t0. The heal resets the running count.
    h.step("home", false, 2, cooldown, t0);
    assert_eq!(
        h.step("home", false, 2, cooldown, t0),
        HealDecision::Heal {
            consecutive_failures: 2
        }
    );
    // Still failing inside the cooldown window: the threshold is re-met on the
    // second failure, but the recent heal owns the cooldown, so it must NOT
    // re-heal. The running count keeps climbing (it is not reset until a heal
    // actually fires) so the moment the cooldown lifts a heal can take.
    let t_in = t0 + Duration::from_secs(10);
    assert_eq!(
        h.step("home", false, 2, cooldown, t_in),
        HealDecision::Wait {
            consecutive_failures: 1
        }
    );
    assert_eq!(
        h.step("home", false, 2, cooldown, t_in),
        HealDecision::Wait {
            consecutive_failures: 2
        }
    );
    // After the cooldown lifts, the next sustained failure heals again. The
    // count carried into this window is 2, so this failing tick (count → 3)
    // crosses the threshold with the cooldown now expired.
    let t_after = t0 + Duration::from_secs(61);
    assert_eq!(
        h.step("home", false, 2, cooldown, t_after),
        HealDecision::Heal {
            consecutive_failures: 3
        }
    );
}

#[tokio::test]
async fn per_connection_state_is_independent() {
    let mut h = healer();
    let now = Instant::now();
    // home fails twice → heals; office stays healthy.
    h.step("home", false, 2, Duration::from_secs(60), now);
    assert_eq!(
        h.step("home", false, 2, Duration::from_secs(60), now),
        HealDecision::Heal {
            consecutive_failures: 2
        }
    );
    assert_eq!(
        h.step("office", true, 2, Duration::from_secs(60), now),
        HealDecision::Healthy
    );
    // office's first failure is still just a Wait at 1.
    assert_eq!(
        h.step("office", false, 2, Duration::from_secs(60), now),
        HealDecision::Wait {
            consecutive_failures: 1
        }
    );
}

#[tokio::test]
async fn prune_drops_stale_connection_state() {
    let mut h = healer();
    let now = Instant::now();
    h.step("home", false, 2, Duration::from_secs(60), now);
    h.step("office", false, 2, Duration::from_secs(60), now);
    assert_eq!(h.states.len(), 2);
    // Only `home` is still a candidate; `office` state must be pruned.
    h.prune(&[WifiConnection {
        name: "home".to_string(),
        iface: "wlan0".to_string(),
    }]);
    assert_eq!(h.states.len(), 1);
    assert!(h.states.contains_key("home"));
}
