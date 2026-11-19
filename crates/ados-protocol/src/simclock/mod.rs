//! `simclock.v1`: ADOS time, and the simulator's clock feed it can follow.
//!
//! Every agent component and plugin reads one of two clocks, typed apart so
//! they cannot be compared by mistake:
//!
//! - [`Clock`], "ADOS time": wall time on a real node, the simulator's time
//!   on a simulated node configured to follow it. Control loops, timers,
//!   freshness checks and stamps read it.
//! - [`steady`], a [`WallInstant`]: auth TTLs, hardware drivers, reconnect
//!   backoff and latency measurement, which must run on real time.
//!
//! On sim time a stale feed stops every timer; nothing falls back to wall
//! time. Agent services build their clock from the node config
//! (`ados_time::from_config`); a plugin builds it with [`Clock::from_env`],
//! which follows the agent's local relay named in [`CLOCK_FEED_ENV`].

mod feed;
pub mod wire;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

pub use feed::{SimFeed, RECONNECT_EVERY, STALE_AFTER};
pub use wire::SIMCLOCK_V1_VERSION;

/// The environment variable the plugin host sets on a node following the
/// simulator's clock: the path of the agent's local relay socket.
pub const CLOCK_FEED_ENV: &str = "ADOS_CLOCK_FEED";

/// A point in simulator time, nanoseconds since the run began.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SimInstant(u64);

impl SimInstant {
    pub fn from_ns(ns: u64) -> Self {
        SimInstant(ns)
    }
    pub fn as_ns(self) -> u64 {
        self.0
    }
    pub fn as_ms(self) -> u64 {
        self.0 / 1_000_000
    }
}

/// A point in steady wall time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WallInstant(Instant);

impl WallInstant {
    pub fn now() -> Self {
        WallInstant(Instant::now())
    }
    pub fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
    pub fn duration_since(&self, earlier: WallInstant) -> Duration {
        self.0.saturating_duration_since(earlier.0)
    }
    pub fn as_std(&self) -> Instant {
        self.0
    }
}

/// Steady wall time, whatever clock the control loops run on.
pub fn steady() -> WallInstant {
    WallInstant::now()
}

/// ADOS time: wall or sim.
#[derive(Clone)]
pub enum Clock {
    /// Wall time, counted from this instant.
    Wall(WallInstant),
    Sim(Arc<SimFeed>),
}

impl Clock {
    /// Wall time counted from now.
    pub fn wall() -> Clock {
        Clock::Wall(WallInstant::now())
    }

    /// A plugin's clock: the agent's relay socket named in
    /// [`CLOCK_FEED_ENV`] when the host set it, wall otherwise. Needs a Tokio
    /// runtime when set.
    pub fn from_env() -> Clock {
        match std::env::var_os(CLOCK_FEED_ENV) {
            Some(p) if !p.is_empty() => Clock::Sim(SimFeed::follow_relay(p.into())),
            _ => Clock::wall(),
        }
    }

    /// `wall` or `sim`.
    pub fn source(&self) -> &'static str {
        match self {
            Clock::Wall(_) => "wall",
            Clock::Sim(_) => "sim",
        }
    }

    /// Milliseconds of ADOS time: since the clock was made on wall, since the
    /// run began on sim (0 before the first tick).
    pub fn now_ms(&self) -> u64 {
        match self {
            Clock::Wall(base) => base.elapsed().as_millis() as u64,
            Clock::Sim(f) => f.sim_ns().unwrap_or(0) / 1_000_000,
        }
    }

    /// [`Clock::now_ms`] in nanoseconds.
    pub fn now_ns(&self) -> u64 {
        match self {
            Clock::Wall(base) => base.elapsed().as_nanos() as u64,
            Clock::Sim(f) => f.sim_ns().unwrap_or(0),
        }
    }

    /// How much ADOS time passes per wall second: 1 on wall, the simulator's
    /// speed on sim, `None` while it runs as fast as it can (no fixed ratio).
    /// Converts a wall-clock age (a stamp from another process) to ADOS time.
    pub fn wall_to_ados(&self) -> Option<f64> {
        match self {
            Clock::Wall(_) => Some(1.0),
            Clock::Sim(f) => Some(f.speed()).filter(|s| *s > 0.0),
        }
    }

    /// The latest sim time; `None` on wall or before the first tick.
    pub fn sim_ns(&self) -> Option<u64> {
        match self {
            Clock::Wall(_) => None,
            Clock::Sim(f) => f.sim_ns(),
        }
    }

    pub fn sim_now(&self) -> Option<SimInstant> {
        self.sim_ns().map(SimInstant::from_ns)
    }

    /// Wall milliseconds since the latest sim tick; `None` on wall.
    pub fn lag_ms(&self) -> Option<u32> {
        match self {
            Clock::Wall(_) => None,
            Clock::Sim(f) => f.lag_ms(),
        }
    }

    /// The simulator's pacing speed (0 for as fast as it can); `None` on wall.
    pub fn speed(&self) -> Option<f64> {
        match self {
            Clock::Wall(_) => None,
            Clock::Sim(f) => Some(f.speed()),
        }
    }

    pub fn is_stale(&self) -> bool {
        matches!(self, Clock::Sim(f) if f.is_stale())
    }

    /// Wait until ADOS time reaches `t_ms`.
    pub async fn sleep_until_ms(&self, t_ms: u64) {
        match self {
            Clock::Wall(base) => {
                let at = base.as_std() + Duration::from_millis(t_ms);
                tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
            }
            Clock::Sim(f) => {
                let mut rx = f.subscribe();
                let target = t_ms.saturating_mul(1_000_000);
                loop {
                    let now = *rx.borrow_and_update();
                    if !f.is_stale() && now.is_some_and(|n| n >= target) {
                        return;
                    }
                    if rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
    }

    /// Wait `d` of ADOS time.
    pub async fn sleep(&self, d: Duration) {
        let t = self.now_ms().saturating_add(d.as_millis() as u64);
        self.sleep_until_ms(t).await;
    }

    /// A periodic timer on ADOS time. The first tick is immediate; missed
    /// ticks are skipped.
    pub fn interval(&self, period: Duration) -> Interval {
        match self {
            Clock::Wall(base) => {
                let mut iv = tokio::time::interval(period);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                Interval::Wall { iv, base: *base }
            }
            Clock::Sim(f) => Interval::Sim {
                feed: Arc::clone(f),
                rx: f.subscribe(),
                period_ns: (period.as_nanos() as u64).max(1),
                next_ns: None,
            },
        }
    }
}

/// A periodic timer from [`Clock::interval`].
pub enum Interval {
    Wall {
        iv: tokio::time::Interval,
        base: WallInstant,
    },
    Sim {
        feed: Arc<SimFeed>,
        rx: watch::Receiver<Option<u64>>,
        period_ns: u64,
        next_ns: Option<u64>,
    },
}

impl Interval {
    /// Wait for the next tick; returns ADOS time in milliseconds.
    pub async fn tick(&mut self) -> u64 {
        match self {
            Interval::Wall { iv, base } => {
                iv.tick().await;
                base.elapsed().as_millis() as u64
            }
            Interval::Sim {
                feed,
                rx,
                period_ns,
                next_ns,
            } => loop {
                let now = *rx.borrow_and_update();
                if let (Some(now), false) = (now, feed.is_stale()) {
                    match *next_ns {
                        None => {
                            *next_ns = Some(now + *period_ns);
                            return now / 1_000_000;
                        }
                        Some(n) if now >= n => {
                            *next_ns = Some(n + *period_ns * ((now - n) / *period_ns + 1));
                            return now / 1_000_000;
                        }
                        Some(_) => {}
                    }
                }
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests;
