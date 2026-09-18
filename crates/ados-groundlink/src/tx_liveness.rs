//! Delta-counter liveness for the ground station's two `wfb_tx` transmitters.
//!
//! The ground station is the node that carries command authority. Its
//! `tx_control` (`wfb_tx -p 1`) radiates HopAck and the presence beacon; its
//! `aux_tx` (`wfb_tx -p 3`) carries the ENTIRE ground→drone uplink — arm,
//! disarm, mode change, mission upload, parameter writes, relay-proxy RPC and
//! the link feedback the drone's adaptive ladder runs on.
//!
//! Both were held only so their `Drop` would killpg them. Nothing observed
//! either one: no counter, no `try_wait`, no stats stream. A `wfb_tx` that goes
//! silently dead without exiting therefore took down every one of those paths
//! while video RX kept decoding and every surface still read `active` /
//! `locked` — a ground station with no command authority that looks healthy.
//! Process liveness is not proof of work.
//!
//! ## The signal
//!
//! `wfb_tx` prints one `PKT` line to stdout every `log_interval` (1000 ms by
//! default, so no `-l` flag is needed):
//!
//! ```text
//! <ts_ms>\tPKT\t<fec_timeouts>:<p_in>:<b_in>:<p_injected>:<b_injected>:<p_dropped>:<p_truncated>
//! ```
//!
//! The counts are PER INTERVAL — reset after each line — and the line is
//! emitted from the top of the poll loop unconditionally, whether or not any
//! traffic moved. That gives two independent, per-transmitter facts:
//!
//! 1. **The line arriving at all** proves the process's main loop is turning.
//!    Its absence is the wedge/death case, and it needs no traffic to detect.
//! 2. **`b_injected` advancing while `b_in` advances** proves bytes offered to
//!    the transmitter actually reached the radio. Ingress without injection is
//!    a live loop with a dead radio.
//!
//! This is deliberately NOT `/proc/<pid>/io` `wchar`. `wfb_tx` injects with
//! `sendmsg(2)` on a `PF_PACKET` socket, and the kernel only accounts `wchar`
//! from the `vfs_write` path — so `wchar` stays flat for a perfectly healthy
//! transmitter, and a watchdog keyed on it would restart the generation forever.
//! It is also not the interface `tx_bytes` counter: that is a netdev counter on
//! an RTL monitor interface (an unreliable primary signal here), and it
//! aggregates both transmitters, so a dead one hides behind a live one.
//!
//! ## Idle is not a stall
//!
//! A transmitter with nothing to send is healthy. Both GS transmitters do have
//! guaranteed ingress in practice (the presence beacon every 10 s into
//! `tx_control`, the link-feedback emitter at 1 Hz into `aux_tx`), but this
//! module does not rely on that: with no ingress, [`TxVerdict::Idle`] is
//! returned and nothing fires. Only ingress-without-injection fires.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::time::Instant;

/// How often the window is evaluated. Shorter than the silence budget so a
/// stall is caught within one budget rather than two.
pub const TX_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a transmitter may go without a `PKT` line, or without injecting
/// while ingress arrives, before it is declared dead. Matches the drone-side TX
/// watchdogs' 30 s window: `wfb_tx` prints every second, so 30 s tolerates 29
/// consecutive missed lines before a restart is triggered.
pub const TX_SILENCE_WINDOW: Duration = Duration::from_secs(30);

/// The per-interval counters off one `wfb_tx` `PKT` line. Only the three fields
/// the liveness judgement needs are kept; the rest of the line is diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxPkt {
    /// Bytes offered to the transmitter on its UDP ingress this interval.
    pub bytes_in: u64,
    /// Bytes actually injected onto the radio this interval.
    pub bytes_injected: u64,
    /// Packets the transmitter dropped this interval (kept for the log line, so
    /// a restart says whether the transmitter was failing loudly or silently).
    pub packets_dropped: u64,
}

/// Parse a `wfb_tx` stdout line, or `None` when it is not a `PKT` line.
///
/// `TX_ANT` lines and anything else on the stream are ignored rather than
/// treated as a fault: this parser is the liveness signal, and a stricter
/// reading would turn a format addition upstream into a false restart of the
/// node that carries command authority.
pub fn parse_tx_pkt(line: &str) -> Option<TxPkt> {
    let mut fields = line.split('\t');
    let _ts = fields.next()?;
    if fields.next()? != "PKT" {
        return None;
    }
    let mut counts = fields.next()?.split(':');
    // fec_timeouts : p_in : b_in : p_injected : b_injected : p_dropped : p_truncated
    let _fec_timeouts = counts.next()?;
    let _packets_in = counts.next()?;
    let bytes_in = counts.next()?.parse().ok()?;
    let _packets_injected = counts.next()?;
    let bytes_injected = counts.next()?.parse().ok()?;
    let packets_dropped = counts.next()?.parse().ok()?;
    Some(TxPkt {
        bytes_in,
        bytes_injected,
        packets_dropped,
    })
}

/// What one evaluation of a transmitter's window concludes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxVerdict {
    /// Bytes were offered and bytes were injected. The transmitter works.
    Healthy,
    /// The loop is turning but nothing was offered to transmit. Not a fault —
    /// and NOT a reason to restart a working transmitter.
    Idle,
    /// No `PKT` line arrived for the whole window: the process is alive (its
    /// `Drop` has not run) but its main loop has stopped.
    StalledSilent,
    /// Lines keep arriving and bytes keep being offered, but none reached the
    /// radio for the whole window.
    StalledNotInjecting,
}

impl TxVerdict {
    /// True when the generation must be restarted.
    pub fn is_stall(self) -> bool {
        matches!(
            self,
            TxVerdict::StalledSilent | TxVerdict::StalledNotInjecting
        )
    }

    /// Stable wire/log token.
    pub fn as_str(self) -> &'static str {
        match self {
            TxVerdict::Healthy => "healthy",
            TxVerdict::Idle => "idle",
            TxVerdict::StalledSilent => "stalled_silent",
            TxVerdict::StalledNotInjecting => "stalled_not_injecting",
        }
    }
}

/// One transmitter's rolling liveness window.
///
/// Split from the reader loop so the whole judgement — including both stall
/// shapes and the idle exemption — is exercisable without a radio, a subprocess
/// or a real clock: every method takes the instant to reason about.
#[derive(Debug)]
pub struct TxLivenessWindow {
    silence: Duration,
    /// When a `PKT` line last arrived.
    last_line: Instant,
    /// When the current accumulation window opened.
    window_start: Instant,
    /// Ingress + injection accumulated over the current window. The wire counts
    /// are per-interval, so they are summed rather than differenced.
    bytes_in: u64,
    bytes_injected: u64,
}

impl TxLivenessWindow {
    pub fn new(now: Instant, silence: Duration) -> Self {
        Self {
            silence,
            last_line: now,
            window_start: now,
            bytes_in: 0,
            bytes_injected: 0,
        }
    }

    /// Record one parsed `PKT` line.
    pub fn observe(&mut self, pkt: TxPkt, now: Instant) {
        self.last_line = now;
        self.bytes_in = self.bytes_in.saturating_add(pkt.bytes_in);
        self.bytes_injected = self.bytes_injected.saturating_add(pkt.bytes_injected);
    }

    /// Judge the window. Returns `None` while the window is still filling, so
    /// the caller can poll faster than the budget without acting on a partial
    /// observation. A non-stall verdict resets the accumulation.
    pub fn evaluate(&mut self, now: Instant) -> Option<TxVerdict> {
        // Line silence is checked FIRST and is not gated on the window: it needs
        // no traffic to be conclusive, and it is the shape a wedged or dead-but-
        // unexited transmitter takes.
        if now.duration_since(self.last_line) >= self.silence {
            return Some(TxVerdict::StalledSilent);
        }
        if now.duration_since(self.window_start) < self.silence {
            return None;
        }
        let verdict = if self.bytes_in == 0 {
            // Nothing was offered, so nothing could be injected. Healthy-idle.
            TxVerdict::Idle
        } else if self.bytes_injected == 0 {
            TxVerdict::StalledNotInjecting
        } else {
            TxVerdict::Healthy
        };
        if !verdict.is_stall() {
            self.window_start = now;
            self.bytes_in = 0;
            self.bytes_injected = 0;
        }
        Some(verdict)
    }
}

/// Watch one transmitter's stdout stats stream and resolve when it stalls.
///
/// Generic over the reader so a test drives it through an in-memory pipe; the
/// caller passes the child's piped stdout. Returns the firing verdict; it never
/// returns a healthy one, so the caller can treat resolution as "restart the
/// generation".
///
/// EOF (the transmitter exited and closed the pipe) is a stall too: the reader
/// stops producing lines, so the silence check fires on the next tick rather
/// than parking on a closed stream forever.
pub async fn watch_tx_liveness<R>(
    name: &'static str,
    stdout: R,
    poll: Duration,
    silence: Duration,
) -> TxVerdict
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stdout).lines();
    let mut window = TxLivenessWindow::new(Instant::now(), silence);
    let mut tick = tokio::time::interval(poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; skip it so the window is not judged
    // before it has had any chance to fill.
    tick.tick().await;
    let mut eof = false;
    loop {
        tokio::select! {
            line = lines.next_line(), if !eof => {
                match line {
                    Ok(Some(l)) => {
                        if let Some(pkt) = parse_tx_pkt(&l) {
                            window.observe(pkt, Instant::now());
                        }
                    }
                    // Pipe closed or unreadable: stop reading and let the
                    // silence check conclude. Returning here instead would skip
                    // the one-line diagnosis the tick path logs.
                    Ok(None) | Err(_) => eof = true,
                }
            }
            _ = tick.tick() => {
                if let Some(verdict) = window.evaluate(Instant::now()) {
                    if verdict.is_stall() {
                        tracing::error!(
                            transmitter = name,
                            verdict = verdict.as_str(),
                            silence_s = silence.as_secs(),
                            "ground_tx_stalled: restarting the receive generation"
                        );
                        return verdict;
                    }
                    tracing::debug!(
                        transmitter = name,
                        verdict = verdict.as_str(),
                        "ground_tx_liveness"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `wfb_tx` stats line (wfb-ng 24.08 `tx.cpp` format string).
    const LINE: &str = "1750000000000\tPKT\t0:12:1480:12:1608:0:0";

    #[test]
    fn a_real_pkt_line_yields_the_ingress_and_injection_bytes() {
        let pkt = parse_tx_pkt(LINE).expect("the shipped format must parse");
        assert_eq!(
            pkt,
            TxPkt {
                bytes_in: 1480,
                bytes_injected: 1608,
                packets_dropped: 0,
            }
        );
    }

    #[test]
    fn non_pkt_lines_are_ignored_rather_than_treated_as_a_fault() {
        // `TX_ANT` shares the stream, and upstream may add lines. Reading any of
        // them as a fault would restart the node that carries command authority
        // on a cosmetic format change.
        assert!(parse_tx_pkt("1750000000000\tTX_ANT\t1\t12:0:100:200:300").is_none());
        assert!(parse_tx_pkt("").is_none());
        assert!(parse_tx_pkt("garbage").is_none());
        assert!(parse_tx_pkt("1750000000000\tPKT\t0:12").is_none());
        assert!(parse_tx_pkt("1750000000000\tPKT\t0:12:x:12:1608:0:0").is_none());
    }

    fn pkt(bytes_in: u64, bytes_injected: u64) -> TxPkt {
        TxPkt {
            bytes_in,
            bytes_injected,
            packets_dropped: 0,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_transmitter_injecting_what_it_is_given_is_healthy() {
        let t0 = Instant::now();
        let mut w = TxLivenessWindow::new(t0, Duration::from_secs(30));
        let mut first = None;
        let mut ticks_before_verdict = 0u64;
        for _ in 1..=31u64 {
            tokio::time::advance(Duration::from_secs(1)).await;
            w.observe(pkt(1480, 1608), Instant::now());
            match w.evaluate(Instant::now()) {
                None => ticks_before_verdict += 1,
                Some(v) => {
                    first = Some(v);
                    break;
                }
            }
        }
        assert_eq!(first, Some(TxVerdict::Healthy));
        // And it withheld judgement until the window had actually filled, so a
        // fast poll cadence can never restart a transmitter on a partial view.
        assert_eq!(ticks_before_verdict, 29);
    }

    #[tokio::test(start_paused = true)]
    async fn a_transmitter_with_nothing_to_send_is_idle_not_stalled() {
        // The exemption that keeps a working-but-quiet transmitter alive. Before
        // it, a 30 s lull would have killpg'd the uplink and restarted the whole
        // receive generation for no fault at all.
        let mut w = TxLivenessWindow::new(Instant::now(), Duration::from_secs(30));
        for _ in 0..40 {
            tokio::time::advance(Duration::from_secs(1)).await;
            w.observe(pkt(0, 0), Instant::now());
            if let Some(v) = w.evaluate(Instant::now()) {
                assert_eq!(v, TxVerdict::Idle);
                assert!(!v.is_stall());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ingress_with_no_injection_for_the_window_is_a_stall() {
        let mut w = TxLivenessWindow::new(Instant::now(), Duration::from_secs(30));
        let mut fired = None;
        for _ in 0..40 {
            tokio::time::advance(Duration::from_secs(1)).await;
            w.observe(pkt(1480, 0), Instant::now());
            if let Some(v) = w.evaluate(Instant::now()) {
                fired = Some(v);
                break;
            }
        }
        assert_eq!(fired, Some(TxVerdict::StalledNotInjecting));
    }

    #[tokio::test(start_paused = true)]
    async fn no_stats_line_for_the_window_is_a_stall_even_with_no_traffic() {
        // `wfb_tx` prints every second regardless of traffic, so silence on the
        // stream means the main loop stopped — the failure that used to be
        // completely invisible because nothing read the stream at all.
        let mut w = TxLivenessWindow::new(Instant::now(), Duration::from_secs(30));
        tokio::time::advance(Duration::from_secs(31)).await;
        assert_eq!(
            w.evaluate(Instant::now()),
            Some(TxVerdict::StalledSilent),
            "line silence must fire without needing any ingress"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_watcher_fires_on_a_flat_injection_counter() {
        // End to end over the real reader: a transmitter that keeps printing and
        // keeps being fed but injects nothing must resolve the watcher, which is
        // what ends the generation and respawns the chain.
        let (mut writer, reader) = tokio::io::duplex(4096);
        let feeder = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for _ in 0..60 {
                // Ingress advancing, injection flat: the dead-radio shape.
                let _ = writer
                    .write_all(b"1750000000000\tPKT\t0:12:1480:0:0:0:0\n")
                    .await;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let verdict = tokio::time::timeout(
            Duration::from_secs(300),
            watch_tx_liveness(
                "aux_tx",
                reader,
                Duration::from_secs(5),
                Duration::from_secs(30),
            ),
        )
        .await
        .expect("the watcher must resolve, not hang");
        assert_eq!(verdict, TxVerdict::StalledNotInjecting);
        feeder.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn the_watcher_fires_when_the_transmitter_stops_printing() {
        // The process died or wedged: the pipe closes (or simply goes quiet) and
        // the watcher must conclude rather than park on the stream forever.
        let (writer, reader) = tokio::io::duplex(4096);
        drop(writer);
        let verdict = tokio::time::timeout(
            Duration::from_secs(300),
            watch_tx_liveness(
                "tx_control",
                reader,
                Duration::from_secs(5),
                Duration::from_secs(30),
            ),
        )
        .await
        .expect("the watcher must resolve on a closed stream");
        assert_eq!(verdict, TxVerdict::StalledSilent);
    }

    #[tokio::test(start_paused = true)]
    async fn the_watcher_does_not_fire_on_a_healthy_transmitter() {
        // The regression that matters most: a false fire restarts the uplink, so
        // a healthy stream must be left alone across several windows.
        let (mut writer, reader) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            loop {
                let _ = writer.write_all(LINE.as_bytes()).await;
                let _ = writer.write_all(b"\n").await;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(120),
            watch_tx_liveness(
                "tx_control",
                reader,
                Duration::from_secs(5),
                Duration::from_secs(30),
            ),
        )
        .await;
        assert!(
            outcome.is_err(),
            "a healthy transmitter must never trigger a restart, got {outcome:?}"
        );
    }
}
