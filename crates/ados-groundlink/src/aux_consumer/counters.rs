use super::*;

impl AuxCounters {
    /// A fresh zeroed counter set.
    pub fn new() -> Self {
        Self::default()
    }

    pub(super) fn bump(&self, field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }

    /// A point-in-time copy for the sidecar and the periodic log line.
    pub fn snapshot(&self) -> AuxCountersSnapshot {
        let c = &self.0;
        AuxCountersSnapshot {
            datagrams_received: c.datagrams_received.load(Ordering::Relaxed),
            bytes_received: c.bytes_received.load(Ordering::Relaxed),
            mavlink_frames: c.mavlink_frames.load(Ordering::Relaxed),
            status_frames: c.status_frames.load(Ordering::Relaxed),
            identity_frames: c.identity_frames.load(Ordering::Relaxed),
            mavlink_republished: c.mavlink_republished.load(Ordering::Relaxed),
            system_id_collisions: c.system_id_collisions.load(Ordering::Relaxed),
            republish_errors: c.republish_errors.load(Ordering::Relaxed),
            republish_lane_down: c.republish_lane_down.load(Ordering::Relaxed),
            decode_foreign: c.decode_foreign.load(Ordering::Relaxed),
            decode_runt: c.decode_runt.load(Ordering::Relaxed),
            decode_unsupported_version: c.decode_unsupported_version.load(Ordering::Relaxed),
            decode_unknown_channel: c.decode_unknown_channel.load(Ordering::Relaxed),
            decode_damaged: c.decode_damaged.load(Ordering::Relaxed),
            decode_overlong: c.decode_overlong.load(Ordering::Relaxed),
            status_undecodable: c.status_undecodable.load(Ordering::Relaxed),
            status_out_of_order: c.status_out_of_order.load(Ordering::Relaxed),
            identity_undecodable: c.identity_undecodable.load(Ordering::Relaxed),
            identity_plugin_version_mismatch: c
                .identity_plugin_version_mismatch
                .load(Ordering::Relaxed),
            response_frames: c.response_frames.load(Ordering::Relaxed),
            request_frames: c.request_frames.load(Ordering::Relaxed),
            link_feedback_frames: c.link_feedback_frames.load(Ordering::Relaxed),
            config_tunnel_frames: c.config_tunnel_frames.load(Ordering::Relaxed),
            config_tunnel_forwarded: c.config_tunnel_forwarded.load(Ordering::Relaxed),
            config_tunnel_undelivered: c.config_tunnel_undelivered.load(Ordering::Relaxed),
            response_undecodable: c.response_undecodable.load(Ordering::Relaxed),
            response_dispatched: c.response_dispatched.load(Ordering::Relaxed),
            response_orphan: c.response_orphan.load(Ordering::Relaxed),
            app_stream: c.app_stream.load(Ordering::Relaxed),
            app_stream_undelivered: c.app_stream_undelivered.load(Ordering::Relaxed),
            app_command_unexpected: c.app_command_unexpected.load(Ordering::Relaxed),
            recv_errors: c.recv_errors.load(Ordering::Relaxed),
            bind_failures: c.bind_failures.load(Ordering::Relaxed),
        }
    }
}

impl AuxCountersSnapshot {
    /// Datagrams accounted for by exactly one received path.
    ///
    /// Every datagram that arrives leaves through one terminal outcome: a
    /// decode failure, an ignored channel, or one of the three MAVLink
    /// dispositions. This sum must therefore equal `datagrams_received`, and a
    /// test asserts it against one datagram of every kind. It exists so the
    /// claim that the tally describes the whole lane is checkable rather than
    /// merely stated: a future channel or failure mode that forgets to bump a
    /// counter shows up here as a shortfall instead of silently making the lane
    /// look quieter than it is.
    ///
    /// `mavlink_frames` is deliberately excluded, being the subtotal that
    /// `mavlink_republished + republish_errors + republish_lane_down` splits
    /// into. Counting both would double every MAVLink datagram. The status and
    /// identity decode tallies (`status_undecodable`, `status_out_of_order`,
    /// `identity_undecodable`) are excluded on the same grounds: they describe
    /// what happened to a datagram already counted by `status_frames` /
    /// `identity_frames`, so adding them would double-count it.
    pub fn accounted(&self) -> u64 {
        self.mavlink_republished
            + self.republish_errors
            + self.republish_lane_down
            + self.status_frames
            + self.identity_frames
            + self.response_frames
            + self.request_frames
            + self.link_feedback_frames
            + self.config_tunnel_frames
            + self.decode_foreign
            + self.decode_runt
            + self.decode_unsupported_version
            + self.decode_unknown_channel
            + self.decode_damaged
            + self.decode_overlong
            + self.app_stream
            + self.app_command_unexpected
    }
}

/// Log the counters when they have moved since the last report.
pub(super) fn report(counters: &AuxCounters, last: AuxCountersSnapshot) -> AuxCountersSnapshot {
    let now = counters.snapshot();
    if now != last {
        tracing::info!(
            datagrams_received = now.datagrams_received,
            mavlink_frames = now.mavlink_frames,
            mavlink_republished = now.mavlink_republished,
            republish_errors = now.republish_errors,
            republish_lane_down = now.republish_lane_down,
            status_frames = now.status_frames,
            identity_frames = now.identity_frames,
            response_frames = now.response_frames,
            request_frames = now.request_frames,
            link_feedback_frames = now.link_feedback_frames,
            response_dispatched = now.response_dispatched,
            response_orphan = now.response_orphan,
            decode_foreign = now.decode_foreign,
            decode_damaged = now.decode_damaged,
            recv_errors = now.recv_errors,
            "ground_aux_consumer_counters"
        );
    }
    now
}
