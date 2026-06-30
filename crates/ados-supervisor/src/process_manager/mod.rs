//! Cross-platform service process-manager abstraction.
//!
//! The supervisor decides *when* to start/stop/restart each managed unit; the
//! actual lifecycle call is delegated to a platform backend behind the
//! [`ProcessManager`] trait. systemd (`systemctl`) drives the units on Linux and
//! launchd (`launchctl`) on macOS; an inert backend covers any other host so the
//! pure-logic core still builds and runs its tests everywhere. A backend treats
//! a missing manager binary or a timeout as a soft failure (`false` / no-op) so
//! the caller logs and proceeds rather than aborting the supervisor.

mod launchd;
mod select;
mod systemd;

pub use launchd::{render_plist, unit_to_label, LaunchdManager};
pub use select::{select, NullManager};
pub use systemd::SystemdManager;

use async_trait::async_trait;

/// Lifecycle operations over a platform service manager. Each method takes a
/// unit/service name and mirrors the verbs the supervisor issues.
#[async_trait]
pub trait ProcessManager: Send + Sync {
    /// Start the unit. True only when it reached the active state.
    async fn start(&self, unit: &str) -> bool;

    /// Stop the unit. True when the stop verb succeeded.
    async fn stop(&self, unit: &str) -> bool;

    /// Restart the unit (a fresh spawn cycle). True on success.
    async fn restart(&self, unit: &str) -> bool;

    /// Clear a failed / start-limit-hit state so a following `start` is not a
    /// no-op on a unit that crash-looped past the start-limit burst. Best-effort,
    /// no return value.
    async fn reset_failed(&self, unit: &str);

    /// True only when the unit is currently active/running.
    async fn is_active(&self, unit: &str) -> bool;

    /// Mask the unit so a stray `start` cannot bring it up (idempotent).
    async fn mask(&self, unit: &str);

    /// Unmask the unit (idempotent).
    async fn unmask(&self, unit: &str);
}
