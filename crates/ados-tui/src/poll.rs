//! Background polling of the agent, and the render loop's view of the results.
//!
//! Both REST reads are blocking calls with a per-request timeout, so they run on
//! their own thread and hand each result to the render loop over a channel. The
//! loop only renders and handles keys, so a hung agent never freezes input. The
//! dashboard model is built once per successful poll and kept across frames.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ados_protocol::rest::RestClient;
use serde_json::Value;

use crate::model::{Dashboard, History};

/// One poll of the agent: the setup status the dashboard is built from, and the
/// best-effort native status carrying the FC transport/heartbeat split.
pub struct PollResult {
    pub setup: Result<Value, String>,
    pub fc_status: Option<Value>,
}

/// Handle to the poller thread.
pub struct Poller {
    results: Receiver<PollResult>,
    refresh: Sender<()>,
}

impl Poller {
    /// Start polling now, then every `interval` (or sooner on [`Poller::refresh`]).
    /// The thread exits once the handle is dropped.
    pub fn spawn(client: RestClient, interval: Duration) -> Self {
        let (result_tx, results) = mpsc::channel();
        let (refresh, refresh_rx) = mpsc::channel::<()>();
        thread::spawn(move || loop {
            let setup = client
                .setup_status()
                .map_err(|e| format!("Agent unreachable: {e}"));
            let fc_status = client.status().ok();
            if result_tx.send(PollResult { setup, fc_status }).is_err() {
                return;
            }
            match refresh_rx.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Requests queued while this poll ran are served by the next one.
            while refresh_rx.try_recv().is_ok() {}
        });
        Self { results, refresh }
    }

    /// Poll again now instead of at the next interval.
    pub fn refresh(&self) {
        let _ = self.refresh.send(());
    }

    /// Every result that landed since the last call, oldest first. Never blocks.
    pub fn drain(&self) -> impl Iterator<Item = PollResult> + '_ {
        self.results.try_iter()
    }
}

/// What the render loop knows about the agent.
#[derive(Default)]
pub struct AgentView {
    /// The dashboard from the last successful poll.
    pub dash: Option<Dashboard>,
    /// Trend buffers of verified telemetry, one sample per successful poll.
    pub history: History,
    /// Why the latest poll failed; cleared by the next success.
    pub error: Option<String>,
    /// Wall-clock time of the last successful poll. Advanced only on success.
    pub refreshed: Option<String>,
    last_success: Option<Instant>,
}

impl AgentView {
    /// Fold one poll result in. A success rebuilds the dashboard and moves the
    /// refreshed clock to `clock`; a failure only records the error and leaves
    /// the last snapshot, and the time it was taken, as they were.
    pub fn apply(&mut self, result: PollResult, now: Instant, clock: String) {
        match result.setup {
            Ok(setup) => {
                let mut dash = Dashboard::from_status(&setup);
                if let Some(fc) = &result.fc_status {
                    dash.merge_fc_status(fc);
                }
                self.history.record(&dash);
                self.dash = Some(dash);
                self.error = None;
                self.refreshed = Some(clock);
                self.last_success = Some(now);
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Whether the shown snapshot is older than `after`.
    pub fn is_stale(&self, now: Instant, after: Duration) -> bool {
        self.last_success
            .is_some_and(|t| now.saturating_duration_since(t) > after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(battery: f64) -> PollResult {
        PollResult {
            setup: Ok(json!({
                "version": "0.99.108",
                "profile": "drone",
                "mavlink": {"connected": true},
                "telemetry": {"battery": {"remaining": battery}}
            })),
            fc_status: None,
        }
    }

    #[test]
    fn a_failed_poll_keeps_the_snapshot_time_and_records_the_error() {
        let t0 = Instant::now();
        let mut view = AgentView::default();
        view.apply(ok(82.0), t0, "12:00:00".into());
        assert_eq!(view.refreshed.as_deref(), Some("12:00:00"));

        let failed = PollResult {
            setup: Err("Agent unreachable: connection refused".into()),
            fc_status: None,
        };
        view.apply(failed, t0 + Duration::from_secs(2), "12:00:02".into());
        assert_eq!(
            view.refreshed.as_deref(),
            Some("12:00:00"),
            "the refreshed clock only moves when data actually refreshed"
        );
        assert_eq!(
            view.error.as_deref(),
            Some("Agent unreachable: connection refused")
        );
        assert_eq!(view.dash.as_ref().and_then(|d| d.battery), Some(82.0));
        assert!(!view.is_stale(t0 + Duration::from_secs(2), Duration::from_secs(6)));
        assert!(view.is_stale(t0 + Duration::from_secs(7), Duration::from_secs(6)));

        view.apply(ok(80.0), t0 + Duration::from_secs(8), "12:00:08".into());
        assert_eq!(view.error, None);
        assert_eq!(view.refreshed.as_deref(), Some("12:00:08"));
        assert_eq!(view.history.battery, vec![82.0, 80.0]);
    }

    #[test]
    fn the_native_status_is_merged_into_the_built_dashboard() {
        let mut view = AgentView::default();
        let result = PollResult {
            setup: Ok(json!({"profile": "drone", "mavlink": {"connected": false}})),
            fc_status: Some(json!({"fcVariant": "inav"})),
        };
        view.apply(result, Instant::now(), "12:00:00".into());
        assert_eq!(
            view.dash.and_then(|d| d.fc_variant).as_deref(),
            Some("inav")
        );
    }
}
