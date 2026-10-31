use super::*;

impl SeamGate {
    pub(super) fn may_attempt(&self, now: Instant) -> bool {
        self.next_attempt.is_none_or(|at| now >= at)
    }

    pub(super) fn on_success(&mut self) {
        if self.outage_logged {
            tracing::info!("ground_aux_republish_recovered");
        }
        self.next_attempt = None;
        self.outage_logged = false;
    }

    pub(super) fn on_failure(&mut self, now: Instant, error: &IngestError) {
        if !self.outage_logged {
            self.outage_logged = true;
            tracing::warn!(error = %error, "ground_aux_republish_unavailable");
        } else {
            tracing::debug!(error = %error, "ground_aux_republish_still_unavailable");
        }
        self.next_attempt = Some(now + SEAM_RETRY_INTERVAL);
    }
}
