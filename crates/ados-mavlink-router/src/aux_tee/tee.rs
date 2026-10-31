//! The tee loop: shed, batch and send FC frames onto the aux lane.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use ados_protocol::aux_egress::{AuxEgress, AuxEgressError};
use ados_protocol::aux_mux::{AuxChannel, AUX_HEADER_LEN, AUX_MAX_PAYLOAD};
use ados_protocol::shutdown::Shutdown;
use tokio::sync::broadcast::error::RecvError;

use super::lane::report;
use super::{
    frame_priority, LaneGate, RateShaper, ShaperConfig, TeeCounters, TeeCountersSnapshot,
    BATCH_WINDOW, COUNTER_REPORT_INTERVAL,
};

/// Run the tee until cancelled or the frame source closes.
///
/// Takes the frame source rather than the FC connection so the whole loop is
/// exercisable against a plain broadcast channel and a fake radio, with no
/// flight controller and no radio hardware. A plain receiver converts into
/// the flat [`crate::routing::FrameFeed`]; the router passes the routed one.
pub async fn run(
    frames: impl Into<crate::routing::FrameFeed>,
    egress: AuxEgress,
    counters: Arc<TeeCounters>,
    shaper_config: ShaperConfig,
    cancel: Shutdown,
) {
    let mut frames = frames.into();
    let mut shaper = RateShaper::new(shaper_config);
    let mut gate = LaneGate::default();
    let mut last_report = TeeCountersSnapshot::default();
    // Frames accumulate here and leave as one datagram. `batched` is the frame
    // count in flight so the counters still report frames, not datagrams.
    let mut batch: Vec<u8> = Vec::with_capacity(AUX_MAX_PAYLOAD);
    let mut batched: u64 = 0;
    let mut batch_deadline: Option<Instant> = None;
    let mut report_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + COUNTER_REPORT_INTERVAL,
        COUNTER_REPORT_INTERVAL,
    );
    report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(
        budget_bytes_per_sec = shaper_config.budget_bytes_per_sec,
        burst_bytes = shaper_config.burst_bytes,
        "mavlink_aux_tee_started"
    );

    loop {
        // A partly-filled batch must not sit forever on a quiet link, so the
        // select waits on its deadline as well as on the next frame.
        let flush_at = batch_deadline.unwrap_or_else(|| Instant::now() + BATCH_WINDOW);
        let frame = tokio::select! {
            biased;
            _ = cancel.wait() => break,
            _ = report_tick.tick() => {
                last_report = report(&counters, last_report);
                continue;
            }
            _ = tokio::time::sleep_until(flush_at.into()), if batch_deadline.is_some() => {
                flush(&egress, &mut batch, &mut batched, &counters, &mut gate).await;
                batch_deadline = None;
                continue;
            }
            received = frames.recv() => match received {
                Ok(frame) => frame.bytes,
                // The runtime discarded frames this tee was too slow to take.
                // That is the shed working as designed: the fan-out producer was
                // never blocked. Count what was lost rather than assume none was.
                Err(RecvError::Lagged(skipped)) => {
                    counters.dropped_lagged.fetch_add(skipped, Ordering::Relaxed);
                    continue;
                }
                Err(RecvError::Closed) => break,
            },
        };

        // Reject before spending any budget or opening anything. A frame that
        // cannot be framed whole is dropped: truncating it would put a corrupt
        // frame on air that fails its checksum on the far side and reads as
        // radio damage rather than as our bug.
        if frame.len() > AUX_MAX_PAYLOAD {
            counters.dropped_oversize.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        if !egress.is_open().await {
            let now = Instant::now();
            if !gate.may_attempt(now) {
                counters.dropped_lane_down.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match egress.ensure_open().await {
                Ok(()) => gate.on_open(),
                Err(AuxEgressError::Disabled) => {
                    gate.on_disabled(Instant::now());
                    counters.dropped_lane_down.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(error) => {
                    counters.open_errors.fetch_add(1, Ordering::Relaxed);
                    gate.on_failure(Instant::now(), &error);
                    counters.dropped_lane_down.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
        }

        // Budget is charged per frame, so shedding stays per-frame and the
        // priority reserve keeps meaning what it did. Only the carrier changes.
        let cost = AUX_HEADER_LEN + frame.len();
        if !shaper.admit(Instant::now(), cost, frame_priority(&frame)) {
            counters.dropped_rate_limit.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Batch rather than send one datagram per frame. Flush first if this
        // frame would not fit, so a datagram never exceeds the lane's budget.
        if !batch.is_empty() && batch.len() + frame.len() > AUX_MAX_PAYLOAD {
            flush(&egress, &mut batch, &mut batched, &counters, &mut gate).await;
        }
        batch.extend_from_slice(&frame);
        batched += 1;
        if batch_deadline.is_none() {
            batch_deadline = Some(Instant::now() + BATCH_WINDOW);
        }
        // A full batch goes now; a partial one waits out the window above.
        if batch.len() >= AUX_MAX_PAYLOAD {
            flush(&egress, &mut batch, &mut batched, &counters, &mut gate).await;
            batch_deadline = None;
        }
    }

    // Whatever is still buffered when the loop ends has been admitted by the
    // shaper already, so it is owed a send attempt rather than a silent drop.
    flush(&egress, &mut batch, &mut batched, &counters, &mut gate).await;

    report(&counters, last_report);
    tracing::info!("mavlink_aux_tee_stopped");
}

/// Send the accumulated batch as one auxiliary-lane datagram.
///
/// Counters still tally FRAMES, not datagrams, so the numbers stay comparable
/// across the change and a reader is never misled into thinking traffic fell
/// when only the carrier got fuller.
async fn flush(
    egress: &AuxEgress,
    batch: &mut Vec<u8>,
    batched: &mut u64,
    counters: &Arc<TeeCounters>,
    gate: &mut LaneGate,
) {
    if batch.is_empty() {
        return;
    }
    let frames = *batched;
    let bytes = (AUX_HEADER_LEN + batch.len()) as u64;
    match egress.send(AuxChannel::Mavlink, batch).await {
        Ok(()) => {
            counters
                .frames_forwarded
                .fetch_add(frames, Ordering::Relaxed);
            counters.bytes_forwarded.fetch_add(bytes, Ordering::Relaxed);
        }
        Err(error) => {
            counters.send_errors.fetch_add(1, Ordering::Relaxed);
            // The send path already dropped its socket, so the next batch
            // re-opens; back off first so a persistent fault does not turn
            // into an open storm.
            gate.on_failure(Instant::now(), &error);
        }
    }
    batch.clear();
    *batched = 0;
}
