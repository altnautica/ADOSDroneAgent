//! Flight-controller state reads shared by the supervisor's safety gates.
//!
//! Anything the supervisor does that can disturb a vehicle in flight (a shared
//! USB hub reset, re-opening a radio bind window) gates on the FC being
//! provably disarmed. An unknown armed state is treated as unsafe.

use std::path::Path;
use std::time::Duration;

/// The vehicle-state socket the MAVLink service publishes snapshots on.
pub const STATE_SOCK: &str = "/run/ados/state.sock";

/// Extract the FC armed flag from one state-socket snapshot, reading whichever
/// wire form the producer emits (v1 newline JSON or v2 length-prefixed msgpack)
/// via the shared auto-detecting state reader. `None` on a clean EOF, a framing
/// error or a snapshot with no `armed` field.
pub async fn armed_from_state<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Option<bool> {
    ados_protocol::state::read_state_value(reader)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.get("armed").and_then(|x| x.as_bool()))
}

/// Read the FC armed state from the vehicle-state socket at `sock` (the MAVLink
/// service pushes a snapshot on connect, then a fresh one at ~10 Hz).
/// `Some(armed)` on a fresh read; `None` when the socket is absent, unreadable,
/// slow (1 s bound per step) or carries no armed field. Callers treat `None`
/// as unsafe (fail-closed).
pub async fn read_fc_armed_at(sock: &Path) -> Option<bool> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::UnixStream::connect(sock),
    )
    .await
    .ok()?
    .ok()?;
    tokio::time::timeout(Duration::from_secs(1), armed_from_state(&mut stream))
        .await
        .ok()
        .flatten()
}

/// [`read_fc_armed_at`] against the live [`STATE_SOCK`].
pub async fn read_fc_armed() -> Option<bool> {
    read_fc_armed_at(Path::new(STATE_SOCK)).await
}

#[cfg(test)]
mod tests;
