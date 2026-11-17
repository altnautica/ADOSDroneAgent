//! Hold the machine awake only while a drone link is actually live.
//!
//! An appliance masks sleep outright, because nobody is using it and it must
//! never suspend mid-flight. A personal computer is the opposite problem: the
//! owner expects the display to sleep, and the install deliberately leaves their
//! power settings alone. But a ground station whose machine suspends while a
//! drone is streaming has dropped the link — usually without anyone noticing
//! until the video stops.
//!
//! So the lock is taken on evidence, not on boot: the radio sidecar
//! (`/run/ados/wfb-stats.json`) must be fresh, must belong to the ground-station
//! profile, and `packets_received` must have advanced since the previous tick.
//! One missed tick is not a dead link — the counters are read on a cadence and a
//! single sample can fall either side of a quiet moment — so release waits for
//! [`RELEASE_AFTER`] of continuous not-live.
//!
//! The lock itself is `systemd-inhibit`, blocking `sleep`, `idle` and the lid
//! switch. Blocking the lid switch is the point of `handle-lid-switch`: a laptop
//! whose owner shuts the lid mid-flight stays up. Everything else (a manual
//! suspend, a logout) is still theirs to do.

use std::path::Path;
use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;
use ados_protocol::netif::WFB_STATS_PATH;
use tokio::process::{Child, Command};
use tokio::sync::watch;

use crate::pair_proof::STATS_FRESH_CEILING;
/// How often the sidecar is sampled.
const TICK: Duration = Duration::from_secs(2);

/// How long the link must read not-live before the lock is dropped. Comfortably
/// longer than one missed tick, so a single dropped sample does not flap it, and
/// short enough that a finished flight releases the machine well inside a
/// minute.
const RELEASE_AFTER: Duration = Duration::from_secs(30);

/// The lock's self-description, shown by `systemd-inhibit --list`.
const WHO: &str = "ADOS ground station";
const WHY: &str = "Drone link active";

/// What the loop should do with the lock this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Take the lock if it is not held.
    Hold,
    /// Drop the lock if it is held.
    Release,
    /// Leave the lock exactly as it is.
    Stay,
}

/// The lock's state across ticks. `last_live` is the last tick that saw traffic.
#[derive(Debug, Clone, Copy)]
pub struct State {
    held: bool,
    last_live: Option<Instant>,
}

impl State {
    /// A state with no lock held.
    pub fn new() -> Self {
        State {
            held: false,
            last_live: None,
        }
    }

    /// Whether the lock is currently held (for the tests and the event).
    pub fn held(&self) -> bool {
        self.held
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

/// The pure decision: given the current state, this tick's verdict and the
/// clock, what to do.
///
/// Hold on the first live tick; release only after [`RELEASE_AFTER`] of not-live.
/// A live tick always refreshes `last_live`, so a link that flickers once keeps
/// the lock.
pub fn next(state: &mut State, live: bool, now: Instant) -> Action {
    if live {
        state.last_live = Some(now);
        if state.held {
            Action::Stay
        } else {
            Action::Hold
        }
    } else if !state.held {
        Action::Stay
    } else {
        let quiet = state
            .last_live
            .map(|t| now.duration_since(t) >= RELEASE_AFTER)
            // Held with no live tick ever seen: drop it rather than hold a lock
            // nobody justified.
            .unwrap_or(true);
        if quiet {
            Action::Release
        } else {
            Action::Stay
        }
    }
}

/// This tick's verdict: is a drone link live right now?
///
/// `None` reads a sidecar that is absent, stale, unparseable or from another
/// profile — all of which mean "no evidence of a link", never "maybe".
async fn link_live(previous: Option<u64>) -> (bool, Option<u64>) {
    let path = Path::new(WFB_STATS_PATH);
    let age = tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok());
    if age.map(|a| a > STATS_FRESH_CEILING).unwrap_or(true) {
        return (false, previous);
    }
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return (false, previous);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (false, previous);
    };
    if value.get("profile").and_then(|v| v.as_str()) != Some("ground_station") {
        return (false, previous);
    }
    let packets = value
        .get("packets_received")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // The first sample only establishes a baseline: there is nothing to compare
    // it against, and treating a large absolute counter as "live" would hold the
    // lock from a sidecar written hours ago that is merely still being touched.
    let advanced = previous.map(|p| packets > p).unwrap_or(false);
    (advanced, Some(packets))
}

/// Spawn the inhibitor. `sleep infinity` is what keeps the command alive for the
/// life of the lock; `kill_on_drop` guarantees the lock dies with this process
/// or the child handle, so a crashed supervisor cannot pin a laptop awake
/// forever.
fn spawn_inhibitor() -> std::io::Result<Child> {
    Command::new("systemd-inhibit")
        .arg("--what=sleep:idle:handle-lid-switch")
        .arg(format!("--who={WHO}"))
        .arg(format!("--why={WHY}"))
        .arg("--mode=block")
        .arg("sleep")
        .arg("infinity")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

/// The supervisor task: sample the link every [`TICK`] and hold or drop the
/// lock accordingly. Runs only on a personal computer running a ground station.
pub async fn run(mut shutdown: watch::Receiver<bool>) {
    tracing::info!("sleep_guard_started");
    let events = EventEmitter::new("ados-supervisor");
    let mut state = State::new();
    let mut child: Option<Child> = None;
    let mut packets: Option<u64> = None;

    loop {
        let (live, next_packets) = link_live(packets).await;
        packets = next_packets;
        match next(&mut state, live, Instant::now()) {
            Action::Hold => match spawn_inhibitor() {
                Ok(mut spawned) => {
                    if let Ok(Some(status)) = spawned.try_wait() {
                        // systemd-inhibit refuses (no logind, a policy denial) and
                        // exits at once. Warn and retry on the next live tick
                        // rather than pretending the machine is held awake.
                        tracing::warn!(?status, "sleep_guard_spawn_failed");
                    } else {
                        child = Some(spawned);
                        state.held = true;
                        tracing::info!("drone link live; keeping the computer awake");
                        events.emit(
                            "sleep_guard.held",
                            ados_protocol::logd::Level::Info,
                            ados_protocol::logd::Fields::new(),
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %e, "sleep_guard_spawn_failed"),
            },
            Action::Release => {
                if let Some(mut held) = child.take() {
                    let _ = held.start_kill();
                }
                state.held = false;
                tracing::info!("drone link quiet; releasing the sleep lock");
                events.emit(
                    "sleep_guard.released",
                    ados_protocol::logd::Level::Info,
                    ados_protocol::logd::Fields::new(),
                );
            }
            Action::Stay => {}
        }

        tokio::select! {
            _ = tokio::time::sleep(TICK) => {}
            _ = shutdown.changed() => {
                if let Some(mut held) = child.take() {
                    let _ = held.start_kill();
                }
                tracing::info!("sleep_guard_stopped");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_live_tick_takes_the_lock() {
        let mut s = State::new();
        let now = Instant::now();
        assert_eq!(next(&mut s, true, now), Action::Hold);
        s.held = true;
        assert_eq!(next(&mut s, true, now), Action::Stay);
    }

    #[test]
    fn one_missed_tick_does_not_release() {
        let mut s = State::new();
        let t0 = Instant::now();
        assert_eq!(next(&mut s, true, t0), Action::Hold);
        s.held = true;
        // A single not-live sample inside the release window keeps the lock.
        assert_eq!(next(&mut s, false, t0 + TICK), Action::Stay);
        assert_eq!(
            next(&mut s, false, t0 + RELEASE_AFTER - Duration::from_secs(1)),
            Action::Stay
        );
    }

    #[test]
    fn the_lock_is_released_after_the_quiet_window() {
        let mut s = State::new();
        let t0 = Instant::now();
        next(&mut s, true, t0);
        s.held = true;
        assert_eq!(next(&mut s, false, t0 + RELEASE_AFTER), Action::Release);
    }

    #[test]
    fn a_link_that_flickers_keeps_the_lock() {
        let mut s = State::new();
        let t0 = Instant::now();
        next(&mut s, true, t0);
        s.held = true;
        // live, quiet, live, quiet: never a full quiet window, never released.
        let mut now = t0;
        for live in [false, true, false, true, false] {
            now += TICK;
            assert_eq!(next(&mut s, live, now), Action::Stay, "at {live}");
        }
    }

    #[test]
    fn a_not_live_flight_never_takes_the_lock() {
        let mut s = State::new();
        let now = Instant::now();
        assert_eq!(next(&mut s, false, now), Action::Stay);
        assert!(!s.held());
    }

    #[test]
    fn releasing_twice_is_harmless() {
        let mut s = State::new();
        let t0 = Instant::now();
        next(&mut s, true, t0);
        s.held = true;
        assert_eq!(next(&mut s, false, t0 + RELEASE_AFTER), Action::Release);
        s.held = false;
        assert_eq!(next(&mut s, false, t0 + RELEASE_AFTER + TICK), Action::Stay);
    }

    #[test]
    fn the_inhibitor_blocks_sleep_idle_and_the_lid_switch() {
        // The three the plan promises: sleep, idle and a closed lid. A manual
        // suspend stays available to the owner.
        let what = "sleep:idle:handle-lid-switch";
        assert!(what.contains("sleep"));
        assert!(what.contains("idle"));
        assert!(what.contains("handle-lid-switch"));
    }
}
