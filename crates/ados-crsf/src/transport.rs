//! Serial transport: the 420 kbaud port, the fixed-cadence RC transmitter,
//! and the sync-hunting telemetry receiver.
//!
//! Both tasks are generic over the async byte stream, so the whole framed
//! path is provable end to end over an in-memory duplex with zero hardware;
//! only [`open_serial`] touches a real device node.
//!
//! Half-duplex note: the single-wire bus turnaround the protocol defines is a
//! microsecond-scale budget a userspace host cannot hold. This transport
//! therefore targets a full-duplex USB-serial RC module bridge, which owns
//! the bus direction itself — the host only holds the frame cadence.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

use crate::channels::build_rc_frame;
use crate::frame::{Parser, TYPE_LINK_STATISTICS};
use crate::sources::SourceMerge;
use crate::telemetry::LinkStatistics;

/// The CRSF UART line rate: 420 kbaud, 8 data bits, no parity, 1 stop bit
/// (the serial defaults for this builder).
pub const CRSF_BAUD: u32 = 420_000;

/// Open the serial device at the given baud. `None` when the open fails —
/// the caller owns the retry/backoff policy.
pub fn open_serial(port: &str, baud: u32) -> Option<SerialStream> {
    tokio_serial::new(port, baud).open_native_async().ok()
}

/// Shared wire counters both tasks bump and the heartbeat reads. Relaxed
/// ordering everywhere: these are statistics, not synchronization.
#[derive(Debug, Default)]
pub struct WireCounters {
    /// RC frames written to the port.
    pub tx_frames: AtomicU64,
    /// Valid frames lifted off the inbound stream.
    pub rx_frames: AtomicU64,
    /// Inbound frames rejected for a CRC mismatch.
    pub crc_errors: AtomicU64,
    /// Inbound bytes skipped hunting for a frame boundary.
    pub resync_bytes: AtomicU64,
}

/// Latest telemetry lifted off the inbound stream, shared with the heartbeat.
#[derive(Debug, Default)]
pub struct TelemetryState {
    /// The last valid link-statistics frame and when it arrived.
    pub last_link_stats: Option<(LinkStatistics, Instant)>,
}

impl TelemetryState {
    /// Age of the last link-statistics frame, `None` when never seen.
    pub fn stats_age(&self, now: Instant) -> Option<Duration> {
        self.last_link_stats
            .as_ref()
            .map(|(_, at)| now.duration_since(*at))
    }
}

/// Why the transmit task returned.
#[derive(Debug, PartialEq, Eq)]
pub enum TxExit {
    /// A write or flush failed — the port is gone; the caller respawns.
    WriteError,
    /// The cancel notify fired.
    Cancelled,
}

/// Why the receive task returned.
#[derive(Debug, PartialEq, Eq)]
pub enum RxExit {
    /// The stream reached EOF or errored — the port is gone.
    StreamClosed,
    /// The cancel notify fired.
    Cancelled,
}

/// Transmit one RC channels frame per tick at `rate_hz` until cancelled or
/// the writer dies. Each tick reads the source merge (authority + TTL applied
/// per tick), so an injection lands on the very next frame and a silent
/// injector decays to neutral on the very next frame after its TTL.
pub async fn run_tx<W: AsyncWrite + Unpin>(
    mut writer: W,
    merge: Arc<Mutex<SourceMerge>>,
    rate_hz: u16,
    counters: Arc<WireCounters>,
    cancel: Arc<Notify>,
) -> TxExit {
    let period = Duration::from_secs_f64(1.0 / f64::from(rate_hz.max(1)));
    let mut ticker = tokio::time::interval(period);
    // A missed tick (scheduler hiccup) must not burst-transmit to catch up;
    // the RC link wants a steady cadence.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => return TxExit::Cancelled,
            _ = ticker.tick() => {}
        }
        let (values, _source) = merge.lock().await.current(Instant::now());
        // 16 in-range channel values always build; a failure here would be a
        // codec bug, and silently skipping the tick would hide it.
        let frame = match build_rc_frame(&values) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(error = %e, "rc frame build failed");
                return TxExit::WriteError;
            }
        };
        if writer.write_all(&frame).await.is_err() || writer.flush().await.is_err() {
            return TxExit::WriteError;
        }
        counters.tx_frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// Read the inbound stream through the sync-hunting parser until cancelled or
/// the stream dies, folding parser counters into the shared ones and posting
/// link-statistics frames onto the telemetry state. Garbage never panics or
/// exits the loop — only EOF/error or a cancel does.
pub async fn run_rx<R: AsyncRead + Unpin>(
    mut reader: R,
    telemetry: Arc<Mutex<TelemetryState>>,
    counters: Arc<WireCounters>,
    cancel: Arc<Notify>,
) -> RxExit {
    let mut parser = Parser::new();
    let mut folded_crc: u64 = 0;
    let mut folded_resync: u64 = 0;
    let mut buf = [0u8; 256];
    loop {
        let n = tokio::select! {
            biased;
            _ = cancel.notified() => return RxExit::Cancelled,
            read = reader.read(&mut buf) => match read {
                Ok(0) | Err(_) => return RxExit::StreamClosed,
                Ok(n) => n,
            },
        };
        let frames = parser.push(&buf[..n]);
        // Fold the parser's cumulative counters into the shared atomics as
        // deltas, so the heartbeat sees monotonic totals.
        let crc_delta = parser.crc_errors - folded_crc;
        folded_crc = parser.crc_errors;
        if crc_delta > 0 {
            counters.crc_errors.fetch_add(crc_delta, Ordering::Relaxed);
        }
        let resync_delta = parser.resync_bytes - folded_resync;
        folded_resync = parser.resync_bytes;
        if resync_delta > 0 {
            counters
                .resync_bytes
                .fetch_add(resync_delta, Ordering::Relaxed);
        }
        for frame in frames {
            counters.rx_frames.fetch_add(1, Ordering::Relaxed);
            if frame.frame_type == TYPE_LINK_STATISTICS {
                match LinkStatistics::decode(&frame.payload) {
                    Ok(stats) => {
                        telemetry.lock().await.last_link_stats = Some((stats, Instant::now()));
                    }
                    // Unreachable through the size-validating parser; kept
                    // defensive because a decode failure must never kill RX.
                    Err(e) => tracing::warn!(error = %e, "link statistics decode failed"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{unpack_channels, CHANNEL_COUNT, PACKED_SIZE};
    use crate::frame::{build_frame, ADDR_FLIGHT_CONTROLLER};
    use crate::sources::{ChannelSourceMode, MAX_INJECT_TTL};
    use crate::telemetry::{build_telemetry_frame, Telemetry};

    fn inject_merge() -> Arc<Mutex<SourceMerge>> {
        Arc::new(Mutex::new(SourceMerge::new(ChannelSourceMode::Inject)))
    }

    fn sample_stats() -> LinkStatistics {
        LinkStatistics {
            uplink_rssi_ant1: -48,
            uplink_rssi_ant2: -52,
            uplink_lq: 100,
            uplink_snr: 10,
            active_antenna: 0,
            rf_mode: 5,
            uplink_tx_power: 25,
            downlink_rssi: -50,
            downlink_lq: 99,
            downlink_snr: 9,
        }
    }

    /// End-to-end loopback: the TX task's framed cadence feeds the RX parser
    /// over an in-memory duplex; every transmitted frame parses back with the
    /// injected channel values.
    #[tokio::test]
    async fn loopback_tx_cadence_parses_end_to_end() {
        let (tx_side, rx_side) = tokio::io::duplex(4096);
        let merge = inject_merge();
        let mut injected = [992u16; CHANNEL_COUNT];
        injected[2] = 172;
        injected[15] = 1811;
        merge
            .lock()
            .await
            .inject_all(injected, MAX_INJECT_TTL, Instant::now(), None)
            .unwrap();
        let counters = Arc::new(WireCounters::default());
        let telemetry = Arc::new(Mutex::new(TelemetryState::default()));
        let cancel = Arc::new(Notify::new());

        let tx = tokio::spawn(run_tx(
            tx_side,
            merge.clone(),
            200, // fast cadence keeps the test quick
            counters.clone(),
            cancel.clone(),
        ));

        // Read the raw bytes off the far end through the parser directly, so
        // the test also proves frame alignment across arbitrary read chunks.
        let mut parser = Parser::new();
        let mut reader = rx_side;
        let mut frames = Vec::new();
        let deadline = tokio::time::timeout(Duration::from_secs(5), async {
            let mut buf = [0u8; 64];
            while frames.len() < 5 {
                let n = reader.read(&mut buf).await.unwrap();
                assert!(n > 0, "tx side closed unexpectedly");
                frames.extend(parser.push(&buf[..n]));
            }
        })
        .await;
        deadline.expect("five frames within the deadline");
        cancel.notify_waiters();
        assert_eq!(tx.await.unwrap(), TxExit::Cancelled);

        assert!(parser.crc_errors == 0, "clean loopback has no crc errors");
        assert!(counters.tx_frames.load(Ordering::Relaxed) >= 5);
        for frame in &frames {
            assert_eq!(frame.frame_type, crate::frame::TYPE_RC_CHANNELS_PACKED);
            let payload: [u8; PACKED_SIZE] = frame.payload.clone().try_into().unwrap();
            assert_eq!(unpack_channels(&payload), injected);
        }
        // The telemetry state stays empty — RC frames are not telemetry.
        assert!(telemetry.lock().await.last_link_stats.is_none());
    }

    /// The RX task lifts telemetry off a dirty stream: garbage, a corrupted
    /// frame, and valid link statistics interleaved. The state updates, the
    /// counters count, and the task survives it all.
    #[tokio::test]
    async fn rx_task_decodes_telemetry_through_garbage() {
        let (mut writer, rx_side) = tokio::io::duplex(4096);
        let counters = Arc::new(WireCounters::default());
        let telemetry = Arc::new(Mutex::new(TelemetryState::default()));
        let cancel = Arc::new(Notify::new());
        let rx = tokio::spawn(run_rx(
            rx_side,
            telemetry.clone(),
            counters.clone(),
            cancel.clone(),
        ));

        let stats = sample_stats();
        let good = build_telemetry_frame(ADDR_FLIGHT_CONTROLLER, &Telemetry::LinkStatistics(stats))
            .unwrap();
        let mut corrupt = good.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        let other = build_frame(ADDR_FLIGHT_CONTROLLER, 0x7F, &[1, 2, 3]).unwrap();

        writer.write_all(&[0x00, 0x55, 0xAA]).await.unwrap(); // garbage
        writer.write_all(&corrupt).await.unwrap();
        writer.write_all(&other).await.unwrap();
        writer.write_all(&good).await.unwrap();
        writer.flush().await.unwrap();

        // Wait until the stats frame lands.
        let deadline = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if telemetry.lock().await.last_link_stats.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        deadline.expect("link statistics decoded within the deadline");

        let held = telemetry.lock().await.last_link_stats.unwrap().0;
        assert_eq!(held, stats);
        assert!(counters.crc_errors.load(Ordering::Relaxed) >= 1);
        assert!(counters.rx_frames.load(Ordering::Relaxed) >= 2);
        assert!(counters.resync_bytes.load(Ordering::Relaxed) >= 3);

        // EOF ends the task with the honest exit reason.
        drop(writer);
        let exit = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit, RxExit::StreamClosed);
    }

    /// A cancel wins over pending reads and pending ticks: both tasks return
    /// their cancelled exits promptly.
    #[tokio::test]
    async fn cancel_stops_both_tasks() {
        let (tx_side, rx_keep) = tokio::io::duplex(4096);
        let (_tx_keep, rx_side) = tokio::io::duplex(4096);
        let counters = Arc::new(WireCounters::default());
        let telemetry = Arc::new(Mutex::new(TelemetryState::default()));
        let cancel = Arc::new(Notify::new());
        let tx = tokio::spawn(run_tx(
            tx_side,
            inject_merge(),
            1, // slow: the cancel must not wait for a tick
            counters.clone(),
            cancel.clone(),
        ));
        let rx = tokio::spawn(run_rx(rx_side, telemetry, counters, cancel.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.notify_waiters();
        let (tx_exit, rx_exit) = tokio::time::timeout(Duration::from_secs(5), async {
            (tx.await.unwrap(), rx.await.unwrap())
        })
        .await
        .expect("both tasks stop promptly");
        assert_eq!(tx_exit, TxExit::Cancelled);
        assert_eq!(rx_exit, RxExit::Cancelled);
        drop(rx_keep);
    }

    /// A dead writer surfaces as the write-error exit so the respawn loop can
    /// tell "port gone" from "asked to stop".
    #[tokio::test]
    async fn tx_write_error_surfaces_when_the_peer_closes() {
        let (tx_side, rx_side) = tokio::io::duplex(64);
        drop(rx_side);
        let counters = Arc::new(WireCounters::default());
        let cancel = Arc::new(Notify::new());
        let exit = tokio::time::timeout(
            Duration::from_secs(5),
            run_tx(tx_side, inject_merge(), 100, counters, cancel),
        )
        .await
        .expect("exit promptly");
        assert_eq!(exit, TxExit::WriteError);
    }
}
