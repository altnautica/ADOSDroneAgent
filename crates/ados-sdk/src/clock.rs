//! ADOS time for a plugin.
//!
//! [`clock()`] is the plugin's one clock for control loops, timers,
//! freshness checks and stamps: wall time on a real node, the simulator's
//! time when the host runs the plugin on a node following it (the host names
//! the agent's relay in `ADOS_CLOCK_FEED`). On sim time a stale feed stops
//! every timer; nothing falls back to wall time. Use [`steady`] for what must
//! stay on real time (reconnect backoff, latency measurement).
//!
//! In lockstep (`simulation.clock_mode: lockstep`) the simulator steps past a
//! grant only when the agent is done with it. A plugin whose loop must keep
//! up opts in with [`done`]: its first call registers the plugin with the
//! agent's barrier (the participants socket beside the relay), and from then
//! on the simulator waits for it. A periodic loop reports its next due time
//! after each tick ([`done_until`]).

use std::sync::LazyLock;

pub use ados_protocol::simclock::{
    steady, Clock, Interval, Participant, SimInstant, WallInstant, CLOCK_FEED_ENV,
};

static CLOCK: LazyLock<Clock> = LazyLock::new(Clock::from_env);

/// The barrier registration, made on the first [`done`]: `None` off sim time.
static PARTICIPANT: LazyLock<Option<Participant>> =
    LazyLock::new(|| match std::env::var_os(CLOCK_FEED_ENV) {
        Some(p) if !p.is_empty() => Some(Participant::connect(
            ados_protocol::simclock::participants_socket_beside(std::path::Path::new(&p)),
        )),
        _ => None,
    });

/// The process's ADOS time, made on first use from `ADOS_CLOCK_FEED`. On a
/// node following the simulator's clock the first call must run inside a
/// Tokio runtime (it starts the relay follower).
pub fn clock() -> Clock {
    CLOCK.clone()
}

/// Every piece of this plugin's work due before sim time `t_ns` is done (its
/// commands sent). The first call registers the plugin with the lockstep
/// barrier, so the simulator waits for it from then on; it must keep
/// reporting as sim time moves. A no-op on wall time. On sim time the first
/// call must run inside a Tokio runtime.
pub fn done(t_ns: u64) {
    if let Some(p) = PARTICIPANT.as_ref() {
        p.done(t_ns);
    }
}

/// [`done`] until `iv`'s next tick: what a periodic loop reports after each
/// tick of an [`Interval`] from [`clock()`].
pub fn done_until(iv: &Interval) {
    if let Some(n) = iv.next_due_ns() {
        done(n);
    }
}
