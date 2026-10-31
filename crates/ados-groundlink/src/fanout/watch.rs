use super::*;

/// The fan-out watchdog decision, pure so it is testable without sockets.
///
/// `prev` and `current` are datagrams forwarded SINCE THE CURRENT BIND, not the
/// process-cumulative total: the never-forwarded gate has to be per-generation
/// or a ground station whose drone has powered off would read as "once
/// forwarded, now flat" forever and rebind its loopback socket on every window
/// for the rest of the flight day.
///
/// `since_advance` is how long `current` has sat flat, and `drops_delta` is the
/// send failures accumulated within the current `window`.
pub fn fanout_decision(
    prev: u64,
    current: u64,
    since_advance: Duration,
    drops_delta: u64,
    window: Duration,
) -> FanoutDecision {
    let forward = if current == 0 {
        // Nothing has ever arrived to forward. Not a fault: no source is
        // delivering, so a flat counter is the correct reading.
        ForwardState::AwaitingFirstDatagram
    } else if current > prev {
        ForwardState::Advancing {
            datagrams: current - prev,
        }
    } else if since_advance < window {
        ForwardState::WithinStallWindow
    } else {
        ForwardState::Stalled { frozen_at: current }
    };
    let drain = if drops_delta >= FANOUT_DROP_DEGRADED_DELTA {
        DrainState::NotDraining { drops: drops_delta }
    } else {
        DrainState::Draining
    };
    FanoutDecision { forward, drain }
}

/// Sample `counters` until the forward path stalls, then return the frozen
/// cumulative `forwarded` value for the caller to log and act on.
///
/// `baseline` is the cumulative `forwarded` total at the moment this bind
/// generation started, which is what makes [`fanout_decision`]'s
/// never-forwarded gate per-generation.
///
/// A sustained drop climb is warned about and does NOT return: the fan-out
/// itself is healthy in that case, and rebinding its socket would not make a
/// downstream consumer drain any faster. One warning per window, re-armed when
/// the window rolls over, so a persistently wedged consumer is visible without
/// flooding the journal.
pub(super) async fn watch_for_fanout_stall(
    counters: &FanoutCounters,
    baseline: u64,
    window: Duration,
    interval: Duration,
) -> u64 {
    let mut prev: u64 = 0;
    let mut last_advance = tokio::time::Instant::now();
    let mut drop_anchor = counters.drops();
    let mut drop_window_start = last_advance;
    let mut drop_warned = false;
    loop {
        tokio::time::sleep(interval).await;
        let now = tokio::time::Instant::now();
        let current = counters.forwarded().saturating_sub(baseline);
        let drops_delta = counters.drops().saturating_sub(drop_anchor);
        let decision = fanout_decision(
            prev,
            current,
            now.saturating_duration_since(last_advance),
            drops_delta,
            window,
        );

        if let DrainState::NotDraining { drops } = decision.drain {
            if !drop_warned {
                tracing::warn!(
                    drops,
                    window_s = window.as_secs_f64(),
                    "fanout_downstream_not_draining"
                );
                drop_warned = true;
            }
        }
        if now.saturating_duration_since(drop_window_start) >= window {
            drop_anchor = counters.drops();
            drop_window_start = now;
            drop_warned = false;
        }

        match decision.forward {
            ForwardState::Advancing { .. } => {
                prev = current;
                last_advance = now;
            }
            ForwardState::AwaitingFirstDatagram | ForwardState::WithinStallWindow => {}
            ForwardState::Stalled { .. } => return counters.forwarded(),
        }
    }
}
