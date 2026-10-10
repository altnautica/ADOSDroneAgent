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
mod tests {
    use super::*;
    use ados_protocol::state::{encode_v1, encode_v2};
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn armed_from_state_reads_both_wire_forms_fail_closed() {
        // v2 (length-prefixed msgpack), the form the producer actually emits.
        let mut armed_v2 = std::io::Cursor::new(encode_v2(&json!({"armed": true})).unwrap());
        assert_eq!(armed_from_state(&mut armed_v2).await, Some(true));

        let mut disarmed_v2 =
            std::io::Cursor::new(encode_v2(&json!({"armed": false, "mode": "GUIDED"})).unwrap());
        assert_eq!(armed_from_state(&mut disarmed_v2).await, Some(false));

        // v1 (newline JSON), the legacy form, still read via the auto-detect.
        let mut armed_v1 = std::io::Cursor::new(encode_v1(&json!({"armed": true})).unwrap());
        assert_eq!(armed_from_state(&mut armed_v1).await, Some(true));

        // Fail-closed: an empty reader (clean EOF) yields None.
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(armed_from_state(&mut empty).await, None);

        // Fail-closed: a snapshot with no `armed` field yields None.
        let mut no_field = std::io::Cursor::new(encode_v2(&json!({"mode": "STABILIZE"})).unwrap());
        assert_eq!(armed_from_state(&mut no_field).await, None);
    }

    #[tokio::test]
    async fn read_fc_armed_at_reads_a_fake_state_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("state.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let frame = encode_v2(&json!({"armed": false})).unwrap();
            conn.write_all(&frame).await.unwrap();
        });
        assert_eq!(read_fc_armed_at(&sock).await, Some(false));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn read_fc_armed_at_is_none_without_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_fc_armed_at(&dir.path().join("absent.sock")).await,
            None
        );
    }
}
