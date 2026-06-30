//! Platform selection for the active [`ProcessManager`] backend.

use std::sync::Arc;

use async_trait::async_trait;

use super::{LaunchdManager, ProcessManager, SystemdManager};

/// Select the process-manager backend for the host OS: systemd on Linux,
/// launchd on macOS, and an inert no-op manager anywhere else so the pure-logic
/// core still builds and its tests run. Every backend type compiles on every
/// host; only the runtime selection differs.
pub fn select() -> Arc<dyn ProcessManager> {
    if cfg!(target_os = "linux") {
        Arc::new(SystemdManager)
    } else if cfg!(target_os = "macos") {
        Arc::new(LaunchdManager)
    } else {
        Arc::new(NullManager)
    }
}

/// Inert backend for a host with no supported service manager. Every verb is a
/// no-op returning the soft-failure value, preserving the prior behavior on a
/// platform where the manager binary was simply absent.
pub struct NullManager;

#[async_trait]
impl ProcessManager for NullManager {
    async fn start(&self, _unit: &str) -> bool {
        false
    }

    async fn stop(&self, _unit: &str) -> bool {
        false
    }

    async fn restart(&self, _unit: &str) -> bool {
        false
    }

    async fn reset_failed(&self, _unit: &str) {}

    async fn is_active(&self, _unit: &str) -> bool {
        false
    }

    async fn mask(&self, _unit: &str) {}

    async fn unmask(&self, _unit: &str) {}
}
