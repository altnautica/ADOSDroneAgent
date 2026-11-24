//! The PreArm/Arm STATUSTEXT ring: long FC texts arrive in 50-byte chunks and
//! are joined before they enter the ring, so a surface shows one line.

use std::time::Instant;

use ados_protocol::statustext::{StatusTextAssembler, CHUNK_LEN};

use super::{VehicleState, PREARM_MAX, PREARM_WINDOW};

/// The chunk joiner the PreArm ring is fed through, with the instant its
/// millisecond clock counts from (the first STATUSTEXT seen).
#[derive(Debug, Clone, Default)]
pub struct PrearmChunks {
    joiner: StatusTextAssembler,
    epoch: Option<Instant>,
}

impl PrearmChunks {
    fn ms(&mut self, now: Instant) -> u64 {
        let epoch = *self.epoch.get_or_insert(now);
        now.saturating_duration_since(epoch).as_millis() as u64
    }
}

impl VehicleState {
    /// Fold one STATUSTEXT chunk; every PreArm/Arm text it completes joins
    /// the ring.
    pub(super) fn on_prearm_chunk(&mut self, severity: u8, text: &[u8; CHUNK_LEN]) {
        let now = self.clock.now();
        let ms = self.prearm_chunks.ms(now);
        let mut done = self.prearm_chunks.joiner.flush_stale(ms);
        done.extend(
            self.prearm_chunks
                .joiner
                .push_untagged(0, 0, severity, text, ms),
        );
        for (_, text) in done {
            if text.starts_with("PreArm:") || text.starts_with("Arm:") {
                self.prearm.push_back((now, text));
            }
        }
        self.prune_prearm(now);
    }

    /// Drop PreArm texts older than the window and bound the ring.
    pub(super) fn prune_prearm(&mut self, now: Instant) {
        while self
            .prearm
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= PREARM_WINDOW)
            || self.prearm.len() > PREARM_MAX
        {
            self.prearm.pop_front();
        }
    }
}
