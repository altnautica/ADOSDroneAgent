//! The vehicle-state subscriber that feeds this node's beacon.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::ipc::connect_with_retry;
use ados_protocol::state::{read_state_value, STATE_V2_MAX_FRAME};
use parking_lot::Mutex;
use tokio::sync::watch;

use super::{cancelled, SharedState, StateSnapshot};

/// Read the vehicle-state socket into a shared cell, reconnecting forever.
///
/// The MAVLink router owns vehicle state; this is a read-only subscriber to the same
/// `state.sock` every other consumer uses, so the beacon reports the same numbers the
/// telemetry surfaces do. An absent socket leaves the cell empty and the beacon goes
/// out with no position and no condition bits, which reads correctly as "on the bus,
/// no fix".
pub(super) fn spawn_state_reader(
    socket_path: String,
    cancel: watch::Receiver<bool>,
) -> (SharedState, tokio::task::JoinHandle<()>) {
    let shared: SharedState = Arc::new(Mutex::new(None));
    let writer = shared.clone();
    let task = tokio::spawn(async move {
        loop {
            let connect = connect_with_retry(&socket_path, 5, Duration::from_millis(300));
            let mut stream = tokio::select! {
                _ = cancelled(&cancel) => return,
                s = connect => match s {
                    Ok(s) => s,
                    Err(_) => {
                        tokio::select! {
                            _ = cancelled(&cancel) => return,
                            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                        }
                        continue;
                    }
                },
            };
            let mut reader =
                tokio::io::BufReader::with_capacity(STATE_V2_MAX_FRAME.min(64 * 1024), &mut stream);
            loop {
                let frame = tokio::select! {
                    _ = cancelled(&cancel) => return,
                    f = read_state_value(&mut reader) => f,
                };
                match frame {
                    Ok(Some(value)) => {
                        *writer.lock() = Some(StateSnapshot {
                            value,
                            received: Instant::now(),
                        });
                    }
                    // The stream ended or failed. Clear the cell rather than
                    // leaving the last snapshot standing: the reconnect loop
                    // below may take seconds, and a held value with no producer
                    // behind it is the frozen-fix case arriving by a different
                    // route. The age gate would catch it, but the honest state
                    // while there is no producer is no state.
                    Ok(None) | Err(_) => {
                        *writer.lock() = None;
                        break;
                    }
                }
            }
            tokio::select! {
                _ = cancelled(&cancel) => return,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    });
    (shared, task)
}
