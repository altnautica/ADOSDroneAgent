//! Shared receive-plane seams: the valid-decode counter, the production channel
//! setter + monotonic clock, the data-RX process handle the watchdog polls, and
//! the live-channel read.
//!
//! These implement the watchdog's and the acquirer's injected seams so the run
//! loop can wire one shared counter / clock / process handle across the stats
//! reader, the watchdog, and the acquirer.

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

/// Real channel setter: `iw <iface> set channel <n>` over the monitor interface
/// (the GS-side async sibling of the hop listener's channel set). Returns true
/// when `iw` reports success.
#[derive(Debug, Default)]
pub struct IwChannelSetter;

impl ChannelSetter for IwChannelSetter {
    fn set_channel<'a>(
        &'a self,
        interface: &'a str,
        channel: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        let iface = interface.to_string();
        Box::pin(async move {
            let out = tokio::process::Command::new("iw")
                .args([&iface, "set", "channel", &channel.to_string()])
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    tracing::warn!(
                        channel,
                        stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                        "acquire_set_channel_failed"
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!(channel, error = %e, "acquire_set_channel_error");
                    false
                }
            }
        })
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
    let out = tokio::time::timeout(
        LIVE_CHANNEL_READ_TIMEOUT,
        tokio::process::Command::new("iw")
            .args([iface, "info"])
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
