//! Peer-presence evidence: the TUN RX poller and the verdict that gates Paired.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::PeerEvidence;
use crate::bind::fsm::{now_monotonic, BindSession};
use crate::bind::{iface, BindRole, TUNNEL_POLL_INTERVAL};

/// Decide whether a completed wire exchange proved a real peer participated.
/// `frames_seen` = the bind-TUN RX counter advanced this session (frames that
/// passed FEC + decryption in wfb_rx; a local TCP loopback never traverses the
/// TUN). `key_fresh` = (drone) the upstream key file was deposited by THIS
/// session, or (gs) unconditionally true (the GS never reads that file).
///
/// INVARIANT: [`PeerEvidence::Verified`] — the only verdict the caller lets
/// proceed to Paired — requires `frames_seen` AND `key_fresh`. Setting a state
/// is not proof of the state; a peer must be proven, never assumed.
pub(super) fn peer_evidence_verdict(
    role: BindRole,
    frames_seen: bool,
    key_fresh: bool,
) -> PeerEvidence {
    if !frames_seen {
        return PeerEvidence::NoFrames;
    }
    if role == BindRole::Drone && !key_fresh {
        return PeerEvidence::StaleKey;
    }
    PeerEvidence::Verified
}

/// True when `path` exists and its mtime is at or after `since` — i.e. the
/// upstream key was deposited by THIS bind session's wire transfer, not left
/// over from an earlier one. Total: any metadata error reads as not-fresh.
pub(super) fn upstream_key_fresh(path: &Path, since: std::time::SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t >= since)
        .unwrap_or(false)
}

/// Background poller: stamp `last_frame_at` when the bind TUN RX counter
/// advances; self-exits when the session leaves the active set.
pub(super) async fn peer_poll(session: Arc<Mutex<Option<BindSession>>>, iface: String) {
    let mut last: Option<u64> = None;
    loop {
        let active = matches!(session.lock().await.as_ref(), Some(s) if s.state.is_active());
        if !active {
            return;
        }
        if let Some(current) = iface::read_rx_packets_counter(&iface) {
            if let Some(prev) = last {
                if current != prev {
                    if let Some(s) = session.lock().await.as_mut() {
                        s.last_frame_at = Some(now_monotonic());
                    }
                }
            }
            last = Some(current);
        }
        tokio::time::sleep(TUNNEL_POLL_INTERVAL).await;
    }
}
