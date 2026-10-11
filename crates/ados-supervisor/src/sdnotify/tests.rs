use super::*;
use std::sync::atomic::AtomicUsize;

#[test]
fn watchdog_interval_is_a_third_of_the_deadline() {
    // The shipped unit deadline → a 10 s keep-alive cadence (a third of 30 s),
    // which tolerates two missed pings before systemd's SIGKILL deadline.
    assert_eq!(watchdog_interval(30), Duration::from_secs(10));
    assert_eq!(watchdog_interval(WATCHDOG_SEC), Duration::from_secs(10));
    assert_eq!(watchdog_interval(90), Duration::from_secs(30));
}

#[test]
fn watchdog_interval_never_collapses_to_zero() {
    // A tiny or zero deadline must still yield a positive, schedulable cadence
    // so `tokio::time::interval` never panics on a zero period.
    assert_eq!(watchdog_interval(2), Duration::from_secs(1));
    assert_eq!(watchdog_interval(1), Duration::from_secs(1));
    assert_eq!(watchdog_interval(0), Duration::from_secs(1));
    assert!(watchdog_interval(0) > Duration::ZERO);
}

#[test]
fn the_stall_budget_leaves_room_for_a_slow_but_advancing_pass() {
    // Three 5 s intervals + the 60 s grace: a pass that stamps progress at
    // every stage boundary can spend over a minute in one stage without the
    // watchdog being withheld, while a pass that stops stamping trips it.
    assert_eq!(
        stall_budget(Duration::from_secs(5)),
        Duration::from_secs(75)
    );
    assert!(stall_budget(Duration::from_secs(5)) > Duration::from_secs(WATCHDOG_SEC));
}

#[tokio::test(start_paused = true)]
async fn the_watchdog_is_withheld_while_the_monitor_pass_is_stalled() {
    // The SUP-WATCHDOG-DECOUPLED regression. A pass that stops making
    // progress must stop feeding the watchdog so systemd restarts the unit,
    // instead of the unit reporting healthy forever with service
    // reconciliation dead.
    let pings = Arc::new(AtomicUsize::new(0));
    let progress = MonitorProgress::new();
    let interval = Duration::from_secs(10);
    let budget = stall_budget(Duration::from_secs(5)); // 75 s

    let counter = pings.clone();
    let loop_progress = progress.clone();
    tokio::spawn(async move {
        watchdog_loop(interval, budget, loop_progress, move || {
            counter.fetch_add(1, Ordering::Relaxed);
        })
        .await
    });

    // A healthy pass stamping at every tick keeps the watchdog fed. Each
    // step yields so the ticker task actually gets polled for the tick the
    // advance made ready.
    for _ in 0..6 {
        progress.mark();
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
    }
    let fed_while_healthy = pings.load(Ordering::Relaxed);
    assert!(
        fed_while_healthy >= 3,
        "a progressing pass must feed the watchdog, got {fed_while_healthy}"
    );

    // Now the pass wedges: no more stamps. Inside the budget the ticker
    // still feeds (a slow stage is not a wedge).
    for _ in 0..5 {
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
    }
    let fed_inside_budget = pings.load(Ordering::Relaxed);
    assert!(
        fed_inside_budget > fed_while_healthy,
        "a pass inside the stall budget must still be fed"
    );

    // Past the budget the pings must stop dead.
    for _ in 0..4 {
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
    }
    let at_stall = pings.load(Ordering::Relaxed);
    for _ in 0..30 {
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(
        pings.load(Ordering::Relaxed),
        at_stall,
        "a stalled monitor pass must NOT be reported to systemd as healthy"
    );

    // And a pass that recovers resumes feeding, so a transient wedge that
    // clears inside WatchdogSec does not need an operator.
    progress.mark();
    for _ in 0..2 {
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
    }
    assert!(
        pings.load(Ordering::Relaxed) > at_stall,
        "a recovered pass must resume feeding the watchdog"
    );
}

#[tokio::test(start_paused = true)]
async fn progress_since_mark_tracks_the_last_stamp() {
    let progress = MonitorProgress::new();
    tokio::time::advance(Duration::from_secs(7)).await;
    assert!(progress.since_mark() >= Duration::from_secs(7));
    progress.mark();
    assert!(progress.since_mark() < Duration::from_secs(1));
}
