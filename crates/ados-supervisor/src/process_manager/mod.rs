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

pub use launchd::LaunchdManager;
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

    /// Restart the unit (a fresh spawn cycle). True on success. Starts a unit
    /// that is not running, so it is for units the caller owns.
    async fn restart(&self, unit: &str) -> bool;

    /// Restart the unit only if it is currently running; a stopped unit stays
    /// stopped. For nudging a unit whose start is some other caller's decision.
    async fn try_restart(&self, unit: &str) -> bool;

    /// Clear a failed / start-limit-hit state so a following `start` is not a
    /// no-op on a unit that crash-looped past the start-limit burst. Best-effort,
    /// no return value.
    async fn reset_failed(&self, unit: &str);

    /// Whether the unit is currently active: `Some(true)` running,
    /// `Some(false)` definitely not running, `None` when the manager could not
    /// answer (a timed-out or unspawnable probe).
    ///
    /// `None` is no verdict. A caller must never read it as a death: under
    /// memory pressure or a busy service manager every probe can fail at once,
    /// and treating that as "inactive" restarts every healthy unit on the node.
    async fn is_active(&self, unit: &str) -> Option<bool>;

    /// Cumulative bytes the unit's main process has read plus written since it
    /// started, or `None` when this backend cannot resolve it.
    ///
    /// The delta of this value across monitor passes is the supervisor's only
    /// proof that an `active` unit is doing anything (see
    /// [`crate::work_proof`]). `None` is the honest answer wherever the signal
    /// does not exist — launchd and the inert backend have no `/proc/<pid>/io`
    /// analogue — and the caller renders that as "no verdict", never as a
    /// stall. Defaulted so a backend that cannot answer says so by omission.
    async fn work_counter(&self, _unit: &str) -> Option<u64> {
        None
    }

    /// Mask the unit so a stray `start` cannot bring it up (idempotent).
    async fn mask(&self, unit: &str);

    /// Unmask the unit (idempotent).
    async fn unmask(&self, unit: &str);
}
