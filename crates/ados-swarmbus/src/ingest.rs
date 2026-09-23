//! Captured bytes to table effect: the receive path's whole decision, as one pure
//! function.
//!
//! Everything between "a frame came off the socket" and "the neighbour table
//! changed" lives here rather than in the platform socket code, so the entire
//! receive classification — foreign traffic, forged frames, our own loopback, a
//! real neighbour — is unit-testable on any host with no radio and no kernel filter.

use std::time::Instant;

use crate::beacon::SwarmBeacon;
use crate::crypto::{SealError, SenderNonce, SwarmCipher};
use crate::frame::{parse_frame, FrameReject, SwarmFrameKind};
use crate::neighbors::{NeighborTable, Recorded};

/// What one captured frame did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingest {
    /// An authenticated beacon, recorded into the table.
    Beacon(SwarmBeacon),
    /// An authenticated beacon deliberately not recorded: our own loopback, a
    /// beacon claiming the ground slot, a second sender on a peer's live slot, or
    /// a table already at its cap.
    BeaconIgnored(SwarmBeacon),
    /// Not ours, not authentic, or not fresh.
    Rejected(IngestReject),
}

/// Why a captured frame produced nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestReject {
    /// Truncated or an unwalkable radiotap header.
    Malformed,
    /// The receiver flagged the frame as failing its FCS: corrupted on the air.
    BadFcs,
    /// Another protocol on the shared adapter — in steady state, wfb-ng video that
    /// reached userspace, which means the kernel filter is not attached.
    ForeignMagic,
    /// Our magic, another fleet's id.
    ForeignFleet,
    /// The seal did not verify, was too short to carry one, or authenticated a
    /// version/kind we do not implement. Only [`SealError::BadTag`] is counted.
    Seal(SealError),
    /// Authenticated as a beacon, but the body was not a beacon.
    BadBeaconBody,
    /// An authentic frame its sender already delivered, or one from a sender run
    /// its slot has moved on from.
    Replayed,
}

/// Classify one captured frame and apply it to `table`, bumping at most one
/// counter.
///
/// The counter discipline matters more than it looks: `beacons_bad_magic` and
/// `beacons_bad_tag` are the two numbers a field diagnosis turns on. A nonzero
/// bad-magic count means the kernel filter is not doing its job and the whole video
/// stream is being copied to userspace; a nonzero bad-tag count means a node in
/// range holds a different fleet key. Conflating them, or counting a malformed,
/// corrupted or version-skewed frame as either, destroys both signals.
///
/// Our own transmissions come back on a monitor interface. They are recognised by
/// the cipher's per-process nonce prefix, not by slot, so a peer provisioned with
/// this node's slot is still heard (and flagged as a conflict by the table).
pub fn ingest_frame(
    buf: &[u8],
    fleet_id: u16,
    cipher: &SwarmCipher,
    table: &mut NeighborTable,
    now: Instant,
) -> Ingest {
    let captured = match parse_frame(buf, fleet_id) {
        Ok(c) => c,
        Err(FrameReject::Malformed) => return Ingest::Rejected(IngestReject::Malformed),
        // Corruption on the air is neither a filter fault nor a key mismatch.
        Err(FrameReject::BadFcs) => return Ingest::Rejected(IngestReject::BadFcs),
        Err(FrameReject::ForeignMagic) => {
            table.record_bad_magic();
            return Ingest::Rejected(IngestReject::ForeignMagic);
        }
        // Another fleet's frame is not a fault of ours and not a forgery: two
        // fleets sharing a channel is a supported configuration, so it is counted
        // as neither bad magic nor bad tag.
        Err(FrameReject::ForeignFleet) => return Ingest::Rejected(IngestReject::ForeignFleet),
    };

    let (kind, body) = match cipher.open(captured.payload) {
        Ok(v) => v,
        Err(e) => {
            // Only a failed tag says "a different fleet key". A payload too short
            // to carry a seal is malformed; an authenticated frame with a version
            // or kind this build does not implement is a version skew inside the
            // fleet, like a wrong-length beacon body. Neither is counted.
            if e == SealError::BadTag {
                table.record_bad_tag();
            }
            return Ingest::Rejected(IngestReject::Seal(e));
        }
    };
    // `open` accepted the payload, so it carries at least a nonce and the tag
    // verified under exactly this one.
    let sender = SenderNonce::from_wire(captured.payload)
        .expect("an opened payload is longer than its nonce");

    match kind {
        SwarmFrameKind::Beacon => match SwarmBeacon::decode(&body) {
            Some(beacon) if cipher.is_own(&sender) => Ingest::BeaconIgnored(beacon),
            Some(beacon) => match table.record(beacon, sender, captured.rssi_dbm, now) {
                Recorded::Accepted | Recorded::OwnSlotConflict => Ingest::Beacon(beacon),
                Recorded::Ignored | Recorded::SecondSender => Ingest::BeaconIgnored(beacon),
                Recorded::Replayed => Ingest::Rejected(IngestReject::Replayed),
            },
            // Authenticated by a fleet member but the wrong length: a version skew
            // inside one fleet, not an attack, so it is not a bad tag.
            None => Ingest::Rejected(IngestReject::BadBeaconBody),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::derive_fleet_key;
    use crate::frame::{
        build_frame, ieee80211_header, radiotap_header, IEEE80211_HDR_LEN, MAGIC_OFFSET,
        RT_F_BADFCS, RT_F_FCS, WFB_MAGIC,
    };

    const FLEET: u16 = 1;

    fn key() -> [u8; 32] {
        derive_fleet_key(Some(&[7u8; 64]))
    }

    /// This node's cipher: the one the receive path opens with.
    fn me() -> SwarmCipher {
        SwarmCipher::new(&key())
    }

    /// Another fleet member on the same key, with its own nonce prefix.
    fn peer() -> SwarmCipher {
        SwarmCipher::new(&key())
    }

    fn beacon(slot: u8) -> SwarmBeacon {
        SwarmBeacon {
            slot,
            lat: 129_716_000,
            lon: 775_946_000,
            ..SwarmBeacon::default()
        }
    }

    /// A frame as a peer would put it on the air.
    fn air(c: &SwarmCipher, kind: SwarmFrameKind, body: &[u8]) -> Vec<u8> {
        build_frame(FLEET, 0, &c.seal(kind, body))
    }

    /// IEEE 802.3 CRC-32, the 802.11 FCS algorithm, bit by bit.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for b in bytes {
            crc ^= *b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        crc ^ 0xFFFF_FFFF
    }

    /// A peer's beacon as a monitor-mode driver hands it up: a receive-side
    /// radiotap header of the driver's choosing (TSFT, FLAGS, RATE, CHANNEL,
    /// DBM_ANTSIGNAL) with FLAGS=`flags`, then the MPDU, then the 4-byte FCS the
    /// driver leaves attached.
    fn driver_capture(sealed: &[u8], flags: u8, rssi: i8) -> Vec<u8> {
        let present: u32 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 5);
        let mut rt = vec![0x00, 0x00, 0x00, 0x00];
        rt.extend_from_slice(&present.to_le_bytes());
        rt.extend_from_slice(&0x0123_4567_89AB_CDEFu64.to_le_bytes()); // TSFT @8
        rt.push(flags); // FLAGS @16
        rt.push(0x0C); // RATE @17
        rt.extend_from_slice(&5745u16.to_le_bytes()); // CHANNEL freq @18
        rt.extend_from_slice(&0x0140u16.to_le_bytes()); // CHANNEL flags @20
        rt.push(rssi as u8); // DBM_ANTSIGNAL @22
        let len = rt.len() as u16;
        rt[2..4].copy_from_slice(&len.to_le_bytes());

        let mut mpdu = ieee80211_header(FLEET, 0x0120).to_vec();
        mpdu.extend_from_slice(sealed);
        let fcs = crc32(&mpdu);
        let mut frame = rt;
        frame.extend_from_slice(&mpdu);
        frame.extend_from_slice(&fcs.to_le_bytes());
        frame
    }

    #[test]
    fn a_peers_beacon_is_recorded_and_counted_as_a_receipt() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let frame = air(&peer(), SwarmFrameKind::Beacon, &beacon(3).encode());

        assert_eq!(
            ingest_frame(&frame, FLEET, &me(), &mut table, t0),
            Ingest::Beacon(beacon(3))
        );
        assert_eq!(table.len(), 1);
        let counters = table.counters();
        assert_eq!(counters.beacons_rx, 1);
        assert_eq!(counters.beacons_bad_magic, 0);
        assert_eq!(counters.beacons_bad_tag, 0);
    }

    /// The receive shape a real monitor-mode adapter produces: FLAGS carries F_FCS
    /// and the MPDU still ends in its CRC. Read as payload, those four bytes sat
    /// after the Poly1305 tag and every peer beacon counted as a bad tag, which
    /// the diagnosis then blamed on a fleet-key mismatch.
    #[test]
    fn a_driver_capture_with_a_trailing_fcs_authenticates() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let sealed = peer().seal(SwarmFrameKind::Beacon, &beacon(3).encode());
        let frame = driver_capture(&sealed, RT_F_FCS, -57);

        assert_eq!(
            ingest_frame(&frame, FLEET, &me(), &mut table, t0),
            Ingest::Beacon(beacon(3))
        );
        assert_eq!(table.counters().beacons_bad_tag, 0);
        assert_eq!(table.counters().beacons_rx, 1);
        assert_eq!(table.get(3).unwrap().rssi_dbm, Some(-57));
    }

    /// A frame the receiver flagged as failing its FCS is corrupt: it is dropped
    /// without moving any counter, rather than inflating the bad-tag count that
    /// reads as a key mismatch.
    #[test]
    fn a_bad_fcs_capture_is_dropped_without_counting_a_fault() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let sealed = peer().seal(SwarmFrameKind::Beacon, &beacon(3).encode());
        let frame = driver_capture(&sealed, RT_F_FCS | RT_F_BADFCS, -80);

        assert_eq!(
            ingest_frame(&frame, FLEET, &me(), &mut table, t0),
            Ingest::Rejected(IngestReject::BadFcs)
        );
        assert_eq!(table.counters(), Default::default());
        assert!(table.is_empty());
    }

    /// wfb-ng video that reaches userspace must be counted as bad magic and nothing
    /// else. That count is the only signal that the kernel filter is not attached.
    #[test]
    fn a_foreign_magic_is_counted_as_bad_magic_and_never_as_a_bad_tag() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);

        let mut frame = radiotap_header(1).to_vec();
        let mut mac = ieee80211_header(FLEET, 0);
        mac[MAGIC_OFFSET..MAGIC_OFFSET + 2].copy_from_slice(&WFB_MAGIC.to_be_bytes());
        frame.extend_from_slice(&mac);
        frame.extend_from_slice(&[0u8; 64]);

        assert_eq!(
            ingest_frame(&frame, FLEET, &me(), &mut table, t0),
            Ingest::Rejected(IngestReject::ForeignMagic)
        );
        let counters = table.counters();
        assert_eq!(counters.beacons_bad_magic, 1);
        assert_eq!(counters.beacons_bad_tag, 0, "not a forgery");
        assert_eq!(counters.beacons_rx, 0);
        assert!(table.is_empty());
    }

    /// A tampered frame must be counted as a bad tag — the number that says a node
    /// in range holds a different fleet key. Sweeping the payload proves no byte
    /// slips through unauthenticated.
    #[test]
    fn a_tampered_frame_is_counted_as_a_bad_tag_at_every_payload_offset() {
        let t0 = Instant::now();
        let c = me();
        let good = air(&peer(), SwarmFrameKind::Beacon, &beacon(3).encode());
        let payload_start = good.len() - 50;

        let mut table = NeighborTable::new(1);
        for i in payload_start..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x01;
            let got = ingest_frame(&bad, FLEET, &c, &mut table, t0);
            assert_eq!(
                got,
                Ingest::Rejected(IngestReject::Seal(SealError::BadTag)),
                "a flipped bit at offset {i} must be rejected"
            );
        }
        let counters = table.counters();
        assert_eq!(
            counters.beacons_bad_tag as usize,
            good.len() - payload_start
        );
        assert_eq!(counters.beacons_bad_magic, 0, "the header was untouched");
        assert_eq!(counters.beacons_rx, 0);
        assert!(table.is_empty(), "no forgery reached the table");
    }

    /// A frame sealed under another fleet's key is a bad tag; a frame ADDRESSED to
    /// another fleet is neither. Two fleets sharing a channel is supported, so their
    /// traffic must not inflate a diagnostic counter.
    #[test]
    fn another_fleets_addressing_is_not_counted_as_a_fault() {
        let t0 = Instant::now();
        let c = me();
        let mut table = NeighborTable::new(1);

        // Correct magic, wrong fleet in the header.
        let other = build_frame(
            2,
            0,
            &peer().seal(SwarmFrameKind::Beacon, &beacon(3).encode()),
        );
        assert_eq!(
            ingest_frame(&other, FLEET, &c, &mut table, t0),
            Ingest::Rejected(IngestReject::ForeignFleet)
        );
        assert_eq!(table.counters(), Default::default(), "no counter moved");

        // Our fleet's addressing, another fleet's key: that IS a tag failure.
        let foreign_key = SwarmCipher::new(&derive_fleet_key(Some(&[9u8; 64])));
        let forged = air(&foreign_key, SwarmFrameKind::Beacon, &beacon(3).encode());
        assert_eq!(
            ingest_frame(&forged, FLEET, &c, &mut table, t0),
            Ingest::Rejected(IngestReject::Seal(SealError::BadTag))
        );
        assert_eq!(table.counters().beacons_bad_tag, 1);
    }

    /// A node hears its own injected frames on a monitor interface. They are
    /// recognised by this cipher's nonce prefix, whatever slot they carry, and
    /// never recorded: that would make every drone its own nearest neighbour.
    #[test]
    fn our_own_loopback_authenticates_but_is_not_recorded() {
        let t0 = Instant::now();
        let c = me();
        let mut table = NeighborTable::new(3);
        let frame = air(&c, SwarmFrameKind::Beacon, &beacon(3).encode());

        assert_eq!(
            ingest_frame(&frame, FLEET, &c, &mut table, t0),
            Ingest::BeaconIgnored(beacon(3)),
            "authentic, but ours"
        );
        assert!(table.is_empty());
        // It still counts as nothing: an ignored own-beacon is not a receipt and not
        // a fault.
        assert_eq!(table.counters(), Default::default());
    }

    /// Two drones provisioned with one slot used to drop each other's beacons as
    /// loopback, so neither separation layer knew the other existed. A foreign
    /// sender on our slot is now recorded and flagged.
    #[test]
    fn a_peer_on_our_own_slot_is_heard_and_flagged() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(5);
        let frame = air(&peer(), SwarmFrameKind::Beacon, &beacon(5).encode());

        assert_eq!(
            ingest_frame(&frame, FLEET, &me(), &mut table, t0),
            Ingest::Beacon(beacon(5))
        );
        assert!(table.get(5).is_some(), "separation sees the other aircraft");
        assert!(table.slot_conflict());
        assert_eq!(table.counters().beacons_slot_conflict, 1);
    }

    /// A captured beacon re-injected later authenticates, and used to overwrite
    /// the slot as just received. It is now refused and counted, and the entry
    /// keeps its real receipt time.
    #[test]
    fn a_replayed_frame_is_refused_and_counted() {
        let t0 = Instant::now();
        let c = me();
        let mut table = NeighborTable::new(1);
        let frame = air(&peer(), SwarmFrameKind::Beacon, &beacon(3).encode());

        assert!(matches!(
            ingest_frame(&frame, FLEET, &c, &mut table, t0),
            Ingest::Beacon(_)
        ));
        let later = t0 + std::time::Duration::from_millis(400);
        assert_eq!(
            ingest_frame(&frame, FLEET, &c, &mut table, later),
            Ingest::Rejected(IngestReject::Replayed)
        );
        assert_eq!(table.get(3).unwrap().received_at, t0);
        assert_eq!(table.counters().beacons_replayed, 1);
        assert_eq!(table.counters().beacons_rx, 1);
        assert_eq!(table.counters().beacons_bad_tag, 0, "it is authentic");
    }

    /// A fleet member on a newer agent may seal with a newer wire version or a
    /// frame kind this build does not implement. The frame authenticates, so it
    /// is a version skew, and it must not read as a fleet-key mismatch.
    #[test]
    fn an_authenticated_frame_of_an_unknown_version_or_kind_is_not_a_bad_tag() {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let raw = ChaCha20Poly1305::new(Key::from_slice(&key()));
        let seal_with_header = |header: [u8; 2], counter: u8| {
            let nonce = [9, 9, 9, 9, 9, 9, 9, 9, counter, 0, 0, 0];
            let mut plaintext = header.to_vec();
            plaintext.extend_from_slice(&beacon(3).encode());
            let sealed = raw
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &plaintext,
                        aad: &[],
                    },
                )
                .unwrap();
            let mut wire = nonce.to_vec();
            wire.extend_from_slice(&sealed);
            build_frame(FLEET, 0, &wire)
        };

        let newer_version = seal_with_header([2, SwarmFrameKind::Beacon as u8], 0);
        assert_eq!(
            ingest_frame(&newer_version, FLEET, &me(), &mut table, t0),
            Ingest::Rejected(IngestReject::Seal(SealError::BadVersion(2)))
        );
        let unknown_kind = seal_with_header([crate::crypto::SWARM_WIRE_VERSION, 2], 1);
        assert_eq!(
            ingest_frame(&unknown_kind, FLEET, &me(), &mut table, t0),
            Ingest::Rejected(IngestReject::Seal(SealError::UnknownKind(2)))
        );
        assert_eq!(table.counters(), Default::default(), "no counter moved");
        assert!(table.is_empty());
    }

    /// A fleet member on a newer agent could seal a beacon body of a different
    /// length. That is a version skew inside one fleet, not an attack, so it must
    /// not inflate the forgery counter that a field diagnosis reads.
    #[test]
    fn an_authenticated_body_of_the_wrong_length_is_not_a_forgery() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let frame = air(&peer(), SwarmFrameKind::Beacon, &[0u8; 24]);
        assert_eq!(
            ingest_frame(&frame, FLEET, &me(), &mut table, t0),
            Ingest::Rejected(IngestReject::BadBeaconBody)
        );
        assert_eq!(table.counters().beacons_bad_tag, 0);
        assert_eq!(table.counters().beacons_rx, 0);
        assert!(table.is_empty());
    }

    /// Garbage off the socket must not panic and must not be misattributed.
    #[test]
    fn malformed_captures_are_rejected_without_moving_any_counter() {
        let t0 = Instant::now();
        let c = me();
        let mut table = NeighborTable::new(1);
        let good = air(&peer(), SwarmFrameKind::Beacon, &beacon(3).encode());
        for cut in 0..good.len().min(37) {
            assert_eq!(
                ingest_frame(&good[..cut], FLEET, &c, &mut table, t0),
                Ingest::Rejected(IngestReject::Malformed)
            );
        }
        assert_eq!(table.counters(), Default::default());
        // A frame with our header but no payload at all fails the seal length
        // gate. It is malformed, not evidence of a different fleet key.
        let headers = build_frame(FLEET, 0, &[]);
        assert_eq!(headers.len(), 37);
        assert!(matches!(
            ingest_frame(&headers, FLEET, &c, &mut table, t0),
            Ingest::Rejected(IngestReject::Seal(SealError::TooShort))
        ));
        assert_eq!(table.counters(), Default::default());
    }

    /// The signal reading has to survive from the radiotap header all the way into
    /// the table entry, since it is what the operator's RSSI column renders.
    #[test]
    fn the_signal_reading_reaches_the_table_entry() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);

        // A capture whose radiotap carries DBM_ANTSIGNAL.
        let sealed = peer().seal(SwarmFrameKind::Beacon, &beacon(4).encode());
        let mut frame = vec![0x00, 0x00, 9, 0x00];
        frame.extend_from_slice(&(1u32 << 5).to_le_bytes());
        frame.push((-52i8) as u8);
        frame.extend_from_slice(&ieee80211_header(FLEET, 0));
        frame.extend_from_slice(&sealed);
        assert_eq!(frame.len(), 9 + IEEE80211_HDR_LEN + sealed.len());

        assert!(matches!(
            ingest_frame(&frame, FLEET, &me(), &mut table, t0),
            Ingest::Beacon(_)
        ));
        assert_eq!(table.get(4).unwrap().rssi_dbm, Some(-52));
    }

    /// The end-to-end property the bus exists for: a peer's beacon becomes a
    /// neighbour carrying its reported velocity, and goes away on its own after the
    /// stale window.
    #[test]
    fn a_received_beacon_becomes_a_neighbour_then_expires() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        let mut b = beacon(6);
        b.vx_cms = 500; // 5 m/s north
        let frame = air(&peer(), SwarmFrameKind::Beacon, &b.encode());

        assert!(matches!(
            ingest_frame(&frame, FLEET, &me(), &mut table, t0),
            Ingest::Beacon(_)
        ));
        assert_eq!(table.get(6).unwrap().beacon, b);

        assert_eq!(table.prune(t0 + crate::NEIGHBOR_STALE), 1);
        assert!(table.get(6).is_none());
        assert_eq!(table.counters().beacons_stale_dropped, 1);
    }
}
