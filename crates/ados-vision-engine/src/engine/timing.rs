//! Rolling per-model inference timing for the telemetry surface.

/// Rolling per-model inference timing for the telemetry surface. EWMA-smoothed so
/// one slow frame does not spike the reported latency/fps. All-zero until the
/// model runs (the honest "no data" reading, never a claim of zero throughput).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ModelTiming {
    pub(super) ewma_latency_ms: f32,
    pub(super) ewma_interval_ms: f32,
    pub(super) last_infer_ms: Option<i64>,
    pub(super) samples: u64,
}

impl ModelTiming {
    /// EWMA weight for a new sample (a light smoothing so the reading tracks the
    /// current rate without jitter).
    const ALPHA: f32 = 0.2;

    pub(super) fn record(&mut self, latency_ms: f32, now: i64) {
        self.ewma_latency_ms = if self.samples == 0 {
            latency_ms
        } else {
            Self::ALPHA * latency_ms + (1.0 - Self::ALPHA) * self.ewma_latency_ms
        };
        if let Some(last) = self.last_infer_ms {
            let interval = (now - last).max(0) as f32;
            if interval > 0.0 {
                self.ewma_interval_ms = if self.ewma_interval_ms == 0.0 {
                    interval
                } else {
                    Self::ALPHA * interval + (1.0 - Self::ALPHA) * self.ewma_interval_ms
                };
            }
        }
        self.last_infer_ms = Some(now);
        self.samples = self.samples.saturating_add(1);
    }

    pub(super) fn fps(&self) -> f32 {
        if self.ewma_interval_ms > 0.0 {
            1000.0 / self.ewma_interval_ms
        } else {
            0.0
        }
    }
}
