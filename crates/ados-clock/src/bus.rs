//! Binding the sensor-bus socket and starting its broker.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use ados_sensorbus::broker::{self, BrokerStats};
use ados_sensorbus::BusFrame;
use tokio::net::UnixListener;
use tokio::sync::mpsc;

/// Frames buffered between the broker and the clock's consumer.
pub const TAP_QUEUE: usize = 8192;

/// Bind `path` (parent created, a stale socket removed, mode 0660, group
/// `ados-operator` when it exists) and spawn the broker on the current
/// runtime. Returns the tap carrying a copy of every published frame.
pub fn spawn_bus(path: &Path) -> io::Result<(mpsc::Receiver<BusFrame>, Arc<BrokerStats>)> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => tracing::info!(path = %path.display(), "removed stale bus socket"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    set_operator_group(path);
    let (tap_tx, tap_rx) = mpsc::channel(TAP_QUEUE);
    let stats = Arc::new(BrokerStats::default());
    tokio::spawn(broker::serve(listener, Some(tap_tx), stats.clone()));
    Ok((tap_rx, stats))
}

/// Hand the socket to the operator group so non-root agent services can
/// connect. Best-effort: absent on a dev host.
fn set_operator_group(path: &Path) {
    let group = ados_protocol::ipc::OPERATOR_GROUP;
    let gid = match nix::unistd::Group::from_name(group) {
        Ok(Some(g)) => g.gid,
        _ => {
            tracing::debug!(group, "socket group not present");
            return;
        }
    };
    if let Err(e) = nix::unistd::chown(path, None, Some(gid)) {
        tracing::debug!(error = %e, group, "chgrp bus socket failed");
    }
}
