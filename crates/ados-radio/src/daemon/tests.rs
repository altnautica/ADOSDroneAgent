use super::*;

/// A latched shutdown watch that is already `true` resolves
/// `wait_for_shutdown_flag` immediately — the latch never loses the edge, so
/// a SIGTERM that flipped the watch while a select arm was busy is still seen
/// on the next poll.
#[tokio::test]
async fn shutdown_flag_already_set_resolves_immediately() {
    let (tx, rx) = watch::channel(false);
    tx.send(true).unwrap();
    let mut rx = rx;
    // No timeout needed: if the latch were lost this would hang and the test
    // harness would catch it, but on a correct latch it returns at once.
    wait_for_shutdown_flag(&mut rx).await;
}

/// With `biased;` and the shutdown arm FIRST, a latched shutdown wins the
/// poll even when a competing arm (a finished "watchdog" task) is also ready
/// in the same instant — so a real shutdown is never mistaken for a respawn
/// trigger.
#[tokio::test]
async fn shutdown_arm_wins_over_a_ready_competitor() {
    let (tx, rx) = watch::channel(false);
    tx.send(true).unwrap();
    let mut rx = rx;
    // A competing future that is immediately ready (stands in for a watchdog
    // task that finished and would otherwise route into the respawn path).
    let competitor = std::future::ready(());

    #[derive(Debug, PartialEq)]
    enum Won {
        Shutdown,
        Competitor,
    }
    let won = tokio::select! {
        biased;
        _ = wait_for_shutdown_flag(&mut rx) => Won::Shutdown,
        _ = competitor => Won::Competitor,
    };
    assert_eq!(
        won,
        Won::Shutdown,
        "the biased shutdown-first arm must win over a ready competitor"
    );
}

/// A dropped sender (no more shutdown signal possible) also resolves the
/// wait, so a vanished signaller never strands the run loop forever.
#[tokio::test]
async fn shutdown_flag_resolves_on_sender_drop() {
    let (tx, rx) = watch::channel(false);
    let mut rx = rx;
    drop(tx);
    wait_for_shutdown_flag(&mut rx).await;
}
