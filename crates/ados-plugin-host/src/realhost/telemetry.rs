//! `telemetry.subscribe`: the vehicle-state snapshot, read once for the whole
//! host off the MAVLink service's state socket and fanned out to every
//! subscribing plugin.
//!
//! The socket carries either wire of contract B, v1 newline JSON or v2
//! length-prefixed msgpack, and a producer may switch between them across a
//! restart. [`ados_protocol::state::read_state_value`] sniffs each frame, so
//! this reader never has to know which one is live.

use super::*;

/// Canonical path of the MAVLink service's vehicle-state socket.
pub(super) const VEHICLE_STATE_SOCK: &str = "/run/ados/state.sock";

/// Fanout depth. The producer publishes at ~10 Hz and a subscriber only ever
/// wants the newest snapshot, so a subscriber this far behind lags and resumes
/// at the tail rather than draining stale state.
pub(super) const STATE_BROADCAST_DEPTH: usize = 8;

/// One state-socket connection's lifetime: decode each snapshot and
/// re-broadcast it as the msgpack map a plugin receives, until EOF or a
/// framing error (the caller reconnects).
pub(super) async fn state_pump(
    path: &std::path::Path,
    tx: &broadcast::Sender<Arc<Value>>,
) -> std::io::Result<()> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    let mut reader = tokio::io::BufReader::new(stream);
    while let Some(state) = ados_protocol::state::read_state_value(&mut reader).await? {
        // Between plugin sessions nobody listens; skip the conversion then.
        if tx.receiver_count() > 0 {
            let _ = tx.send(Arc::new(json_to_mpv(&state)));
        }
    }
    Ok(())
}
