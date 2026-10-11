//! Process signals and the latched shutdown watch the run loop selects on.

use tokio::sync::watch;

/// Wait until the latched shutdown watch flips to `true`. Returns immediately if
/// it is already set (the latch never loses an edge) and on a closed channel
/// (the sender dropped — treat as shutdown). The single owner of the `&mut`
/// receiver for the run-loop's own shutdown checks; worker tasks get their wake
/// via the per-bring-up `Shutdown` bridge.
pub(super) async fn wait_for_shutdown_flag(shutdown: &mut watch::Receiver<bool>) {
    // `wait_for` resolves as soon as the predicate holds — including on the
    // current value — and on sender-drop it returns `Err`, which we also treat as
    // a shutdown signal so a vanished sender never strands the loop.
    let _ = shutdown.wait_for(|s| *s).await;
}

/// Resolve when SIGTERM or SIGINT is received.
pub(super) async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
