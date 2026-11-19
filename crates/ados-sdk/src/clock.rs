//! ADOS time for a plugin.
//!
//! [`clock()`] is the plugin's one clock for control loops, timers,
//! freshness checks and stamps: wall time on a real node, the simulator's
//! time when the host runs the plugin on a node following it (the host names
//! the agent's relay in `ADOS_CLOCK_FEED`). On sim time a stale feed stops
//! every timer; nothing falls back to wall time. Use [`steady`] for what must
//! stay on real time (reconnect backoff, latency measurement).

use std::sync::LazyLock;

pub use ados_protocol::simclock::{
    steady, Clock, Interval, SimInstant, WallInstant, CLOCK_FEED_ENV,
};

static CLOCK: LazyLock<Clock> = LazyLock::new(Clock::from_env);

/// The process's ADOS time, made on first use from `ADOS_CLOCK_FEED`. On a
/// node following the simulator's clock the first call must run inside a
/// Tokio runtime (it starts the relay follower).
pub fn clock() -> Clock {
    CLOCK.clone()
}
