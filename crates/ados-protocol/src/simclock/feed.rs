//! The sim-clock feed: the latest simulator time, received over UDP from the
//! simulator or over the agent's local relay socket.
//!
//! Ticks are taken in `seq` order only (an older or repeated `seq` is
//! dropped) and sim time never moves backwards within a run. A feed silent for
//! longer than [`STALE_AFTER`] wall time is stale: sim timers stop until the
//! next tick arrives, and nothing falls back to wall time.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use super::wire::{Tick, TICK_LEN};

/// Wall silence after which the feed is stale.
pub const STALE_AFTER: Duration = Duration::from_millis(500);
/// How often the listener re-checks staleness.
const STALE_CHECK: Duration = Duration::from_millis(100);
/// How often a relay follower retries a refused or dropped connection.
pub const RECONNECT_EVERY: Duration = Duration::from_secs(1);

fn stale_check() -> tokio::time::Interval {
    let mut check = tokio::time::interval(STALE_CHECK);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    check
}

#[derive(Default)]
struct Last {
    seq: Option<u32>,
    rx_at: Option<Instant>,
}

/// The latest sim time and the feed's health.
pub struct SimFeed {
    tx: watch::Sender<Option<u64>>,
    rx: watch::Receiver<Option<u64>>,
    stale: AtomicBool,
    /// `f64` bits of the simulator's pacing speed.
    speed: AtomicU64,
    last: Mutex<Last>,
}

impl SimFeed {
    /// A feed with no time yet (stale until the first tick).
    pub fn new() -> Arc<SimFeed> {
        let (tx, rx) = watch::channel(None);
        Arc::new(SimFeed {
            tx,
            rx,
            stale: AtomicBool::new(true),
            speed: AtomicU64::new(0f64.to_bits()),
            last: Mutex::new(Last::default()),
        })
    }

    /// Bind `addr` and feed every received `TICK` in; a background task also
    /// marks the feed stale when it goes silent. Needs a Tokio runtime.
    pub fn listen(addr: SocketAddr) -> std::io::Result<Arc<SimFeed>> {
        let std_sock = std::net::UdpSocket::bind(addr)?;
        std_sock.set_nonblocking(true)?;
        let sock = tokio::net::UdpSocket::from_std(std_sock)?;
        let feed = SimFeed::new();
        let f = Arc::clone(&feed);
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let mut check = stale_check();
            loop {
                tokio::select! {
                    r = sock.recv_from(&mut buf) => match r {
                        Ok((n, _)) => f.take(&buf[..n]),
                        Err(e) => tracing::warn!(error = %e, "simclock_recv_failed"),
                    },
                    _ = check.tick() => { f.check_stale(Instant::now()); }
                }
            }
        });
        tracing::info!(%addr, "simclock_feed_listening");
        Ok(feed)
    }

    /// Follow the agent's local relay (a plugin's `ADOS_CLOCK_FEED`): a Unix
    /// stream socket that writes every `TICK` as its 32 bytes. A refused or
    /// dropped connection is retried every [`RECONNECT_EVERY`]; the feed goes
    /// stale meanwhile. Needs a Tokio runtime.
    pub fn follow_relay(path: PathBuf) -> Arc<SimFeed> {
        let feed = SimFeed::new();
        let f = Arc::clone(&feed);
        tokio::spawn(async move {
            let mut check = stale_check();
            loop {
                check.tick().await;
                f.check_stale(Instant::now());
            }
        });
        let f = Arc::clone(&feed);
        tokio::spawn(async move {
            loop {
                if let Ok(mut s) = tokio::net::UnixStream::connect(&path).await {
                    let mut buf = [0u8; TICK_LEN];
                    while s.read_exact(&mut buf).await.is_ok() {
                        f.take(&buf);
                    }
                }
                tokio::time::sleep(RECONNECT_EVERY).await;
            }
        });
        feed
    }

    fn take(&self, d: &[u8]) {
        match Tick::decode(d) {
            Some(t) => {
                self.ingest(&t, Instant::now());
            }
            None => tracing::debug!(len = d.len(), "simclock_datagram_refused"),
        }
    }

    /// Take one tick received at wall `at`. Returns false when it was dropped
    /// (an older or repeated `seq`, or sim time going backwards).
    pub fn ingest(&self, tick: &Tick, at: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = last.seq {
            // Wrapping comparison: newer means ahead by less than half the range.
            if tick.seq.wrapping_sub(prev) == 0 || tick.seq.wrapping_sub(prev) > u32::MAX / 2 {
                return false;
            }
        }
        if self.rx.borrow().is_some_and(|now| tick.sim_ns < now) {
            return false;
        }
        last.seq = Some(tick.seq);
        last.rx_at = Some(at);
        self.speed.store(tick.speed.to_bits(), Ordering::Relaxed);
        self.stale.store(false, Ordering::Release);
        // Always notify: a tick after a stale gap must wake waiting timers even
        // when the time itself did not move (paused).
        self.tx.send_modify(|t| *t = Some(tick.sim_ns));
        true
    }

    /// Mark the feed stale when no tick arrived within [`STALE_AFTER`] of `at`.
    /// Returns the stale state.
    pub fn check_stale(&self, at: Instant) -> bool {
        let last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let silent = last
            .rx_at
            .is_none_or(|r| at.saturating_duration_since(r) > STALE_AFTER);
        if silent {
            self.stale.store(true, Ordering::Release);
        }
        self.stale.load(Ordering::Acquire)
    }

    pub fn is_stale(&self) -> bool {
        self.stale.load(Ordering::Acquire)
    }

    /// The latest sim time, `None` before the first tick.
    pub fn sim_ns(&self) -> Option<u64> {
        *self.rx.borrow()
    }

    /// The simulator's pacing speed from the latest tick.
    pub fn speed(&self) -> f64 {
        f64::from_bits(self.speed.load(Ordering::Relaxed))
    }

    /// Wall milliseconds since the latest tick arrived.
    pub fn lag_ms(&self) -> Option<u32> {
        let last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        last.rx_at
            .map(|r| r.elapsed().as_millis().min(u128::from(u32::MAX)) as u32)
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<u64>> {
        self.rx.clone()
    }
}
