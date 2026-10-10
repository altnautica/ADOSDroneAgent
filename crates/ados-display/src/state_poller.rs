//! Off-loop agent-state polling for the LCD page UI.
//!
//! [`StateSource::build_context`] makes blocking HTTP calls (each with a 0.9 s
//! ceiling), a logging-store query and file reads. Run on the render loop, a
//! restarting or wedged agent stalled touches, buttons, renders and the systemd
//! watchdog for seconds per poll. The [`StatePoller`] owns the source on its own
//! OS thread and publishes each fresh [`PageContext`] through a
//! [`tokio::sync::watch`] channel; the render loop only ever `borrow()`s the
//! latest value, so it never waits on the agent.
//!
//! Two triggers wake the thread: the timed poll every `period`, which is the
//! only poll that advances the sparkline history, and a refresh-now
//! ([`StatePoller::refresh_now`], a [`tokio::sync::Notify`]) the render loop
//! raises on a route or modal change so the new page gets fresh state soon.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Notify};

use crate::pages::PageContext;
use crate::state_source::StateSource;

/// Handle to the poller thread. Dropping it stops the thread at its next wake.
pub struct StatePoller {
    rx: watch::Receiver<PageContext>,
    notify: Arc<Notify>,
    stop: Arc<AtomicBool>,
}

impl StatePoller {
    /// Poll `source` now, then every `period` (history-advancing), and on every
    /// [`StatePoller::refresh_now`] (not history-advancing).
    pub fn spawn(mut source: StateSource, period: Duration) -> Self {
        Self::spawn_with(period, move |advance| source.build_context(advance))
    }

    /// Run `fetch(advance_history)` on the poller thread with the same cadence
    /// as [`StatePoller::spawn`].
    pub fn spawn_with<F>(period: Duration, fetch: F) -> Self
    where
        F: FnMut(bool) -> PageContext + Send + 'static,
    {
        let (tx, rx) = watch::channel(PageContext::default());
        let notify = Arc::new(Notify::new());
        let stop = Arc::new(AtomicBool::new(false));
        {
            let notify = notify.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("ados-display-state".to_string())
                .spawn(move || poll_loop(tx, notify, stop, period, fetch))
                .expect("spawn display state poller thread");
        }
        Self { rx, notify, stop }
    }

    /// Ask for a fresh context now instead of at the next timed poll. Never
    /// blocks; repeated requests before the thread wakes coalesce into one.
    pub fn refresh_now(&self) {
        self.notify.notify_one();
    }

    /// The latest published context. Never blocks on the agent.
    pub fn borrow(&self) -> watch::Ref<'_, PageContext> {
        self.rx.borrow()
    }

    /// A receiver the render loop can `changed().await` on to repaint when a
    /// fresh context lands.
    pub fn subscribe(&self) -> watch::Receiver<PageContext> {
        self.rx.clone()
    }
}

impl Drop for StatePoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

fn poll_loop<F>(
    tx: watch::Sender<PageContext>,
    notify: Arc<Notify>,
    stop: Arc<AtomicBool>,
    period: Duration,
    mut fetch: F,
) where
    F: FnMut(bool) -> PageContext,
{
    // A private current-thread runtime only to wait on the timer or the
    // Notify; the blocking fetch runs inline on this dedicated thread.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build display state poller runtime");
    rt.block_on(async move {
        let mut next_timed = tokio::time::Instant::now();
        loop {
            let timed = tokio::select! {
                _ = tokio::time::sleep_until(next_timed) => true,
                _ = notify.notified() => false,
            };
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if timed {
                let now = tokio::time::Instant::now();
                next_timed += period;
                if next_timed <= now {
                    // A fetch overran the period: re-anchor instead of bursting.
                    next_timed = now + period;
                }
            }
            let ctx = fetch(timed);
            if tx.send(ctx).is_err() {
                // Every receiver is gone; the UI has shut down.
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use super::*;

    fn offline_source(base: &str) -> StateSource {
        StateSource::with_paths(
            base,
            Path::new("/nonexistent/pairing.json"),
            PathBuf::from("/nonexistent/hop.json"),
        )
    }

    #[tokio::test]
    async fn ten_route_changes_do_not_advance_history() {
        let poller = StatePoller::spawn(
            offline_source("http://127.0.0.1:1"),
            Duration::from_secs(3600),
        );
        let mut rx = poller.subscribe();
        let wait = Duration::from_secs(10);

        // The first (timed) poll seeds one sample per series.
        tokio::time::timeout(wait, rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rx.borrow_and_update().link.rssi_history.len(), 1);

        for _ in 0..10 {
            poller.refresh_now();
            tokio::time::timeout(wait, rx.changed())
                .await
                .unwrap()
                .unwrap();
            let ctx = rx.borrow_and_update();
            assert_eq!(ctx.link.rssi_history.len(), 1);
            assert_eq!(ctx.system.cpu_history.len(), 1);
            assert_eq!(ctx.fc.battery_history.len(), 1);
        }
    }

    #[tokio::test]
    async fn slow_http_does_not_block_touch_to_present() {
        // An agent that takes 2 s to answer every request.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(2));
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
                    );
                });
            }
        });

        let poller = StatePoller::spawn(offline_source(&base), Duration::from_secs(5));
        // Let the poller get stuck inside its first request.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A touch lands: the loop asks for fresh state and repaints from the
        // latest context. Neither step may wait on the slow agent.
        let (touch_tx, mut touch_rx) = tokio::sync::mpsc::channel::<()>(1);
        touch_tx.send(()).await.unwrap();
        let t0 = Instant::now();
        touch_rx.recv().await.unwrap();
        poller.refresh_now();
        let ctx = poller.borrow().clone();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "touch to present took {elapsed:?}"
        );
        // The frame still renders from a context (the default before the first
        // poll lands).
        assert!(ctx.link.rssi_dbm.is_none());
    }
}
