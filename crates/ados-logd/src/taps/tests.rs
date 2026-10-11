use super::*;
use std::time::Duration;

#[tokio::test]
async fn spawn_all_taps_starts_three_tasks_that_stop_on_one_signal() {
    // Point the taps at a tempdir with no sockets and no sidecars: every tap
    // must come up, sit in its absent-seam backoff, and then stop promptly
    // when the single shutdown signal fires.
    let dir = tempfile::tempdir().unwrap();
    let paths = TapPaths {
        state_socket: dir.path().join("state.sock"),
        mavlink_socket: dir.path().join("mavlink.sock"),
        sidecar_root: dir.path().to_path_buf(),
    };
    let (ingest_tx, _ingest_rx) = mpsc::channel::<IngestFrame>(16);
    let (shutdown_tx, _keep) = broadcast::channel::<()>(1);

    let handles = spawn_all_taps(&paths, ingest_tx, &shutdown_tx, mavlink::DEFAULT_SAMPLE_HZ);
    assert_eq!(handles.len(), 3);

    // Let them attempt their connects / first poll.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // One fire stops all three within the bound.
    let _ = shutdown_tx.send(());
    for h in handles {
        tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("each tap stops within the bound")
            .expect("no tap task panicked");
    }
}

#[tokio::test]
async fn shutdown_never_parks_until_dropped_receiver() {
    // `never()` must not resolve on its own within a short window.
    let mut s = Shutdown::never();
    let r = tokio::time::timeout(Duration::from_millis(100), s.recv()).await;
    assert!(r.is_err(), "never() must not fire on its own");
}

#[tokio::test]
async fn shutdown_pair_fires_the_subscriber() {
    let (stop, mut s) = Shutdown::pair();
    stop.fire();
    tokio::time::timeout(Duration::from_millis(200), s.recv())
        .await
        .expect("fire wakes the subscriber");
}
