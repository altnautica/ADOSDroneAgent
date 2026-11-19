//! One generation's ground transmitters, driven by the box's uplink state.
//!
//! While the box transmits, both `wfb_tx` children run and a liveness watcher
//! judges each off its own stats stream; a stall ends the generation. While
//! the box stands by for a phone, both are stopped, neither is respawned, and
//! the presence watcher's receiver runs in their place.

use ados_proc::GroupChild;
use ados_radio::tx_liveness::{watch_tx_liveness, TxVerdict, TX_POLL_INTERVAL, TX_SILENCE_WINDOW};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::uplink::uplink;
use crate::wfb_rx::{GroundTx, WfbRxManager};

pub(super) struct GenTx {
    tx: GroundTx,
    control_watch: Option<JoinHandle<TxVerdict>>,
    aux_watch: Option<JoinHandle<TxVerdict>>,
    /// Raised before a deliberate stop, so the watchers judge it standby.
    stopped: watch::Sender<bool>,
    /// The standby presence watcher's receiver, while standing by.
    watcher_rx: Option<GroupChild>,
}

impl GenTx {
    pub(super) fn new(tx: GroundTx) -> Self {
        Self {
            tx,
            control_watch: None,
            aux_watch: None,
            stopped: watch::channel(false).0,
            watcher_rx: None,
        }
    }

    /// Bring the transmitters to `transmit`. False when they had to start and
    /// could not, which ends the generation exactly as a spawn failure of the
    /// whole chain did.
    pub(super) async fn apply(&mut self, transmit: bool, manager: &WfbRxManager) -> bool {
        if transmit {
            self.watcher_rx = None;
            if self.tx.is_running() {
                return true;
            }
            self.stopped.send_replace(false);
            let streams = match self.tx.start() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "ground_tx_failed_to_start");
                    return false;
                }
            };
            self.control_watch = streams.control.map(|out| self.watch("tx_control", out));
            self.aux_watch = streams.aux.map(|out| self.watch("aux_tx", out));
            if self.control_watch.is_none() || self.aux_watch.is_none() {
                tracing::warn!(
                    tx_control_watched = self.control_watch.is_some(),
                    aux_tx_watched = self.aux_watch.is_some(),
                    "ground_tx_liveness_unwatched: a transmitter stats stream was unavailable"
                );
            }
            uplink().report_tx_up(true);
            return true;
        }
        if self.tx.is_running() {
            self.stopped.send_replace(true);
            self.tx.stop().await;
            self.abort_watches();
            tracing::info!("ground_tx_stopped_for_standby");
        }
        uplink().report_tx_up(false);
        if self.watcher_rx.is_none() {
            match manager.spawn_uplink_watcher() {
                Ok(child) => self.watcher_rx = Some(child),
                // The holder timer still runs without it; it then falls back
                // after the stale window, which is the safe direction.
                Err(e) => tracing::error!(error = %e, "uplink_watcher_rx_failed_to_start"),
            }
        }
        true
    }

    fn watch(&self, name: &'static str, out: tokio::process::ChildStdout) -> JoinHandle<TxVerdict> {
        tokio::spawn(watch_tx_liveness(
            name,
            out,
            TX_POLL_INTERVAL,
            TX_SILENCE_WINDOW,
            self.stopped.subscribe(),
        ))
    }

    fn abort_watches(&mut self) {
        for t in [self.control_watch.take(), self.aux_watch.take()]
            .into_iter()
            .flatten()
        {
            t.abort();
        }
    }

    /// Resolve when a running transmitter stalls. Parks forever while none
    /// runs or none is watched: a missing stats handle is this process's own
    /// plumbing fault, never a verdict about the transmitter.
    pub(super) async fn wait_stall(&mut self) {
        match (&mut self.control_watch, &mut self.aux_watch) {
            (Some(a), Some(b)) => {
                tokio::select! {
                    _ = a => {}
                    _ = b => {}
                }
            }
            (Some(a), None) | (None, Some(a)) => {
                let _ = a.await;
            }
            (None, None) => std::future::pending::<()>().await,
        }
    }
}

impl Drop for GenTx {
    /// The generation ended: the watchers hold the read end of pipes whose
    /// writers are killpg'd with `tx`, so a previous generation's watcher never
    /// judges the next one's radio. Nothing transmits until the next
    /// generation starts its own pair.
    fn drop(&mut self) {
        self.abort_watches();
        uplink().report_tx_up(false);
    }
}
