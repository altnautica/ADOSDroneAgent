//! Captured bytes to table effect: the receive path's whole decision, as one pure
//! function.
//!
//! Everything between "a frame came off the socket" and "the neighbour table
//! changed" lives here rather than in the platform socket code, so the entire
//! receive classification — foreign traffic, forged frames, our own loopback, a
//! real neighbour — is unit-testable on any host with no radio and no kernel filter.

use std::time::Instant;

use crate::beacon::SwarmBeacon;
use crate::crypto::{SealError, SwarmCipher};
use crate::frame::{parse_frame, FrameReject, SwarmFrame, SwarmFrameKind};
use crate::neighbors::NeighborTable;

/// What one captured frame did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingest {
    /// An authenticated beacon, recorded into the table.
    Beacon(SwarmBeacon),
    /// An authenticated beacon deliberately not recorded: our own loopback, a
    /// beacon claiming the ground slot, or a table already at its cap.
    BeaconIgnored(SwarmBeacon),
    /// An authenticated frame belonging to another layer (the CBBA bid lane).
    Frame(SwarmFrame),
    /// Not ours, or not authentic.
    Rejected(IngestReject),
}

/// Why a captured frame produced nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestReject {
    /// Truncated or an unwalkable radiotap header.
    Malformed,
    /// Another protocol on the shared adapter — in steady state, wfb-ng video that
    /// reached userspace, which means the kernel filter is not attached.
    ForeignMagic,
    /// Our magic, another fleet's id.
    ForeignFleet,
    /// The seal did not verify or carried a version/kind we do not implement.
    Seal(SealError),
    /// Authenticated as a beacon, but the body was not a beacon.
    BadBeaconBody,
}

/// Classify one captured frame and apply it to `table`, bumping exactly one
/// counter.
///
/// The counter discipline matters more than it looks: `beacons_bad_magic` and
/// `beacons_bad_tag` are the two numbers a field diagnosis turns on. A nonzero
/// bad-magic count means the kernel filter is not doing its job and the whole video
/// stream is being copied to userspace; a nonzero bad-tag count means a node in
/// range holds a different fleet key. Conflating them, or counting a malformed frame
/// as either, destroys both signals.
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
            table.record_bad_tag();
            return Ingest::Rejected(IngestReject::Seal(e));
        }
    };

    match kind {
        SwarmFrameKind::Beacon => match SwarmBeacon::decode(&body) {
            Some(beacon) => {
                if table.record(beacon, captured.rssi_dbm, now) {
                    Ingest::Beacon(beacon)
                } else {
                    Ingest::BeaconIgnored(beacon)
                }
            }
            // Authenticated by a fleet member but the wrong length: a version skew
            // inside one fleet, not an attack, so it is not a bad tag.
            None => Ingest::Rejected(IngestReject::BadBeaconBody),
        },
        SwarmFrameKind::CbbaBid => Ingest::Frame(SwarmFrame {
            kind,
            body,
            rssi_dbm: captured.rssi_dbm,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::derive_fleet_key;
    use crate::frame::{
        build_frame, ieee80211_header, radiotap_header, IEEE80211_HDR_LEN, MAGIC_OFFSET, WFB_MAGIC,
    };

    const FLEET: u16 = 1;

    fn cipher() -> SwarmCipher {
        SwarmCipher::new(&derive_fleet_key(Some(&[7u8; 64])))
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

    #[test]
    fn a_peers_beacon_is_recorded_and_counted_as_a_receipt() {
        let t0 = Instant::now();
        let c = cipher();
        let mut table = NeighborTable::new(1);
        let frame = air(&c, SwarmFrameKind::Beacon, &beacon(3).encode());

        assert_eq!(
            ingest_frame(&frame, FLEET, &c, &mut table, t0),
            Ingest::Beacon(beacon(3))
        );
        assert_eq!(table.len(), 1);
        let counters = table.counters();
        assert_eq!(counters.beacons_rx, 1);
        assert_eq!(counters.beacons_bad_magic, 0);
        assert_eq!(counters.beacons_bad_tag, 0);
    }

    /// wfb-ng video that reaches userspace must be counted as bad magic and nothing
    /// else. That count is the only signal that the kernel filter is not attached.
    #[test]
    fn a_foreign_magic_is_counted_as_bad_magic_and_never_as_a_bad_tag() {
        let t0 = Instant::now();
        let c = cipher();
        let mut table = NeighborTable::new(1);

        let mut frame = radiotap_header(1).to_vec();
        let mut mac = ieee80211_header(FLEET, 0);
        mac[MAGIC_OFFSET..MAGIC_OFFSET + 2].copy_from_slice(&WFB_MAGIC.to_be_bytes());
        frame.extend_from_slice(&mac);
        frame.extend_from_slice(&[0u8; 64]);

        assert_eq!(
            ingest_frame(&frame, FLEET, &c, &mut table, t0),
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
        let c = cipher();
        let good = air(&c, SwarmFrameKind::Beacon, &beacon(3).encode());
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
        let c = cipher();
        let mut table = NeighborTable::new(1);

        // Correct magic, wrong fleet in the header.
        let other = build_frame(2, 0, &c.seal(SwarmFrameKind::Beacon, &beacon(3).encode()));
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

    /// A node hears its own injected frames on a monitor interface. Recording them
    /// would make every drone its own nearest neighbour at zero distance.
    #[test]
    fn our_own_loopback_authenticates_but_is_not_recorded() {
        let t0 = Instant::now();
        let c = cipher();
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

    #[test]
    fn a_bid_frame_is_handed_out_without_touching_the_table() {
        let t0 = Instant::now();
        let c = cipher();
        let mut table = NeighborTable::new(1);
        let bid: Vec<u8> = (0..40u8).collect();
        let frame = air(&c, SwarmFrameKind::CbbaBid, &bid);

        assert_eq!(
            ingest_frame(&frame, FLEET, &c, &mut table, t0),
            Ingest::Frame(SwarmFrame {
                kind: SwarmFrameKind::CbbaBid,
                body: bid,
                rssi_dbm: None,
            })
        );
        assert!(table.is_empty());
        assert_eq!(table.counters().beacons_rx, 0, "a bid is not a beacon");
    }

    /// A fleet member on a newer agent could seal a beacon body of a different
    /// length. That is a version skew inside one fleet, not an attack, so it must
    /// not inflate the forgery counter that a field diagnosis reads.
    #[test]
    fn an_authenticated_body_of_the_wrong_length_is_not_a_forgery() {
        let t0 = Instant::now();
        let c = cipher();
        let mut table = NeighborTable::new(1);
        let frame = air(&c, SwarmFrameKind::Beacon, &[0u8; 24]);
        assert_eq!(
            ingest_frame(&frame, FLEET, &c, &mut table, t0),
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
        let c = cipher();
        let mut table = NeighborTable::new(1);
        let good = air(&c, SwarmFrameKind::Beacon, &beacon(3).encode());
        for cut in 0..good.len().min(37) {
            assert_eq!(
                ingest_frame(&good[..cut], FLEET, &c, &mut table, t0),
                Ingest::Rejected(IngestReject::Malformed)
            );
        }
        assert_eq!(table.counters(), Default::default());
        // A frame with our header but no payload at all fails the seal length gate.
        let headers = build_frame(FLEET, 0, &[]);
        assert_eq!(headers.len(), 37);
        assert!(matches!(
            ingest_frame(&headers, FLEET, &c, &mut table, t0),
            Ingest::Rejected(IngestReject::Seal(SealError::TooShort))
        ));
        assert_eq!(table.counters().beacons_bad_tag, 1);
    }

    /// The signal reading has to survive from the radiotap header all the way into
    /// the table entry, since it is what the operator's RSSI column renders.
    #[test]
    fn the_signal_reading_reaches_the_table_entry() {
        let t0 = Instant::now();
        let c = cipher();
        let mut table = NeighborTable::new(1);

        // A capture whose radiotap carries DBM_ANTSIGNAL.
        let sealed = c.seal(SwarmFrameKind::Beacon, &beacon(4).encode());
        let mut frame = vec![0x00, 0x00, 9, 0x00];
        frame.extend_from_slice(&(1u32 << 5).to_le_bytes());
        frame.push((-52i8) as u8);
        frame.extend_from_slice(&ieee80211_header(FLEET, 0));
        frame.extend_from_slice(&sealed);
        assert_eq!(frame.len(), 9 + IEEE80211_HDR_LEN + sealed.len());

        assert!(matches!(
            ingest_frame(&frame, FLEET, &c, &mut table, t0),
            Ingest::Beacon(_)
        ));
        assert_eq!(table.get(4).unwrap().rssi_dbm, Some(-52));
    }

    /// The end-to-end property the bus exists for: a peer's beacon becomes a
    /// dead-reckonable neighbour, and goes away on its own after the stale window.
    #[test]
    fn a_received_beacon_becomes_a_predictable_neighbour_then_expires() {
        let t0 = Instant::now();
        let c = cipher();
        let mut table = NeighborTable::new(1);
        let mut b = beacon(6);
        b.vx_cms = 500; // 5 m/s north
        let frame = air(&c, SwarmFrameKind::Beacon, &b.encode());

        assert!(matches!(
            ingest_frame(&frame, FLEET, &c, &mut table, t0),
            Ingest::Beacon(_)
        ));
        let (lat, _, _) = table
            .predicted(6, t0 + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(lat > b.lat_deg(), "it moved north");

        assert_eq!(table.prune(t0 + crate::NEIGHBOR_STALE), 1);
        assert!(table.predicted(6, t0 + crate::NEIGHBOR_STALE).is_none());
        assert_eq!(table.counters().beacons_stale_dropped, 1);
    }
}
