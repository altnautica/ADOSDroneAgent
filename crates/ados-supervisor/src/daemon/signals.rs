//! Stop-signal handling for the daemon loop.

/// Resolve on the first SIGTERM or SIGINT, naming which.
pub(super) async fn stop_signal(
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) -> &'static str {
    tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sigint.recv() => "SIGINT",
    }
}
