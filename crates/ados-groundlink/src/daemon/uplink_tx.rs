//! One generation's ground transmitters, driven by the box's uplink state.
//!
//! While the box transmits, both `wfb_tx` children run and a liveness watcher
//! judges each off its own stats stream; a stall ends the generation. While
//! the box stands by for a phone, both are stopped, neither is respawned, and
//! the presence watcher's receiver runs in their place.
//!
//! The generation attaches to the uplink when it starts, carries out every
//! ask in turn and reports the version it carried out (see
//! [`crate::uplink::TxControl`]), and detaches only once nothing it ran
//! transmits, so a take is confirmed only after this generation's own pair
//! exited, never on a report from before the ask.

use std::future::Future;

use ados_radio::tx_liveness::{watch_tx_liveness, TxVerdict, TX_POLL_INTERVAL, TX_SILENCE_WINDOW};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::uplink::{TxAsk, TxAsks, Uplink};
use crate::wfb_rx::{GroundTx, TxStdout};

#[cfg(test)]
mod tests;

/// The two ground transmitters a generation drives: [`GroundTx`] on the box,
/// a fake in the tests and the simulation.
pub trait TxPair {
    fn is_running(&self) -> bool;
    /// Spawn both; their stats streams.
    fn start(&mut self) -> impl Future<Output = std::io::Result<TxStdout>> + Send;
    /// Stop both; resolves once both exited.
    fn stop(&mut self) -> impl Future<Output = ()> + Send;
}

impl TxPair for GroundTx {
    fn is_running(&self) -> bool {
        GroundTx::is_running(self)
    }

    fn start(&mut self) -> impl Future<Output = std::io::Result<TxStdout>> + Send {
        std::future::ready(GroundTx::start(self))
    }

    fn stop(&mut self) -> impl Future<Output = ()> + Send {
        GroundTx::stop(self)
    }
}

/// One receive generation's ground transmitters, following the uplink's asks.
pub struct GenTx<'u, T: TxPair, W> {
    uplink: &'u Uplink,
    /// `None` only while dropping, which kills the pair before it detaches.
    tx: Option<T>,
    control_watch: Option<JoinHandle<TxVerdict>>,
    aux_watch: Option<JoinHandle<TxVerdict>>,
    /// Raised before a deliberate stop, so the watchers judge it standby.
    stopped: watch::Sender<bool>,
    /// The standby presence watcher's receiver, while standing by.
    watcher_rx: Option<W>,
}

impl<'u, T: TxPair, W> GenTx<'u, T, W> {
    /// Attach a new generation to `uplink`; the asks it follows from now on,
    /// starting with the one in force.
    pub fn attach(uplink: &'u Uplink, tx: T) -> (Self, TxAsks) {
        let asks = uplink.attach_generation();
        let gen_tx = Self {
            uplink,
            tx: Some(tx),
            control_watch: None,
            aux_watch: None,
            stopped: watch::channel(false).0,
            watcher_rx: None,
        };
        (gen_tx, asks)
    }

    /// Carry out `ask` and report it. False when the transmitters had to start
    /// and could not, which ends the generation exactly as a spawn failure of
    /// the whole chain did. `spawn_watcher` starts the standby watcher's
    /// receiver, when standing by without one.
    pub async fn apply(
        &mut self,
        ask: TxAsk,
        spawn_watcher: impl FnOnce() -> std::io::Result<W>,
    ) -> bool {
        let Some(tx) = self.tx.as_mut() else {
            return false;
        };
        if ask.transmit {
            self.watcher_rx = None;
            if !tx.is_running() {
                self.stopped.send_replace(false);
                let streams = match tx.start().await {
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
            }
            self.uplink.report_applied(ask.version, true);
            return true;
        }
        if tx.is_running() {
            self.stopped.send_replace(true);
            tx.stop().await;
            self.abort_watches();
            tracing::info!("ground_tx_stopped_for_standby");
        }
        self.uplink.report_applied(ask.version, false);
        if self.watcher_rx.is_none() {
            match spawn_watcher() {
                Ok(child) => self.watcher_rx = Some(child),
                // The holder timer still runs without it; it then falls back
                // after the stale window, which is the safe direction.
                Err(e) => tracing::error!(error = %e, "uplink_watcher_rx_failed_to_start"),
            }
        }
        true
    }

    /// End the generation: stop both transmitters and wait until both exited,
    /// then detach (in `Drop`), so a take waiting on this generation is
    /// answered only once nothing it ran transmits.
    pub async fn close(mut self) {
        if let Some(tx) = self.tx.as_mut().filter(|tx| tx.is_running()) {
            self.stopped.send_replace(true);
            tx.stop().await;
        }
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

impl<T: TxPair, W> Drop for GenTx<'_, T, W> {
    /// The generation ended: the watchers hold the read end of pipes whose
    /// writers are killpg'd with the pair, so a previous generation's watcher
    /// never judges the next one's radio. Dropping the pair killpg's anything
    /// still running before the uplink stops counting on this generation;
    /// [`GenTx::close`] is the orderly path that waits for both to exit.
    fn drop(&mut self) {
        self.abort_watches();
        drop(self.tx.take());
        self.uplink.detach_generation();
    }
}
