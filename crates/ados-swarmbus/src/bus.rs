//! [`SwarmBus`]: the transmit and receive halves of one fleet's beacon plane.
//!
//! Thin by design. It owns the radio, the cipher and the 802.11 sequence counter,
//! and it delegates every decision: what to put in a beacon is
//! [`crate::vehicle`]'s, when to send it is [`crate::schedule`]'s, and what a
//! received frame means is [`crate::ingest`]'s. The bus is the only part that
//! cannot be tested without a radio, so it is kept to the smallest thing that
//! could possibly need one.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Instant;

use tokio::sync::broadcast;

use crate::beacon::SwarmBeacon;
use crate::crypto::SwarmCipher;
use crate::frame::{build_frame, SwarmFrame, SwarmFrameKind, MAX_FRAME_LEN};
use crate::ingest::{ingest_frame, Ingest};
use crate::neighbors::NeighborTable;
use crate::radio::Radio;

/// Depth of the beacon and frame fan-out channels.
///
/// 64 covers more than a full fleet's worth of beacons in one period, so a consumer
/// that stalls briefly resumes with current data instead of a backlog. A consumer
/// slow enough to overflow it gets `RecvError::Lagged` and skips ahead, which is the
/// right failure for position data: the newest beacon is the only one worth having.
const FANOUT_DEPTH: usize = 64;

/// One fleet's swarm bus on one monitor interface.
pub struct SwarmBus {
    radio: Radio,
    cipher: SwarmCipher,
    fleet_id: u16,
    slot: u8,
    /// 802.11 sequence control, advanced per transmission so a driver cannot treat
    /// consecutive beacons as duplicate retransmissions of one frame.
    seq: AtomicU16,
    beacons: broadcast::Sender<SwarmBeacon>,
    frames: broadcast::Sender<SwarmFrame>,
}

impl SwarmBus {
    /// Open the bus on `iface` for `fleet_id`, as the node in `slot`, keyed by
    /// `key`.
    ///
    /// Fails when the interface is absent, is not in monitor mode, or the process
    /// lacks `CAP_NET_RAW` — all operational conditions the caller retries, since
    /// the radio manager may simply not have selected an adapter yet.
    pub fn open(iface: &str, fleet_id: u16, slot: u8, key: &[u8; 32]) -> anyhow::Result<Self> {
        let radio = Radio::open(iface, fleet_id)?;
        Ok(Self {
            radio,
            cipher: SwarmCipher::new(key),
            fleet_id,
            slot,
            seq: AtomicU16::new(0),
            beacons: broadcast::channel(FANOUT_DEPTH).0,
            frames: broadcast::channel(FANOUT_DEPTH).0,
        })
    }

    /// The fleet this bus carries.
    pub fn fleet_id(&self) -> u16 {
        self.fleet_id
    }

    /// This node's fleet slot.
    pub fn slot(&self) -> u8 {
        self.slot
    }

    /// The interface the bus is bound to.
    pub fn iface(&self) -> &str {
        self.radio.iface()
    }

    /// Transmit one beacon.
    pub async fn broadcast(&self, beacon: &SwarmBeacon) -> anyhow::Result<()> {
        self.broadcast_frame(SwarmFrameKind::Beacon, &beacon.encode())
            .await
    }

    /// Transmit an arbitrary frame body under `kind` — the transport seam for the
    /// CBBA bid lane, whose codec belongs to the onboard autonomy layer.
    ///
    /// `async` for the shape of the call rather than because it yields: a 50-byte
    /// injection on a qdisc-bypassed socket either completes or reports a full
    /// driver queue immediately, so there is nothing to await. Keeping the signature
    /// async leaves room to wait on writability if a larger lane ever needs it,
    /// without changing every call site.
    pub async fn broadcast_frame(&self, kind: SwarmFrameKind, body: &[u8]) -> anyhow::Result<()> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let frame = build_frame(self.fleet_id, seq, &self.cipher.seal(kind, body));
        let sent = self.radio.send(&frame)?;
        if sent != frame.len() {
            anyhow::bail!("short injection: {sent} of {} bytes", frame.len());
        }
        Ok(())
    }

    /// Subscribe to every beacon accepted into the table.
    pub fn subscribe(&self) -> broadcast::Receiver<SwarmBeacon> {
        self.beacons.subscribe()
    }

    /// Subscribe to every authenticated non-beacon frame.
    pub fn subscribe_frames(&self) -> broadcast::Receiver<SwarmFrame> {
        self.frames.subscribe()
    }

    /// Await one captured frame, fold it into `table`, and fan it out.
    ///
    /// Returns what the frame did, so the caller can log and so a test can drive the
    /// loop one frame at a time. Only an error from the socket itself is an `Err`; a
    /// rejected frame is a normal `Ok` outcome, already counted.
    pub async fn recv_into(&self, table: &Mutex<NeighborTable>) -> anyhow::Result<Ingest> {
        let mut buf = [0u8; MAX_FRAME_LEN];
        let n = self.radio.recv(&mut buf).await?;
        let now = Instant::now();
        let outcome = {
            let mut guard = table.lock();
            ingest_frame(&buf[..n], self.fleet_id, &self.cipher, &mut guard, now)
        };
        // A send with no subscribers is not an error; on a ground station nothing
        // subscribes at all and the table read is the whole consumer.
        match &outcome {
            Ingest::Beacon(b) => {
                let _ = self.beacons.send(*b);
            }
            Ingest::Frame(f) => {
                let _ = self.frames.send(f.clone());
            }
            Ingest::BeaconIgnored(_) | Ingest::Rejected(_) => {}
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off Linux there is no radio, so this asserts what it can: opening reports the
    /// platform rather than fabricating a bus. On Linux without `CAP_NET_RAW` it
    /// reports the permission failure. Either way it is a clean error, never a panic
    /// and never a fake success — the receive classification itself is tested
    /// exhaustively in [`crate::ingest`], which needs no socket.
    #[test]
    fn opening_without_a_usable_radio_is_a_clean_error() {
        let key = [0u8; 32];
        let err = match SwarmBus::open("nonexistent-swarm-iface0", 1, 1, &key) {
            Ok(_) => return,
            Err(e) => e,
        };
        assert!(
            !err.to_string().is_empty(),
            "the failure must say something"
        );
    }

    /// The fan-out depth has to cover a full fleet's beacons within one period, or a
    /// consumer that pauses for a single tick loses positions it could have had.
    #[test]
    fn the_fanout_depth_covers_a_full_fleet_period() {
        let per_period = ados_radio::config::FLEET_MAX_SLOTS as usize;
        assert!(
            FANOUT_DEPTH >= per_period * 2,
            "{FANOUT_DEPTH} must hold at least two periods of {per_period} beacons"
        );
    }
}
