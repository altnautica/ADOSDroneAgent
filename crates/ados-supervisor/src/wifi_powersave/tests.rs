use super::*;

#[tokio::test]
async fn due_when_never_ticked_then_throttled() {
    let r = WifiPowersaveReconciler::new(EventEmitter::with_socket(
        "ados-test",
        "/nonexistent/ados/logd.sock",
    ));
    let now = Instant::now();
    // Never ticked → due.
    assert!(r.due(Duration::from_secs(30), now));
    // A recent tick throttles the next attempt inside the interval.
    let mut r2 = WifiPowersaveReconciler::new(EventEmitter::with_socket(
        "ados-test",
        "/nonexistent/ados/logd.sock",
    ));
    r2.last_tick = Some(now);
    assert!(!r2.due(Duration::from_secs(30), now + Duration::from_secs(10)));
    assert!(r2.due(Duration::from_secs(30), now + Duration::from_secs(31)));
}

#[tokio::test]
async fn tick_is_inert_without_a_real_radio_environment() {
    // A first tick on a freshly-constructed reconciler is "due" (fast window
    // open, never ticked) but must complete without panic on any host: off
    // Linux it is a no-op; on Linux CI `iw` reads fall through safely. This
    // exercises the started_at / effective_interval wiring end to end.
    let mut r = WifiPowersaveReconciler::new(EventEmitter::with_socket(
        "ados-test",
        "/nonexistent/ados/logd.sock",
    ));
    r.tick().await;
}
