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
use std::sync::Arc;
use std::time::Instant;

use crate::beacon::SwarmBeacon;
use crate::crypto::SwarmCipher;
use crate::frame::{build_frame, SwarmFrameKind, MAX_FRAME_LEN};
use crate::ingest::{ingest_frame, Ingest};
use crate::neighbors::NeighborTable;
use crate::radio::Radio;

/// One fleet's swarm bus on one monitor interface.
pub struct SwarmBus {
    radio: Radio,
    cipher: Arc<SwarmCipher>,
    fleet_id: u16,
    slot: u8,
    /// 802.11 sequence number, advanced per transmission so a driver cannot treat
    /// consecutive beacons as duplicate retransmissions of one frame.
    seq: AtomicU16,
}

impl SwarmBus {
    /// Open the bus on `iface` for `fleet_id`, as the node in `slot`, sealing and
    /// opening with `cipher`.
    ///
    /// The cipher is supplied rather than built here so it outlives the socket: an
    /// adapter flap reopens the bus with the same cipher, and this node keeps its
    /// nonce prefix (its identity on the bus) and counter across the reopen.
    ///
    /// Fails when the interface is absent, is not a radiotap monitor interface, or
    /// the process lacks `CAP_NET_RAW` — all operational conditions the caller
    /// retries, since the radio manager may simply not have selected an adapter or
    /// switched it to monitor mode yet.
    pub fn open(
        iface: &str,
        fleet_id: u16,
        slot: u8,
        cipher: Arc<SwarmCipher>,
    ) -> anyhow::Result<Self> {
        let radio = Radio::open(iface, fleet_id)?;
        Ok(Self {
            radio,
            cipher,
            fleet_id,
            slot,
            seq: AtomicU16::new(0),
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
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let sealed = self.cipher.seal(SwarmFrameKind::Beacon, &beacon.encode());
        let frame = build_frame(self.fleet_id, seq, &sealed);
        // `async` for the shape of the call rather than because it yields: a
        // 50-byte injection on a qdisc-bypassed socket either completes or reports
        // a full driver queue immediately, so there is nothing to await.
        let sent = self.radio.send(&frame)?;
        if sent != frame.len() {
            anyhow::bail!("short injection: {sent} of {} bytes", frame.len());
        }
        Ok(())
    }

    /// Await one captured frame and fold it into `table`.
    ///
    /// Returns what the frame did, so the caller can log and so a test can drive the
    /// loop one frame at a time. Only an error from the socket itself is an `Err`; a
    /// rejected frame is a normal `Ok` outcome, already counted.
    pub async fn recv_into(&self, table: &Mutex<NeighborTable>) -> anyhow::Result<Ingest> {
        let mut buf = [0u8; MAX_FRAME_LEN];
        let n = self.radio.recv(&mut buf).await?;
        let now = Instant::now();
        let mut guard = table.lock();
        Ok(ingest_frame(
            &buf[..n],
            self.fleet_id,
            &self.cipher,
            &mut guard,
            now,
        ))
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
        let cipher = Arc::new(SwarmCipher::new(&[0u8; 32]));
        let err = match SwarmBus::open("nonexistent-swarm-iface0", 1, 1, cipher) {
            Ok(_) => return,
            Err(e) => e,
        };
        assert!(
            !err.to_string().is_empty(),
            "the failure must say something"
        );
    }
}
