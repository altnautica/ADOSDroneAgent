//! Auxiliary-stream liveness watchdog.

use super::*;

/// How often the auxiliary-stream liveness watchdog polls.
pub(super) const AUX_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Flat-counter window before the aux watchdog treats the aux transmitter as
/// silently stalled. Same 30 s window as the data-plane TX watchdog.
pub(super) const AUX_SILENCE_THRESHOLD: Duration = Duration::from_secs(30);

/// Watch the **auxiliary** application-stream transmitter's liveness and restart
/// the aux pair IN PLACE on a silent stall, never returning to the run loop's
/// respawn select.
///
/// This mirrors the data-plane delta-counter contract — process-liveness alone is
/// never proof of work — over the aux transmitter's own stats totals: its stats
/// line count (the loop is turning; `wfb_tx` prints one every second whether or
/// not traffic moved) and its ingress bytes (the application frames a plugin
/// feeds it). It differs from the data-plane watchdog in ONE deliberate way: it
/// owns its own recovery. A stalled aux pair must NOT trigger a whole-group
/// respawn (that would interrupt the data + control planes, breaking the
/// additive-aux invariant), so on a stall the watchdog calls
/// [`RadioProcesses::restart_aux_stream`] directly and keeps watching. It returns
/// only when cancelled.
///
/// SAFE while the aux stream is closed: the watchdog idles (resetting its
/// windows) and never restarts anything.
///
/// IDLE IS NOT A STALL. A low-rate aux channel legitimately sends nothing for
/// long stretches, so flat ingress on its own is not evidence of a wedge. Two
/// things fire: the stats lines stopping for the whole window (a loop that is
/// no longer turning), or ingress that advanced at least once and then went
/// flat for the window (a transmitter that WAS carrying frames and died).
pub async fn aux_liveness_watchdog(proc: std::sync::Arc<Mutex<RadioProcesses>>, cancel: Shutdown) {
    let mut last_progress = Instant::now();
    let mut last_line = Instant::now();
    let mut prev: Option<TxPlaneTotals> = None;
    // Whether ingress has advanced at least once since the stream was opened.
    // A never-fed (idle) stream keeps this false, so flat ingress alone never
    // restarts it.
    let mut had_activity = false;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(AUX_POLL_INTERVAL) => {}
            _ = cancel.wait() => return,
        }

        // Resolve this tick's pair state under one lock. An exited half, or a
        // stream a plugin still wants whose pair is down (a failed restart),
        // is restarted now and retried every poll until it comes back; the
        // retry is the fixed poll interval with no cap.
        let (action, totals) = {
            let mut p = proc.lock().await;
            let action = aux_pair_action(p.aux_wanted(), p.aux_tx_pid().is_some(), p.aux_exited());
            (action, p.aux_totals())
        };
        let reset = |last_progress: &mut Instant,
                     last_line: &mut Instant,
                     prev: &mut Option<TxPlaneTotals>,
                     had_activity: &mut bool| {
            *last_progress = Instant::now();
            *last_line = Instant::now();
            *prev = None;
            *had_activity = false;
        };
        match action {
            AuxPairAction::Idle => {
                reset(
                    &mut last_progress,
                    &mut last_line,
                    &mut prev,
                    &mut had_activity,
                );
                continue;
            }
            AuxPairAction::Restart => {
                tracing::warn!("aux_pair_down_restarting");
                let _ = proc.lock().await.restart_aux_stream().await;
                reset(
                    &mut last_progress,
                    &mut last_line,
                    &mut prev,
                    &mut had_activity,
                );
                continue;
            }
            AuxPairAction::Watch => {}
        }

        let Some(before) = prev else {
            // First reading of a freshly-opened stream: seed the baseline.
            prev = Some(totals);
            last_progress = Instant::now();
            last_line = Instant::now();
            continue;
        };
        if totals.lines > before.lines {
            last_line = Instant::now();
        }
        let window_elapsed = last_progress.elapsed() >= AUX_SILENCE_THRESHOLD;
        let silent = last_line.elapsed() >= AUX_SILENCE_THRESHOLD;
        let tick = if silent {
            AuxTick::Restart
        } else {
            aux_tick_decision(
                before.bytes_in,
                totals.bytes_in,
                had_activity,
                window_elapsed,
            )
        };
        match tick {
            AuxTick::Progress => {
                // The plugin fed the pipe: real activity. From here a later
                // sustained-flat window is a genuine stall.
                had_activity = true;
                last_progress = Instant::now();
            }
            AuxTick::Restart => {
                tracing::warn!(
                    silent,
                    elapsed_s = last_progress.elapsed().as_secs(),
                    "aux_tx_stalled_restarting"
                );
                // ADDITIVE recovery: restart ONLY the aux pair, in place.
                let _ = proc.lock().await.restart_aux_stream().await;
                reset(
                    &mut last_progress,
                    &mut last_line,
                    &mut prev,
                    &mut had_activity,
                );
                continue;
            }
            AuxTick::Hold => {}
        }
        prev = Some(totals);
    }
}

/// What the aux watchdog does with the pair this tick, before any counter is
/// read. `wanted` is whether the stream is open (settings retained), `spawned`
/// whether an aux tx process is held, `exited` whether either held half has
/// exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AuxPairAction {
    /// Closed: nothing to watch.
    Idle,
    /// Wanted but a half has exited or the pair is down: restart it.
    Restart,
    /// Running: judge its ingress counter.
    Watch,
}

pub(super) fn aux_pair_action(wanted: bool, spawned: bool, exited: bool) -> AuxPairAction {
    if !wanted {
        AuxPairAction::Idle
    } else if exited || !spawned {
        AuxPairAction::Restart
    } else {
        AuxPairAction::Watch
    }
}

/// The aux liveness watchdog's per-tick decision, factored out so the
/// idle-vs-stall distinction is unit-testable without `/proc` or a live radio.
///
/// Given the previous + current ingress counters, whether the counter has ever
/// advanced since the stream opened (`had_activity`), and whether the silence
/// window has elapsed, decide whether to restart the aux pair. The cardinal
/// rule: an idle stream (one that never fed) is NEVER restarted; only a stream
/// that fed and then went sustained-flat is a stall.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum AuxTick {
    /// Counter advanced: real activity, reset the progress clock.
    Progress,
    /// Counter flat but the stream is idle / within the window: leave running.
    Hold,
    /// Counter flat after prior activity AND past the silence window: restart.
    Restart,
}

pub(super) fn aux_tick_decision(
    prev: u64,
    cur: u64,
    had_activity: bool,
    window_elapsed: bool,
) -> AuxTick {
    if cur > prev {
        AuxTick::Progress
    } else if had_activity && window_elapsed {
        AuxTick::Restart
    } else {
        AuxTick::Hold
    }
}
