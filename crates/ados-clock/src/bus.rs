//! Binding the sensor-bus socket and starting its broker.

use std::io;
use std::path::Path;
use std::sync::Arc;

use ados_protocol::ipc::{bind_reader_socket, SENSORS_GROUP};
use ados_sensorbus::broker::{self, BrokerStats};
use ados_sensorbus::BusFrame;
use tokio::sync::mpsc;

/// Frames buffered between the broker and the clock's consumer.
pub const TAP_QUEUE: usize = 8192;

/// Bind `path` as the bus's reader socket (parent created, a stale socket
/// removed, mode 0660, group [`SENSORS_GROUP`], every peer classified on
/// accept: services publish anything, operators sensor data, sensors-group
/// readers nothing) and spawn the broker on the current runtime. Returns the
/// tap carrying a copy of every published frame.
pub fn spawn_bus(path: &Path) -> io::Result<(mpsc::Receiver<BusFrame>, Arc<BrokerStats>)> {
    let listener = bind_reader_socket(path, 0o660, SENSORS_GROUP)?;
    let (tap_tx, tap_rx) = mpsc::channel(TAP_QUEUE);
    let stats = Arc::new(BrokerStats::default());
    tokio::spawn(broker::serve(listener, Some(tap_tx), stats.clone()));
    Ok((tap_rx, stats))
}
