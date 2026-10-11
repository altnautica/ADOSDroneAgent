use super::*;

#[tokio::test]
async fn due_when_never_ticked_then_throttled() {
    let r = RegReconciler::new(EventEmitter::with_socket(
        "ados-test",
        "/nonexistent/ados/logd.sock",
    ));
    let now = Instant::now();
    // Never ticked → due.
    assert!(r.due(Duration::from_secs(30), now));
    // Simulate a recent tick by constructing one with last_tick set.
    let mut r2 = RegReconciler::new(EventEmitter::with_socket(
        "ados-test",
        "/nonexistent/ados/logd.sock",
    ));
    r2.last_tick = Some(now);
    // Not yet due inside the interval.
    assert!(!r2.due(Duration::from_secs(30), now + Duration::from_secs(10)));
    // Due once the interval elapses.
    assert!(r2.due(Duration::from_secs(30), now + Duration::from_secs(31)));
}

#[tokio::test]
async fn tick_is_inert_without_a_real_radio_environment() {
    // A first tick on a freshly-constructed reconciler is "due" (fast window
    // open, never ticked) but must complete without panic on any host: off
    // Linux it is a no-op; on Linux CI `iw` is read-only and the wanted
    // domain resolves to the safe default. This exercises the started_at /
    // effective_interval wiring end to end.
    let mut r = RegReconciler::new(EventEmitter::with_socket(
        "ados-test",
        "/nonexistent/ados/logd.sock",
    ));
    r.tick().await;
}
