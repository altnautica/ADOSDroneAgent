//! Shared receive-plane seams: the valid-decode counter, the production channel
//! setter + monotonic clock, the data-RX process handle the watchdog polls, and
//! the live-channel read.
//!
//! These implement the watchdog's and the acquirer's injected seams so the run
//! loop can wire one shared counter / clock / process handle across the stats
//! reader, the watchdog, and the acquirer.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::acquire::{ChannelSetter, ValidPacketCounter};
use crate::process_spawn::GsWfbProcess;
use crate::watchdog::{Clock, RxProcess};

/// The cumulative valid-decode packet counter the stats reader updates and the
/// watchdog/acquirer read. Implements both the watchdog's and the acquirer's
/// counter seams.
#[derive(Debug, Default, Clone)]
pub struct SharedValidCounter {
    inner: Arc<AtomicI64>,
}

impl SharedValidCounter {
    pub fn new() -> Self {
        Self::default()
    }
    /// Add this interval's valid-decode count (the per-interval `packets_received`).
    pub fn add(&self, n: i64) {
        if n > 0 {
            self.inner.fetch_add(n, Ordering::SeqCst);
        }
    }
    pub fn get(&self) -> i64 {
        self.inner.load(Ordering::SeqCst)
    }
}

impl ValidPacketCounter for SharedValidCounter {
    fn valid_packets(&self) -> i64 {
        self.get()
    }
}

/// Per-call ceiling on `iw set channel`.
///
/// The RTL drivers this receiver runs on can wedge mid-retune — recovering from
/// exactly that is why `rtl_modprobe` exists — and an unbounded spawn makes a
/// wedged `iw` permanent: the future never resolves, so the valid-packet
/// watchdog's [`ChannelAcquirer`](crate::acquire::ChannelAcquirer) sweep and
/// `HopFollower::follow_at_epoch` both park forever on it. That leaves the
/// ground station unable to retune to the channel the drone hopped to, with no
/// exit signal and (until the sibling watchdog pinger lands) no systemd
/// restart — recovery needed a power cycle or SSH.
///
/// Matches the drone-side `ados_radio::bringup::SET_CHANNEL_TIMEOUT`: the same
/// command against the same driver family, so the two halves of a coordinated
/// hop give up on the same budget.
const SET_CHANNEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Real channel setter: `iw <iface> set channel <n>` over the monitor interface
/// (the GS-side async sibling of the hop listener's channel set). Returns true
/// when `iw` reports success; a timeout returns false so the acquirer advances
/// to the next candidate channel instead of stalling on this one.
#[derive(Debug, Default)]
pub struct IwChannelSetter;

impl ChannelSetter for IwChannelSetter {
    fn set_channel<'a>(
        &'a self,
        interface: &'a str,
        channel: u8,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        let iface = interface.to_string();
        Box::pin(async move {
            let mut cmd = tokio::process::Command::new("iw");
            cmd.args([&iface, "set", "channel", &channel.to_string()]);
            run_retune_bounded(cmd, channel, SET_CHANNEL_TIMEOUT).await
        })
    }
}

/// Run one retune command under `budget`, reporting success only on exit 0.
///
/// Takes the built command rather than the program name so the bound itself is
/// exercisable: a test hands it a command that never exits and asserts the false
/// verdict, which is the whole point of the seam — `iw` is absent on a dev host
/// and a real driver wedge cannot be staged on demand.
async fn run_retune_bounded(
    mut cmd: tokio::process::Command,
    channel: u8,
    budget: std::time::Duration,
) -> bool {
    // `kill_on_drop` is what makes the timeout an actual bound: dropping the
    // `output()` future at the deadline otherwise leaves the wedged `iw`
    // running, and one leaks per sweep step until the process table fills.
    let out = tokio::time::timeout(budget, cmd.kill_on_drop(true).output()).await;
    match out {
        Ok(Ok(o)) if o.status.success() => true,
        Ok(Ok(o)) => {
            tracing::warn!(
                channel,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "acquire_set_channel_failed"
            );
            false
        }
        Ok(Err(e)) => {
            tracing::warn!(channel, error = %e, "acquire_set_channel_error");
            false
        }
        Err(_) => {
            tracing::warn!(
                channel,
                timeout_s = budget.as_secs(),
                "acquire_set_channel_timeout"
            );
            false
        }
    }
}

/// Per-call ceiling on the live-channel `iw info` read so a hung `iw` (driver
/// wedged) cannot stall the stats loop.
const LIVE_CHANNEL_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Read the interface's LIVE channel from `iw <iface> info`, or `None` when `iw`
/// cannot be run or its output carries no channel. The acquirer sweep can land
/// the netdev on a different channel than the configured/operating one, so the
/// sidecar reads the live value rather than reporting the configured channel.
pub(super) async fn live_channel(iface: &str) -> Option<u8> {
    // `kill_on_drop` reaps the child the timeout abandons. Without it a wedged
    // driver leaks one `iw` per stats tick, which on a 1 Hz loop is thousands
    // per hour on a node whose radio is already in trouble.
    let out = tokio::time::timeout(
        LIVE_CHANNEL_READ_TIMEOUT,
        tokio::process::Command::new("iw")
            .args([iface, "info"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    parse_iface_channel(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the `channel <N>` token out of an `iw <iface> info` body. The line
/// reads e.g. `\tchannel 149 (5745 MHz), width: 20 MHz, …`; the first integer
/// after the `channel` keyword is the channel number. Pure helper, symmetric
/// with the drone-side parser.
fn parse_iface_channel(info: &str) -> Option<u8> {
    for line in info.lines() {
        let mut toks = line.split_whitespace();
        while let Some(tok) = toks.next() {
            if tok == "channel" {
                if let Some(n) = toks.next() {
                    if let Ok(ch) = n.parse::<u8>() {
                        return Some(ch);
                    }
                }
            }
        }
    }
    None
}

/// Monotonic system clock (the production `Clock` seam).
#[derive(Debug, Default)]
pub struct SystemClock {
    epoch: std::sync::OnceLock<std::time::Instant>,
}

impl Clock for SystemClock {
    fn monotonic(&self) -> f64 {
        let start = self.epoch.get_or_init(std::time::Instant::now);
        start.elapsed().as_secs_f64()
    }
}

/// Wraps a live `WfbProcess` so the watchdog can poll liveness + terminate it.
/// The data-RX child is shared (the stats reader takes its stdout; the watchdog
/// holds this handle to assert liveness and request a restart).
pub struct DataRxHandle {
    proc: Mutex<Option<GsWfbProcess>>,
    terminated: AtomicU32,
}

impl DataRxHandle {
    pub fn new(proc: GsWfbProcess) -> Arc<Self> {
        Arc::new(Self {
            proc: Mutex::new(Some(proc)),
            terminated: AtomicU32::new(0),
        })
    }
}

impl RxProcess for DataRxHandle {
    fn is_running(&self) -> bool {
        // try_lock so a liveness poll never blocks behind a kill; treat a
        // contended lock as "alive" (the killer holds it only momentarily).
        match self.proc.try_lock() {
            Ok(mut guard) => guard.as_mut().map(|p| p.is_running()).unwrap_or(false),
            Err(_) => true,
        }
    }
    fn terminate(&self) {
        self.terminated.fetch_add(1, Ordering::SeqCst);
        // Best-effort: take the process out and drop it so its `Drop` fires the
        // synchronous killpg without this fn having to await a wait. Dropping
        // the handle is the structural kill (the whole process group dies); the
        // run loop respawns it on the next generation. A contended lock means a
        // kill is already in flight, so skip.
        if let Ok(mut guard) = self.proc.try_lock() {
            guard.take();
        }
    }
    fn terminate_count(&self) -> u32 {
        self.terminated.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::acquire::ChannelAcquirer;
    use std::collections::BTreeSet;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    /// A retune command that never exits, standing in for the wedged RTL driver
    /// `rtl_modprobe` exists to recover from. `sleep` is on every host `iw` is
    /// not, so the bound is asserted on a dev machine and on the SBC alike.
    fn never_exits() -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("600");
        cmd
    }

    #[tokio::test]
    async fn a_retune_that_never_returns_is_a_bounded_failure() {
        // Before the bound this future simply never resolved, and both callers —
        // the valid-packet watchdog's channel sweep and the hop follower — parked
        // on it for the rest of the process lifetime. The ground station could
        // then never retune to the channel the drone hopped to, with no exit
        // signal and no restart; recovery took a power cycle or SSH.
        let started = tokio::time::Instant::now();
        let verdict = run_retune_bounded(never_exits(), 149, Duration::from_millis(150)).await;
        assert!(
            !verdict,
            "a retune that never completes must report failure, not hang"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the bound must resolve well inside the 600 s the child would take"
        );
    }

    #[tokio::test]
    async fn a_wedged_retune_lets_the_sweep_reach_the_next_channel() {
        // The consequence that matters: the acquirer must keep sweeping. The
        // first candidate wedges, so the setter bounds it and reports false; the
        // acquirer moves on and locks the channel the peer is actually on.
        struct WedgeThenSucceed {
            wedge_channel: u8,
            decoding: Arc<AtomicBool>,
            attempts: Arc<Mutex<Vec<u8>>>,
        }
        impl ChannelSetter for WedgeThenSucceed {
            fn set_channel<'a>(
                &'a self,
                _interface: &'a str,
                channel: u8,
            ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
                Box::pin(async move {
                    self.attempts.lock().await.push(channel);
                    if channel == self.wedge_channel {
                        // Exactly what the production setter does with a hung
                        // `iw`: bound it, reap it, report failure.
                        return run_retune_bounded(
                            never_exits(),
                            channel,
                            Duration::from_millis(50),
                        )
                        .await;
                    }
                    // A healthy retune, after which the peer's frames decode.
                    self.decoding.store(true, Ordering::SeqCst);
                    true
                })
            }
        }

        let counter = SharedValidCounter::new();
        let decoding = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(Mutex::new(Vec::new()));
        // The counter advances only once a successful retune landed, so a lock
        // can never be credited to the wedged channel.
        let reader = {
            let counter = counter.clone();
            let decoding = decoding.clone();
            move || {
                if decoding.load(Ordering::SeqCst) {
                    counter.add(1);
                }
                counter.get()
            }
        };
        let setter = Arc::new(WedgeThenSucceed {
            wedge_channel: 36,
            decoding: decoding.clone(),
            attempts: attempts.clone(),
        });
        let mut acq = ChannelAcquirer::new(
            "wlan1",
            "custom",
            Arc::new(reader),
            setter,
            0.02,
            1,
            Some(BTreeSet::from([36u8, 40u8])),
        );
        let locked = tokio::time::timeout(Duration::from_secs(10), acq.acquire())
            .await
            .expect("the sweep must not stall on the wedged channel");
        assert_eq!(
            locked,
            Some(40),
            "the sweep must advance past the wedged channel and lock the next one"
        );
        assert_eq!(
            attempts.lock().await.as_slice(),
            &[36, 40],
            "the wedged channel must be tried and abandoned, in order"
        );
    }

    #[test]
    fn shared_counter_accumulates_positive_intervals_only() {
        let c = SharedValidCounter::new();
        assert_eq!(c.get(), 0);
        c.add(5);
        c.add(0); // ignored
        c.add(3);
        assert_eq!(c.get(), 8);
        assert_eq!(c.valid_packets(), 8);
    }

    #[test]
    fn system_clock_is_monotone() {
        let clk = SystemClock::default();
        let a = clk.monotonic();
        let b = clk.monotonic();
        assert!(b >= a);
    }

    #[test]
    fn parse_iface_channel_reads_channel_token() {
        // The live-channel readback the stats loop uses for `actual_channel`.
        let info = "Interface wlan0\n\tifindex 5\n\ttype monitor\n\
                    \tchannel 149 (5745 MHz), width: 20 MHz, center1: 5745 MHz\n";
        assert_eq!(parse_iface_channel(info), Some(149));
        let other = "Interface wlan0\n\tchannel 44 (5220 MHz), width: 20 MHz\n";
        assert_eq!(parse_iface_channel(other), Some(44));
    }

    #[test]
    fn parse_iface_channel_no_channel_is_none() {
        assert_eq!(
            parse_iface_channel("Interface wlan0\n\ttype managed\n"),
            None
        );
        assert_eq!(parse_iface_channel(""), None);
        assert_eq!(parse_iface_channel("\tchannel\n"), None);
    }
}
