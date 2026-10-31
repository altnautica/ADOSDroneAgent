//! Encoder-ceiling publish and retry.

use super::*;

/// Gap between retries of a failed encoder-ceiling publish. Fixed and uncapped:
/// an encoder that comes back after an outage gets its ceiling within one gap,
/// however long it was down.
pub(super) const CEILING_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// One warning per this many consecutive failures, after the first.
///
/// At the capped retry gap that is roughly one line every half hour: quiet
/// enough to live with for the life of a process, frequent enough that the
/// fault is still on the record.
pub(super) const CEILING_LOG_EVERY: u32 = 30;

/// When to retry publishing the encoder ceiling after a failed attempt.
///
/// Without this the controller retried at the tick rate, which is once a second,
/// forever. The path that made it permanent is a node with no encoder at all:
/// nothing publishes an encoder state, so there is no observed ceiling to
/// reconcile against and the fallback compares against the last SUCCESSFUL
/// publish — which never happens, because every attempt fails. So the
/// needs-apply test was true on every tick from boot, and one absent socket
/// became a warning every second for as long as the node ran.
///
/// The answer is not to stop trying. An encoder that starts late, or comes back
/// after a crash, has to get its ceiling promptly. So the attempt repeats on a
/// fixed [`CEILING_RETRY_INTERVAL`], and the log decays instead of the retry:
/// loud on the first failure, then one line per [`CEILING_LOG_EVERY`] failures
/// carrying the running total, so an operator reading the log an hour later
/// still learns that this has been failing the whole time and how many times.
///
/// A change of intent — a rung step, or the ladder being disarmed — clears the
/// wait. That is new information the encoder has not been told yet, and it
/// preserves the original contract of one attempt per rung change.
#[derive(Debug, Default)]
pub(super) struct CeilingRetry {
    /// Consecutive failed attempts since the last success. Not reset by a
    /// change of intent: it is the count of how long this has been broken.
    pub(super) failures: u32,
    /// Earliest instant the next attempt may run after a failure.
    next_attempt: Option<Instant>,
    /// The ceiling the last attempt tried to publish, so a change of intent can
    /// be told from a repeat of the same one.
    attempted: Option<Option<u32>>,
}

impl CeilingRetry {
    /// Whether an attempt to publish `want` may run now.
    pub(super) fn should_attempt(&self, want: Option<u32>, now: Instant) -> bool {
        // New intent always gets an immediate attempt.
        if self.attempted != Some(want) {
            return true;
        }
        self.next_attempt.map(|at| now >= at).unwrap_or(true)
    }

    /// Record a failed attempt and schedule the next one a fixed interval out.
    pub(super) fn record_failure(&mut self, want: Option<u32>, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.attempted = Some(want);
        self.next_attempt = Some(now + CEILING_RETRY_INTERVAL);
    }

    /// Record a successful attempt, returning how many failures it ended.
    pub(super) fn record_success(&mut self, want: Option<u32>) -> u32 {
        let recovered_from = self.failures;
        self.failures = 0;
        self.next_attempt = None;
        self.attempted = Some(want);
        recovered_from
    }

    /// Whether this failure count warrants a warning rather than a debug line.
    pub(super) fn should_warn(failures: u32) -> bool {
        failures == 1 || failures.is_multiple_of(CEILING_LOG_EVERY)
    }
}

/// Whether the encoder ceiling needs (re)publishing.
///
/// `observed` is `ados-video`'s own published ceiling: `Some(live)` when it has
/// stamped its sidecar, `None` when it has not (service down, or no pipeline on
/// this node). When there is a published truth, reconcile against it — that is
/// what makes the ceiling self-heal across an `ados-video` restart, which resets
/// the ceiling to `None` while this controller still believes it is clamped.
/// With no published truth, fall back to comparing against our own last
/// successful publish, so a down service costs one failed attempt per rung change
/// instead of one per second.
pub(super) fn ceiling_needs_apply(
    observed: Option<Option<u32>>,
    asserted: Option<u32>,
    want: Option<u32>,
) -> bool {
    match observed {
        Some(live) => live != want,
        None => asserted != want,
    }
}
impl BitrateController {
    /// Publish the ladder's bitrate ceiling to `ados-video` when it differs from
    /// what is actually applied there. `observed` is the sidecar state already
    /// read this tick, so the steady-state cost is zero socket traffic.
    pub(super) async fn reconcile_encoder_ceiling(
        &mut self,
        want: Option<u32>,
        observed: Option<&EncoderState>,
    ) {
        let live = observed.map(|s| s.ceiling_kbps);
        if !ceiling_needs_apply(live, self.asserted_ceiling_kbps, want) {
            return;
        }
        // On a node with no encoder the check above is true on every tick
        // forever, because only a SUCCESSFUL publish advances what it compares
        // against. Backing the attempt off is what stops that from being a
        // warning every second for the life of the process, without the
        // controller giving up on an encoder that has not started yet.
        let now = Instant::now();
        if !self.ceiling_retry.should_attempt(want, now) {
            return;
        }
        match ados_video::profile::set_bitrate_ceiling_at(&self.encoder_sock, want).await {
            Ok(state) => {
                self.asserted_ceiling_kbps = want;
                let recovered_from = self.ceiling_retry.record_success(want);
                tracing::info!(
                    ceiling_kbps = ?want,
                    profile = state.profile.as_str(),
                    encoder_bitrate_kbps = state.bitrate_kbps,
                    recovered_after_failures = recovered_from,
                    "encoder_ceiling_applied"
                );
            }
            Err(e) => {
                self.ceiling_retry.record_failure(want, now);
                let failures = self.ceiling_retry.failures;
                // Loud the first time, then one line per CEILING_LOG_EVERY
                // carrying the running total, so the fault stays on the record
                // without filling it.
                if CeilingRetry::should_warn(failures) {
                    tracing::warn!(
                        error = %e,
                        ceiling_kbps = ?want,
                        consecutive_failures = failures,
                        "encoder_ceiling_apply_failed"
                    );
                } else {
                    tracing::debug!(
                        error = %e,
                        ceiling_kbps = ?want,
                        consecutive_failures = failures,
                        "encoder_ceiling_apply_failed"
                    );
                }
            }
        }
    }
}
