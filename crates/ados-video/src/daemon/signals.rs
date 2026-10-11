//! Signal wiring for the `ados-video` daemon.

use ados_protocol::shutdown::Shutdown;

#[cfg(target_os = "linux")]
pub(super) fn spawn_signal_handlers(
    shutdown: Shutdown,
    camera_plugged: std::sync::Arc<tokio::sync::Notify>,
) {
    use tokio::signal::unix::{signal, SignalKind};

    tokio::spawn(async move {
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGINT handler");
                return;
            }
        };
        let mut sigusr1 = match signal(SignalKind::user_defined1()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGUSR1 handler");
                return;
            }
        };
        loop {
            tokio::select! {
                _ = sigterm.recv() => {
                    tracing::info!("received SIGTERM");
                    shutdown.trigger();
                    break;
                }
                _ = sigint.recv() => {
                    tracing::info!("received SIGINT");
                    shutdown.trigger();
                    break;
                }
                _ = sigusr1.recv() => {
                    tracing::debug!("received SIGUSR1: camera hotplug");
                    camera_plugged.notify_one();
                }
            }
        }
    });
}

#[cfg(not(target_os = "linux"))]
pub(super) fn spawn_ctrl_c_handler(shutdown: Shutdown) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("received Ctrl-C");
            shutdown.trigger();
        }
    });
}
