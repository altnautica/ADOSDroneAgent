//! A lockstep participant: one lane of work the agent's barrier waits on
//! before it lets the simulator step past a grant.
//!
//! A participant registers by connecting to the barrier's Unix stream socket
//! ([`PARTICIPANTS_SOCKET`], beside the plugin relay's) and reports its
//! progress as `DONE` frames: every piece of its work due before `done_ns` is
//! finished, its commands written. A periodic lane reports its next due time
//! after each tick ([`Participant::done_until`]). Dropping the participant
//! closes the connection, which unregisters it.

use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use super::wire::Done;
use super::{Interval, RECONNECT_EVERY};

/// The barrier's socket, in the same directory as the relay socket so the
/// plugin host's bind of that directory covers both.
pub const PARTICIPANTS_SOCKET: &str = "participants.sock";

/// The barrier socket beside the relay socket at `relay`.
pub fn participants_socket_beside(relay: &Path) -> PathBuf {
    relay.with_file_name(PARTICIPANTS_SOCKET)
}

/// One registered lane. Reports are kept as the highest `done_ns` seen and
/// written by a background task, so [`Participant::done`] never blocks.
pub struct Participant {
    tx: watch::Sender<Option<u64>>,
    task: tokio::task::JoinHandle<()>,
}

impl Participant {
    /// Register with the barrier at `path`: a background task connects,
    /// writes the latest report on every change, and reconnects every
    /// [`RECONNECT_EVERY`] after a refused or dropped connection (each
    /// connection registers afresh and starts with the latest report). Needs
    /// a Tokio runtime.
    pub fn connect(path: PathBuf) -> Participant {
        let (tx, rx) = watch::channel(None::<u64>);
        let task = tokio::spawn(async move {
            loop {
                if let Ok(mut s) = tokio::net::UnixStream::connect(&path).await {
                    let mut rx = rx.clone();
                    rx.mark_changed();
                    while rx.changed().await.is_ok() {
                        let latest = *rx.borrow_and_update();
                        if let Some(done_ns) = latest {
                            if s.write_all(&Done { done_ns }.encode()).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                tokio::time::sleep(RECONNECT_EVERY).await;
            }
        });
        Participant { tx, task }
    }

    /// Every piece of this lane's work due before sim time `t_ns` is done.
    /// A report at or below an earlier one changes nothing.
    pub fn done(&self, t_ns: u64) {
        self.tx.send_if_modified(|d| {
            let newer = d.is_none_or(|d| t_ns > d);
            if newer {
                *d = Some(t_ns);
            }
            newer
        });
    }

    /// A periodic lane after its tick: done until the interval's next due
    /// time. Nothing before the interval's first tick.
    pub fn done_until(&self, iv: &Interval) {
        if let Some(n) = iv.next_due_ns() {
            self.done(n);
        }
    }

    /// The latest report.
    pub fn reported(&self) -> Option<u64> {
        *self.tx.borrow()
    }
}

impl Drop for Participant {
    fn drop(&mut self) {
        self.task.abort();
    }
}
