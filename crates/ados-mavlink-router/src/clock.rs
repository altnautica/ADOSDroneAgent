//! The router's ADOS time.
//!
//! The control lanes run on ADOS time: the swarm setpoint loop, the FC
//! request ticks (stream cadence, banner, landed state, parameter sweep), the
//! inbound-activity clock those ticks read, the mission mirror's deadlines and
//! every stamp on the shared vehicle state. ADOS time is wall time on a real
//! node and the simulator's time on a node with `simulation.enabled` and
//! `simulation.clock: sim`; the router follows the simulator's feed on its own
//! port, `simulation.clock_feed` + 3 ([`ados_time::FeedPeer::Router`], default
//! `127.0.0.1:14653`). Reconnect backoffs, probes, aux shaping, the demo
//! source, the sidecars and FC heartbeat liveness stay on steady wall time.
//!
//! The swarm controller and the vehicle state take `Instant`s, so ADOS time is
//! handed out as one: the real `Instant::now()` on wall, a fixed anchor plus
//! the sim time on sim. Instants from [`RouterClock::now`] are only compared
//! with each other; the files that hold them are on the clock-seam list
//! (`scripts/clock-seam-allow.txt`), where a wall `Instant::now()` fails the
//! guard.

use std::time::{Duration, Instant};

use ados_config::simulation::SimulationGate;
use ados_time::{Clock, FeedPeer, Interval};

/// ADOS time for the router, as `Instant`s.
#[derive(Clone)]
pub struct RouterClock {
    clock: Clock,
    /// The `Instant` sim time 0 maps to.
    anchor: Instant,
}

impl RouterClock {
    /// Wall time.
    pub fn wall() -> Self {
        Self::new(Clock::wall())
    }

    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            anchor: Instant::now(),
        }
    }

    /// The router's clock from the node config: the simulator's feed on
    /// [`FeedPeer::Router`] on a node configured for `simulation.clock: sim`
    /// (binds the port; needs a Tokio runtime), wall otherwise.
    pub fn from_config(gate: &SimulationGate) -> std::io::Result<Self> {
        ados_time::from_config(gate, FeedPeer::Router).map(Self::new)
    }

    /// `wall` or `sim`.
    pub fn source(&self) -> &'static str {
        self.clock.source()
    }

    /// ADOS time now. On sim it stands still while the simulator is paused or
    /// its feed is stale, and reads the anchor before the first tick.
    pub fn now(&self) -> Instant {
        match &self.clock {
            Clock::Wall(_) => Instant::now(),
            Clock::Sim(_) => self.anchor + Duration::from_nanos(self.clock.now_ns()),
        }
    }

    /// ADOS time elapsed since `at`, a stamp taken from [`Self::now`].
    pub fn since(&self, at: Instant) -> Duration {
        self.now().saturating_duration_since(at)
    }

    /// ADOS time in nanoseconds since the clock began (wall) or the run
    /// began (sim), for a stamp kept in an atomic.
    pub fn now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    /// A periodic timer on ADOS time: the first tick is immediate, missed
    /// ticks are skipped, and on sim it stops while the feed is stale.
    pub fn interval(&self, period: Duration) -> Interval {
        self.clock.interval(period)
    }
}

impl Default for RouterClock {
    fn default() -> Self {
        Self::wall()
    }
}

impl std::fmt::Debug for RouterClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterClock")
            .field("source", &self.source())
            .finish()
    }
}

#[cfg(test)]
pub(crate) mod tests;
