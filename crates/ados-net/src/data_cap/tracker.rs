//! Usage accrual, persistence and threshold classification.

use super::*;

impl UsageState {
    pub(super) fn fresh(month: String) -> Self {
        Self {
            window_started_at: now_secs(),
            cumulative_bytes: 0,
            last_rx: 0,
            last_tx: 0,
            last_iface: None,
            last_reset_month: month,
        }
    }

    /// A new month window that keeps the current kernel-counter baseline, so
    /// the first poll of the month counts only traffic since the last sample
    /// and not every byte the interface has carried since boot.
    fn next_month(&self, month: String) -> Self {
        Self {
            window_started_at: now_secs(),
            cumulative_bytes: 0,
            last_rx: self.last_rx,
            last_tx: self.last_tx,
            last_iface: self.last_iface.clone(),
            last_reset_month: month,
        }
    }

    /// Render with a fixed key order and `", "` / `": "` separators.
    pub fn render_json(&self) -> String {
        let w = serde_json::to_string(&self.window_started_at).unwrap_or_else(|_| "0.0".into());
        let m = serde_json::to_string(&self.last_reset_month).unwrap_or_else(|_| "\"\"".into());
        let i = serde_json::to_string(&self.last_iface).unwrap_or_else(|_| "null".into());
        format!(
            "{{\"window_started_at\": {w}, \"cumulative_bytes\": {}, \"last_rx\": {}, \"last_tx\": {}, \"last_iface\": {i}, \"last_reset_month\": {m}}}",
            self.cumulative_bytes, self.last_rx, self.last_tx
        )
    }
}

impl DataCapTracker {
    /// Build a tracker with the default cap and state path.
    pub fn new(source: Arc<dyn UsageSource>, bus: Arc<UplinkEventBus>) -> Self {
        Self::with_config(source, bus, DEFAULT_CAP_GB, PathBuf::from(USAGE_STATE_PATH))
    }

    /// Full constructor (tests).
    pub fn with_config(
        source: Arc<dyn UsageSource>,
        bus: Arc<UplinkEventBus>,
        cap_gb: f64,
        state_path: PathBuf,
    ) -> Self {
        let cap_bytes = cap_to_bytes(cap_gb);
        let state = load_state(&state_path);
        Self {
            source,
            bus,
            cap_bytes,
            state_path,
            state,
            last_threshold: None,
            emitter: None,
        }
    }

    /// Attach a durable-store emitter. Builder-style so the daemon can opt in to
    /// shipping each poll's usage block as a `net.modem_usage` event while tests
    /// keep the default (no emitter). The emitter is best-effort and never gates
    /// the poll's persist.
    pub fn with_emitter(mut self, emitter: IngestEmitter) -> Self {
        self.emitter = Some(emitter);
        self
    }

    /// Reset the monthly cap.
    pub fn set_cap(&mut self, gb: f64) {
        self.cap_bytes = cap_to_bytes(gb);
        info!(cap_gb = gb, "uplink.datacap_set");
    }

    /// Classify the current usage.
    pub fn classify(&self) -> DataCapState {
        classify(self.cap_bytes, self.state.cumulative_bytes)
    }

    /// Month-window reset check. Returns `true` when a reset happened. Compares against
    /// the local-time `%Y-%m`.
    pub fn check_month_reset(&mut self) -> bool {
        let now_month = current_month();
        if self.state.last_reset_month != now_month {
            info!(
                from_month = %self.state.last_reset_month,
                to_month = %now_month,
                bytes_used = self.state.cumulative_bytes,
                "uplink.datacap_month_reset"
            );
            self.state = self.state.next_month(now_month);
            self.last_threshold = None;
            self.save_state();
            true
        } else {
            false
        }
    }

    /// fsync-before-rename persist. The fsync defends against power loss between
    /// the write and the kernel flushing dirty pages, which would otherwise roll
    /// the cap counter back.
    pub(super) fn save_state(&self) {
        let body = self.state.render_json();
        if let Err(exc) = sidecar::write_atomic_fsync(&self.state_path, body.as_bytes()) {
            warn!(error = %exc, "uplink.datacap_save_failed");
        }
    }

    /// One poll. Reads the source, applies counter-reset handling, accumulates,
    /// persists, and emits a `data_cap_threshold` event ONLY on a state
    /// transition.
    pub async fn poll_once(&mut self) {
        if let Some(sample) = self.source.data_usage().await {
            let carried = self.accrue(&sample);
            self.state.cumulative_bytes += carried;
            self.state.last_rx = sample.bytes.rx_bytes;
            self.state.last_tx = sample.bytes.tx_bytes;
            self.state.last_iface = Some(sample.iface);
        }
        self.save_state();

        // Ship the just-persisted usage snapshot to the store. The body is the
        // exact `get_usage()` block the modem view serves, so a store-first
        // reader gets byte-identical cumulative figures. Best-effort.
        self.emit_usage();

        let new_state = self.classify();
        if Some(new_state) != self.last_threshold {
            self.last_threshold = Some(new_state);
            info!(
                state = ?new_state,
                used_mb = self.state.cumulative_bytes / (1024 * 1024),
                cap_mb = self.cap_bytes / (1024 * 1024),
                "uplink.datacap_threshold"
            );
            self.bus.publish(UplinkEvent {
                kind: UplinkEventKind::DataCapThreshold,
                active_uplink: None,
                available: Vec::new(),
                internet_reachable: true,
                data_cap_state: Some(new_state),
                timestamp_ms: now_ms(),
            });
        }
    }

    /// Bytes carried since the previous sample. A sample from a different
    /// interface than the baseline only re-baselines (the two counters are
    /// unrelated). A counter smaller than its baseline restarted from zero, so
    /// its current value is the traffic since the restart.
    fn accrue(&self, sample: &UsageSample) -> u64 {
        if self.state.last_iface.as_deref() != Some(sample.iface.as_str()) {
            return 0;
        }
        let delta = |now: u64, last: u64| if now < last { now } else { now - last };
        delta(sample.bytes.rx_bytes, self.state.last_rx)
            + delta(sample.bytes.tx_bytes, self.state.last_tx)
    }

    /// Usage snapshot. Key names + rounding match the Python `get_usage`.
    pub fn get_usage(&self) -> Value {
        let used_mb = self.state.cumulative_bytes / (1024 * 1024);
        let cap_mb = self.cap_bytes / (1024 * 1024);
        let pct = if self.cap_bytes > 0 {
            round2((self.state.cumulative_bytes as f64 / self.cap_bytes as f64) * 100.0)
        } else {
            0.0
        };
        serde_json::json!({
            "data_used_mb": used_mb,
            "cap_mb": cap_mb,
            "percent": pct,
            "state": self.classify(),
            "window_reset_at": self.state.window_started_at,
            "last_reset_month": self.state.last_reset_month,
        })
    }

    /// Emit the current usage snapshot to the store as a `net.modem_usage`
    /// event. Best-effort: an absent or saturated emitter drops it silently and
    /// never disturbs the poll. The body is `get_usage()` so the stored row is a
    /// faithful copy of what the modem view serves.
    fn emit_usage(&self) {
        let Some(emitter) = self.emitter.as_ref() else {
            return;
        };
        let body = self.get_usage();
        emitter.emit_event("net.modem_usage", Level::Info, json_object_to_fields(&body));
    }

    /// Flush the latest counter to disk (clean-shutdown hook).
    pub fn flush(&self) {
        self.save_state();
    }
}

pub(super) fn cap_to_bytes(gb: f64) -> u64 {
    (gb * 1024.0 * 1024.0 * 1024.0) as u64
}

/// Shared classifier so the tracker and any external caller agree.
pub fn classify(cap_bytes: u64, cumulative_bytes: u64) -> DataCapState {
    if cap_bytes == 0 {
        return DataCapState::Ok;
    }
    let pct = (cumulative_bytes as f64 / cap_bytes as f64) * 100.0;
    if pct >= 100.0 {
        DataCapState::Blocked100
    } else if pct >= 95.0 {
        DataCapState::Throttle95
    } else if pct >= 80.0 {
        DataCapState::Warn80
    } else {
        DataCapState::Ok
    }
}

pub(super) fn load_state(path: &Path) -> UsageState {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<RawUsageState>(&bytes) {
            Ok(raw) => UsageState {
                window_started_at: raw.window_started_at.unwrap_or_else(now_secs),
                cumulative_bytes: raw.cumulative_bytes.unwrap_or(0),
                last_rx: raw.last_rx.unwrap_or(0),
                last_tx: raw.last_tx.unwrap_or(0),
                last_iface: raw.last_iface,
                last_reset_month: raw.last_reset_month.unwrap_or_default(),
            },
            Err(exc) => {
                warn!(error = %exc, "uplink.datacap_load_failed");
                UsageState::fresh(current_month())
            }
        },
        Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => {
            UsageState::fresh(current_month())
        }
        Err(exc) => {
            debug!(error = %exc, "uplink.datacap_load_failed");
            UsageState::fresh(current_month())
        }
    }
}
