//! The state-stream tap.
//!
//! Connects to the vehicle-state socket, reads the newline-terminated JSON
//! snapshot stream, and turns each snapshot into durable rows on the same ingest
//! channel the socket producers and the hardware collector feed:
//!
//! - The scalar telemetry of interest (attitude, altitude, speed, battery, GPS,
//!   link) becomes one [`TelemetryFrame`] per dotted metric key, so the full
//!   sample cadence is recorded rather than the one-sample-per-heartbeat the
//!   ground side otherwise sees.
//! - An arm/disarm transition emits an [`EventFrame`] carrying `reason=arm` /
//!   `reason=disarm` in its detail, which the writer reads to open and close the
//!   flight session. A mode change emits a `mode.change` event.
//!
//! Connection management and snapshot processing are split so the parsing path
//! is testable without a live socket: [`process_stream`] consumes any async byte
//! source (a fixture file, a socket half, an in-memory pipe) and a test feeds it
//! synthetic snapshots from a tempdir. [`run_state_tap`] wraps it with the
//! connect-then-reconnect-with-backoff loop the daemon spawns.
//!
//! The socket is absent on a host with no agent, and on an idle or unpaired
//! agent before the state hub comes up. That is normal, not an error: the tap
//! logs the absence at debug level and retries on a backoff. It only ever reads;
//! the state wire model stays frozen.

use std::path::Path;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use ados_protocol::logd::{EventFrame, IngestFrame, Level, TelemetryFrame};
use ados_protocol::state::STATE_V2_MAX_FRAME;

use super::backoff::ReconnectBackoff;
use super::{Shutdown, SOURCE_STATE};
use crate::writer::now_us;

/// Cap on one newline-delimited snapshot line, matching the state contract's
/// frame cap. A peer that never sends a newline cannot grow the read buffer
/// without bound past this.
const MAX_LINE_BYTES: usize = STATE_V2_MAX_FRAME;

/// One numeric metric to lift out of a state snapshot: the dotted JSON path to
/// the value and the dotted metric key it is stored under.
struct MetricMap {
    /// Dotted path into the snapshot (`battery.voltage`, `link.rssi_dbm`, ...).
    path: &'static str,
    /// The metric key the value is recorded under.
    key: &'static str,
}

/// The numeric telemetry lifted from each snapshot. Only the paths present in a
/// given snapshot are emitted; an absent field produces no row. The set covers
/// the fields the state contract documents plus common extensions (altitude and
/// speed) so they are captured durably when the autopilot reports them.
const METRICS: &[MetricMap] = &[
    MetricMap {
        path: "attitude.roll",
        key: "attitude.roll",
    },
    MetricMap {
        path: "attitude.pitch",
        key: "attitude.pitch",
    },
    MetricMap {
        path: "attitude.yaw",
        key: "attitude.yaw",
    },
    MetricMap {
        path: "altitude.agl",
        key: "altitude.agl_m",
    },
    MetricMap {
        path: "altitude.msl",
        key: "altitude.msl_m",
    },
    MetricMap {
        path: "groundspeed",
        key: "groundspeed.ms",
    },
    MetricMap {
        path: "airspeed",
        key: "airspeed.ms",
    },
    MetricMap {
        path: "battery.voltage",
        key: "battery.voltage.v",
    },
    MetricMap {
        path: "battery.current",
        key: "battery.current.a",
    },
    MetricMap {
        path: "battery.remaining",
        key: "battery.remaining.pct",
    },
    MetricMap {
        path: "gps.fix",
        key: "gps.fix",
    },
    MetricMap {
        path: "gps.sats",
        key: "gps.sats",
    },
    MetricMap {
        path: "gps.lat",
        key: "gps.lat",
    },
    MetricMap {
        path: "gps.lon",
        key: "gps.lon",
    },
    MetricMap {
        path: "link.rssi_dbm",
        key: "link.rssi.dbm",
    },
    MetricMap {
        path: "link.snr_db",
        key: "link.snr.db",
    },
    MetricMap {
        path: "link.valid_rx_packets_per_s",
        key: "link.valid_rx_pkt_per_s",
    },
];

/// What the previous snapshot carried, so successive snapshots can be compared
/// for arm/disarm and mode transitions. `None` before the first snapshot.
#[derive(Debug, Default)]
struct PrevState {
    armed: Option<bool>,
    mode: Option<String>,
}

/// Run the state tap until `shutdown` resolves.
///
/// Connects to `socket_path`, processes the snapshot stream, and on any
/// disconnect or an absent socket reconnects with capped backoff. A missing
/// socket is expected (no agent on a host, an idle agent before the state hub is
/// up) and is logged at debug level, never as an error.
pub async fn run_state_tap(
    socket_path: impl AsRef<Path>,
    tx: mpsc::Sender<IngestFrame>,
    mut shutdown: Shutdown,
) {
    let socket_path = socket_path.as_ref();
    let mut backoff = ReconnectBackoff::default();
    tracing::info!(path = %socket_path.display(), "state tap started");
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => {
                tracing::info!("state tap stopping");
                return;
            }
            connected = connect(socket_path) => {
                match connected {
                    Some(stream) => {
                        backoff.reset();
                        let reader = BufReader::new(stream);
                        let mut prev = PrevState::default();
                        process_stream(reader, &tx, &mut prev, &mut shutdown).await;
                        // The stream ended (EOF or a read error). Loop to
                        // reconnect; the writer side staying open is checked in
                        // process_stream, which returns on a closed channel.
                        if tx.is_closed() {
                            return;
                        }
                    }
                    None => {
                        // Absent socket: normal on a host or an idle agent.
                        let wait = backoff.next_delay();
                        tokio::select! {
                            _ = shutdown.recv() => {
                                tracing::info!("state tap stopping");
                                return;
                            }
                            _ = tokio::time::sleep(wait) => {}
                        }
                    }
                }
            }
        }
    }
}

/// Try once to connect to the state socket. Returns `None` (logged at debug)
/// when the socket is absent or refuses, so the caller backs off and retries.
async fn connect(path: &Path) -> Option<UnixStream> {
    match UnixStream::connect(path).await {
        Ok(s) => {
            tracing::debug!(path = %path.display(), "state socket connected");
            Some(s)
        }
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "state socket absent; will retry");
            None
        }
    }
}

/// Consume a newline-delimited JSON snapshot stream to EOF (or until the writer
/// channel closes or shutdown fires), emitting telemetry and transition frames.
///
/// This is the injectable seam: `reader` is any async byte source, so a test
/// feeds synthetic snapshot lines from a fixture file without a live socket.
async fn process_stream<R>(
    mut reader: R,
    tx: &mpsc::Sender<IngestFrame>,
    prev: &mut PrevState,
    shutdown: &mut Shutdown,
) where
    R: AsyncBufRead + Unpin,
{
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        let read = tokio::select! {
            biased;
            _ = shutdown.recv() => return,
            r = read_capped_line(&mut reader, &mut line) => r,
        };
        match read {
            Ok(0) => return, // clean EOF at a line boundary
            Ok(_) => {
                // The state stream is UTF-8 JSON; a non-UTF-8 line is malformed
                // and skipped like any other bad snapshot.
                let text = match std::str::from_utf8(&line) {
                    Ok(t) => t.trim_end_matches(['\n', '\r']),
                    Err(_) => {
                        tracing::debug!("skipping a non-utf8 state line");
                        continue;
                    }
                };
                if text.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(text) {
                    Ok(snapshot) => {
                        if emit_snapshot(&snapshot, prev, tx).await.is_err() {
                            // The writer side is gone; stop.
                            return;
                        }
                    }
                    Err(e) => {
                        // A malformed line is skipped, never fatal: a single bad
                        // snapshot must not end the tap.
                        tracing::debug!(error = %e, "skipping a malformed state snapshot");
                    }
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "state stream read error");
                return;
            }
        }
    }
}

/// Read one newline-delimited line into `out`, capped at [`MAX_LINE_BYTES`] so a
/// peer that never sends a newline cannot grow the buffer without bound. Returns
/// the number of bytes accumulated (zero at a clean EOF). A line that reaches the
/// cap without a terminating newline is rejected so the caller reconnects rather
/// than buffer a runaway producer. Bytes are accumulated raw and decoded by the
/// caller so a multibyte UTF-8 sequence is never split.
async fn read_capped_line<R>(reader: &mut R, out: &mut Vec<u8>) -> std::io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Ok(out.len()); // EOF
        }
        out.push(byte[0]);
        if byte[0] == b'\n' {
            return Ok(out.len());
        }
        if out.len() >= MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "state line exceeded the maximum length without a newline",
            ));
        }
    }
}

/// Emit every frame derived from one snapshot: arm/disarm and mode-change events
/// (emitted before the metrics so the writer opens the flight session ahead of
/// the rows that belong to it), then one telemetry frame per present metric.
/// Returns `Err(())` when the writer channel has closed.
async fn emit_snapshot(
    snapshot: &Value,
    prev: &mut PrevState,
    tx: &mpsc::Sender<IngestFrame>,
) -> Result<(), ()> {
    let ts = now_us();

    // Arm/disarm: a transition drives the flight session through the writer.
    let armed = snapshot.get("armed").and_then(Value::as_bool);
    if let Some(now_armed) = armed {
        let was = prev.armed;
        if was != Some(now_armed) {
            let reason = if now_armed { "arm" } else { "disarm" };
            let mut ev = EventFrame::new(ts, "state.arm", SOURCE_STATE, Level::Info);
            ev.detail
                .insert("reason".to_string(), rmpv::Value::from(reason));
            send(tx, IngestFrame::Event(ev)).await?;
        }
        prev.armed = Some(now_armed);
    }

    // Mode change: record the from/to pair.
    let mode = snapshot
        .get("mode")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(now_mode) = mode {
        if prev.mode.as_deref() != Some(now_mode.as_str()) {
            let mut ev = EventFrame::new(ts, "mode.change", SOURCE_STATE, Level::Info);
            if let Some(from) = &prev.mode {
                ev.detail
                    .insert("from".to_string(), rmpv::Value::from(from.clone()));
            }
            ev.detail
                .insert("to".to_string(), rmpv::Value::from(now_mode.clone()));
            send(tx, IngestFrame::Event(ev)).await?;
            prev.mode = Some(now_mode);
        }
    }

    // Scalar telemetry: one metric per present numeric field.
    for m in METRICS {
        if let Some(value) = lookup_number(snapshot, m.path) {
            let mut frame = TelemetryFrame::new(ts, m.key, value);
            frame
                .tags
                .insert("sample".to_string(), rmpv::Value::from("state_tick"));
            send(tx, IngestFrame::Telemetry(frame)).await?;
        }
    }
    Ok(())
}

/// Send one frame, mapping a closed channel to `Err(())`. A bounded await is
/// fine here: the state cadence is ~10 Hz and the writer drains far faster, so
/// the send rarely waits, and the tap is its own task that must not stall the
/// flight stack only because it never touches it.
async fn send(tx: &mpsc::Sender<IngestFrame>, frame: IngestFrame) -> Result<(), ()> {
    tx.send(frame).await.map_err(|_| ())
}

/// Resolve a dotted path (`battery.voltage`) to an `f64`, accepting JSON numbers
/// and booleans (booleans map to 1.0/0.0 so an `armed`-style flag could be a
/// metric if ever mapped). Returns `None` for an absent path or a non-numeric
/// leaf, so only present numeric fields are emitted.
fn lookup_number(root: &Value, path: &str) -> Option<f64> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    match cur {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    /// Drive `process_stream` against an in-memory snapshot stream and collect
    /// every frame it emits. A never-firing shutdown lets the stream run to EOF.
    async fn run_against(lines: &str) -> Vec<IngestFrame> {
        let (tx, mut rx) = mpsc::channel::<IngestFrame>(256);
        let reader = BufReader::new(Cursor::new(lines.to_string().into_bytes()));
        let mut prev = PrevState::default();
        let mut shutdown = Shutdown::never();
        process_stream(reader, &tx, &mut prev, &mut shutdown).await;
        drop(tx);
        let mut out = Vec::new();
        while let Some(f) = rx.recv().await {
            out.push(f);
        }
        out
    }

    fn metric<'a>(frames: &'a [IngestFrame], key: &str) -> Option<&'a TelemetryFrame> {
        frames.iter().find_map(|f| match f {
            IngestFrame::Telemetry(t) if t.metric == key => Some(t),
            _ => None,
        })
    }

    fn events<'a>(frames: &'a [IngestFrame], kind: &str) -> Vec<&'a EventFrame> {
        frames
            .iter()
            .filter_map(|f| match f {
                IngestFrame::Event(e) if e.kind == kind => Some(e),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn snapshot_emits_the_documented_scalar_metrics() {
        let line = serde_json::to_string(&json!({
            "armed": false,
            "mode": "STABILIZE",
            "battery": {"voltage": 16.4, "current": 12.1, "remaining": 87},
            "gps": {"fix": 3, "sats": 14, "lat": 12.9716, "lon": 77.5946},
            "attitude": {"roll": 0.01, "pitch": -0.02, "yaw": 1.57},
            "link": {"rssi_dbm": -48, "snr_db": 22, "valid_rx_packets_per_s": 630}
        }))
        .unwrap();
        let frames = run_against(&format!("{line}\n")).await;

        // Every mapped, present field becomes a metric with the dotted key.
        assert_eq!(
            metric(&frames, "battery.voltage.v").map(|m| m.value),
            Some(16.4)
        );
        assert_eq!(
            metric(&frames, "battery.current.a").map(|m| m.value),
            Some(12.1)
        );
        assert_eq!(
            metric(&frames, "battery.remaining.pct").map(|m| m.value),
            Some(87.0)
        );
        assert_eq!(metric(&frames, "gps.fix").map(|m| m.value), Some(3.0));
        assert_eq!(metric(&frames, "gps.sats").map(|m| m.value), Some(14.0));
        assert_eq!(metric(&frames, "attitude.yaw").map(|m| m.value), Some(1.57));
        assert_eq!(
            metric(&frames, "link.rssi.dbm").map(|m| m.value),
            Some(-48.0)
        );
        assert_eq!(metric(&frames, "link.snr.db").map(|m| m.value), Some(22.0));
        assert_eq!(
            metric(&frames, "link.valid_rx_pkt_per_s").map(|m| m.value),
            Some(630.0)
        );
        // The sample tag is set so the read edge can tell state-tap rows apart.
        assert_eq!(
            metric(&frames, "battery.voltage.v")
                .and_then(|m| m.tags.get("sample"))
                .and_then(|v| v.as_str()),
            Some("state_tick")
        );

        // The first snapshot reports a disarmed flag, which is a transition from
        // the unknown initial state, so a disarm event is emitted once.
        let arm = events(&frames, "state.arm");
        assert_eq!(arm.len(), 1);
        assert_eq!(
            arm[0].detail.get("reason").and_then(|v| v.as_str()),
            Some("disarm")
        );
        // The first mode is a transition from unknown, so one mode.change.
        let mode = events(&frames, "mode.change");
        assert_eq!(mode.len(), 1);
        assert_eq!(
            mode[0].detail.get("to").and_then(|v| v.as_str()),
            Some("STABILIZE")
        );
        assert!(!mode[0].detail.contains_key("from"));
    }

    #[tokio::test]
    async fn arm_then_disarm_emits_exactly_two_transition_events_with_writer_reasons() {
        let armed = serde_json::to_string(&json!({"armed": true, "mode": "GUIDED"})).unwrap();
        let still_armed = serde_json::to_string(&json!({"armed": true, "mode": "GUIDED"})).unwrap();
        let disarmed = serde_json::to_string(&json!({"armed": false, "mode": "GUIDED"})).unwrap();
        let stream = format!("{armed}\n{still_armed}\n{disarmed}\n");
        let frames = run_against(&stream).await;

        // The unchanged middle snapshot emits no transition: one arm, one disarm.
        let arm = events(&frames, "state.arm");
        assert_eq!(arm.len(), 2, "one arm, one disarm");
        assert_eq!(
            arm[0].detail.get("reason").and_then(|v| v.as_str()),
            Some("arm")
        );
        assert_eq!(
            arm[1].detail.get("reason").and_then(|v| v.as_str()),
            Some("disarm")
        );
        // The mode never changed after the first snapshot, so exactly one
        // mode.change (the unknown -> GUIDED transition).
        assert_eq!(events(&frames, "mode.change").len(), 1);
    }

    #[tokio::test]
    async fn mode_change_records_the_from_and_to_pair() {
        let a = serde_json::to_string(&json!({"armed": false, "mode": "STABILIZE"})).unwrap();
        let b = serde_json::to_string(&json!({"armed": false, "mode": "ALT_HOLD"})).unwrap();
        let frames = run_against(&format!("{a}\n{b}\n")).await;
        let mode = events(&frames, "mode.change");
        assert_eq!(mode.len(), 2);
        // Second mode change carries both ends.
        assert_eq!(
            mode[1].detail.get("from").and_then(|v| v.as_str()),
            Some("STABILIZE")
        );
        assert_eq!(
            mode[1].detail.get("to").and_then(|v| v.as_str()),
            Some("ALT_HOLD")
        );
    }

    #[tokio::test]
    async fn absent_fields_emit_no_metric_and_malformed_lines_are_skipped() {
        // A snapshot with only battery voltage present plus a malformed line.
        let ok = serde_json::to_string(&json!({"battery": {"voltage": 15.0}})).unwrap();
        let stream = format!("{ok}\n{{ not json }}\n\n");
        let frames = run_against(&stream).await;
        assert_eq!(
            metric(&frames, "battery.voltage.v").map(|m| m.value),
            Some(15.0)
        );
        // No GPS metrics from a snapshot that has no gps block.
        assert!(metric(&frames, "gps.fix").is_none());
        // The malformed and empty lines produced no events.
        assert!(events(&frames, "state.arm").is_empty());
    }

    #[tokio::test]
    async fn process_stream_stops_when_the_writer_channel_closes() {
        // Drop the receiver before processing: the first send fails and the
        // stream processor returns rather than spinning.
        let (tx, rx) = mpsc::channel::<IngestFrame>(1);
        drop(rx);
        let line = serde_json::to_string(&json!({"armed": true})).unwrap();
        let reader = BufReader::new(Cursor::new(format!("{line}\n").into_bytes()));
        let mut prev = PrevState::default();
        let mut shutdown = Shutdown::never();
        // Returns promptly; if it looped forever the test would hang.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            process_stream(reader, &tx, &mut prev, &mut shutdown),
        )
        .await
        .expect("process_stream returns when the channel is closed");
    }

    #[tokio::test]
    async fn lookup_number_walks_dotted_paths_and_coerces_bool() {
        let v = json!({"a": {"b": 3}, "flag": true, "text": "x"});
        assert_eq!(lookup_number(&v, "a.b"), Some(3.0));
        assert_eq!(lookup_number(&v, "flag"), Some(1.0));
        assert_eq!(lookup_number(&v, "text"), None);
        assert_eq!(lookup_number(&v, "a.missing"), None);
    }

    #[tokio::test]
    async fn run_state_tap_against_an_absent_socket_retries_then_stops_on_shutdown() {
        // No socket exists at this path; the tap must back off and retry without
        // crashing, then stop promptly when shutdown fires.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sock");
        let (tx, _rx) = mpsc::channel::<IngestFrame>(8);
        let (stop, shutdown) = Shutdown::pair();
        let handle = tokio::spawn(run_state_tap(path, tx, shutdown));
        // Let it attempt a connect and enter the backoff wait.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        stop.fire();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("state tap stops within the bound")
            .expect("state tap task did not panic");
    }

    #[tokio::test]
    async fn run_state_tap_reads_a_live_socket_then_reconnects_on_eof() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let (tx, mut rx) = mpsc::channel::<IngestFrame>(64);
        let (stop, shutdown) = Shutdown::pair();
        let handle = tokio::spawn(run_state_tap(path.clone(), tx, shutdown));

        // First connection: send one snapshot, then close to force a reconnect.
        let (mut a, _addr) = listener.accept().await.unwrap();
        let line =
            serde_json::to_string(&json!({"armed": true, "battery": {"voltage": 16.0}})).unwrap();
        a.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        a.flush().await.unwrap();
        drop(a); // EOF -> the tap reconnects

        // The arm event and the voltage metric arrived from the first connection.
        let mut saw_arm = false;
        let mut saw_voltage = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Some(IngestFrame::Event(e))) if e.kind == "state.arm" => saw_arm = true,
                Ok(Some(IngestFrame::Telemetry(t))) if t.metric == "battery.voltage.v" => {
                    saw_voltage = true
                }
                Ok(Some(_)) => continue,
                _ => {}
            }
            if saw_arm && saw_voltage {
                break;
            }
        }
        assert!(saw_arm, "arm event from the first connection");
        assert!(saw_voltage, "voltage metric from the first connection");

        // The tap reconnects: a second accept succeeds within the bound.
        let second = tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
            .await
            .expect("the tap reconnects after EOF");
        assert!(second.is_ok());

        stop.fire();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }
}
