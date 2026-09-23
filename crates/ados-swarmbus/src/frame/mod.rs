//! The 802.11 frame the swarm bus rides in.
//!
//! ## Why this is not another wfb-ng radio port
//!
//! Two facts from the vendored wfb-ng receiver define the whole design.
//!
//! 1. **N transmitters on one channel is fine; N transmitters on one
//!    `channel_id` is not.** `rx.cpp`'s `Aggregator` holds one `session_key` and
//!    re-inits its FEC decoder on any foreign session packet
//!    (`vendor/wfb-ng/src/rx.cpp:686-696`). Session packets re-announce about
//!    once a second, so two senders sharing a `channel_id` thrash each other's
//!    decoder continuously — which presents as unexplained link loss, not as an
//!    obvious error.
//! 2. **A per-instance kernel BPF makes one promiscuous adapter serve many
//!    logical streams.** `rx.cpp:70,84` opens a promiscuous, non-exclusive pcap
//!    handle and compiles `ether[0x0a:2]==0x5742 && ether[0x0c:4]==<channel_id>`
//!    onto it, so each `wfb_rx` sees only its own stream.
//!
//! Giving every drone its own beacon `channel_id` would satisfy (1) but cost 24
//! more receiver instances — 24 pcap handles and 24 kernel ring buffers — on
//! *every* aircraft. So the swarm bus keeps fact (2) and drops the wfb-ng session
//! model entirely: **one** filtered socket per node, our own magic in the same
//! header position wfb-ng's filter reads, and sender demultiplexing in userspace
//! by fleet slot. With no session key there is no decoder to thrash, so N
//! transmitters on one logical bus is legal by construction.
//!
//! ## The frame
//!
//! ```text
//! radiotap header      13 B   injection parameters (stripped before transmission)
//! ieee80211 header     24 B   data frame, broadcast receiver, addressing below
//!   bytes 10..12              SWARM_MAGIC (0xAD03)   <- wfb-ng's filter wants 0x5742
//!   bytes 12..16              fleet_id as u32 BE     <- one bus per fleet
//! payload             50 B   nonce + sealed(version, kind, beacon) + tag
//! ```
//!
//! The magic sits exactly where wfb-ng puts its own, so the two filters are
//! mutually exclusive on one byte pair and neither bus ever sees the other's
//! traffic. Like wfb-ng's `0x57`, `0xAD` has its low bit set, marking the address
//! multicast and locally administered — which is what a broadcast beacon is.

mod bpf;
mod radiotap;

pub use bpf::{bpf_program, SockFilter, BPF_PROGRAM_LEN};
pub use radiotap::{
    radiotap_flags, radiotap_header, radiotap_rssi, BEACON_MCS_INDEX, FCS_LEN, RADIOTAP_TX_LEN,
    RT_F_BADFCS, RT_F_FCS,
};

/// Our magic, in the two bytes of the transmitter address that wfb-ng fills with
/// `0x5742`. The two differ in the first byte, so the two kernel filters diverge
/// on a single 16-bit compare.
pub const SWARM_MAGIC: u16 = 0xAD03;

/// wfb-ng's magic, asserted against in a test so a future edit cannot make the
/// two buses collide.
pub const WFB_MAGIC: u16 = 0x5742;

/// Offset of the magic within the 802.11 MAC header (the transmitter address's
/// first two octets). `ether[0x0a:2]` in BPF terms.
pub const MAGIC_OFFSET: usize = 0x0a;

/// Offset of the fleet discriminator within the 802.11 MAC header (the remaining
/// four octets of the transmitter address). `ether[0x0c:4]` in BPF terms.
pub const FLEET_OFFSET: usize = 0x0c;

/// The 802.11 MAC header length: frame control, duration, three addresses, and
/// the sequence-control field. No QoS control — this is a plain data frame.
pub const IEEE80211_HDR_LEN: usize = 24;

/// Bytes prepended to the payload on injection.
pub const FRAME_HEADER_LEN: usize = RADIOTAP_TX_LEN + IEEE80211_HDR_LEN;

/// The largest frame the receive path accepts off the socket.
///
/// A beacon frame is 87 bytes injected. The ceiling bounds a single read against
/// a corrupt or hostile length.
pub const MAX_FRAME_LEN: usize = 2048;

/// Frame kinds carried in the sealed payload's second byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SwarmFrameKind {
    /// A [`crate::SwarmBeacon`]: the periodic 2 Hz cooperative-awareness message.
    Beacon = 1,
}

impl SwarmFrameKind {
    /// Decode the wire byte. An unknown kind is `None`: a newer agent's frame is
    /// dropped rather than mis-dispatched into the beacon decoder.
    pub const fn from_wire(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Beacon),
            _ => None,
        }
    }
}

/// Why a captured frame was not ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameReject {
    /// Truncated, or a radiotap header whose declared length runs past the
    /// capture.
    Malformed,
    /// The magic in `ether[0x0a:2]` is not [`SWARM_MAGIC`] — wfb-ng traffic, or
    /// any other frame on the channel.
    ForeignMagic,
    /// Our magic, another fleet's id. Two fleets may legitimately share one
    /// channel, and this is how they stay separate.
    ForeignFleet,
    /// The receiver's radiotap FLAGS marked the frame as failing its FCS check:
    /// corrupted on the air, so none of its bytes are read.
    BadFcs,
}

/// One captured frame, split into what the cipher needs and what the neighbour
/// table needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturedFrame<'a> {
    /// The sealed payload (nonce, ciphertext, tag) following the 802.11 header.
    pub payload: &'a [u8],
    /// Radiotap antenna signal in dBm when the capture carried one.
    pub rssi_dbm: Option<i8>,
}

/// Assemble a complete injectable frame: radiotap, 802.11 header, then `payload`.
pub fn build_frame(fleet_id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.extend_from_slice(&radiotap_header(BEACON_MCS_INDEX));
    out.extend_from_slice(&ieee80211_header(fleet_id, seq));
    out.extend_from_slice(payload);
    out
}

/// Build the 802.11 MAC header for a fleet.
///
/// Layout copied from `vendor/wfb-ng/src/wifibroadcast.hpp:156` with our magic in
/// place of `0x5742` and the fleet id in place of the `channel_id`: a data frame
/// (`0x08 0x01`) with a broadcast receiver address, the discriminator repeated in
/// both the transmitter and BSSID fields exactly as wfb-ng repeats its
/// `channel_id`, and the sequence-control field carrying `seq`.
///
/// `seq` is the 12-bit sequence number, advanced by the caller per transmission.
/// It is written above the 4-bit fragment number, which stays 0: every beacon is
/// a whole, unfragmented MSDU. A changing sequence number stops a driver or an
/// intermediate from treating consecutive beacons as retransmissions of one frame
/// and discarding them as duplicates. Bits above the 12th are dropped, so the
/// caller's counter may wrap freely.
pub fn ieee80211_header(fleet_id: u16, seq: u16) -> [u8; IEEE80211_HDR_LEN] {
    let mut h = [0u8; IEEE80211_HDR_LEN];
    // Frame control: data frame, from STA to DS; duration not set.
    h[0] = 0x08;
    h[1] = 0x01;
    // Receiver address: broadcast.
    h[4..10].fill(0xFF);
    let magic = SWARM_MAGIC.to_be_bytes();
    let fleet = (fleet_id as u32).to_be_bytes();
    // Transmitter address = magic || fleet_id; the BSSID repeats it.
    h[MAGIC_OFFSET..MAGIC_OFFSET + 2].copy_from_slice(&magic);
    h[FLEET_OFFSET..FLEET_OFFSET + 4].copy_from_slice(&fleet);
    h[16..18].copy_from_slice(&magic);
    h[18..22].copy_from_slice(&fleet);
    // Sequence control: sequence number in bits 4-15, fragment number 0.
    h[22..24].copy_from_slice(&(seq << 4).to_le_bytes());
    h
}

/// Split a captured radiotap frame into its payload and its signal reading,
/// verifying the magic and the fleet.
///
/// The in-kernel BPF ([`bpf_program`]) already rejects foreign magic and foreign
/// fleets, so on a live socket those two only ever pass. They are checked again
/// anyway, for two reasons that are not paranoia: the filter is attached
/// best-effort (a kernel that refuses it leaves the socket unfiltered rather than
/// dead), and this is the seam the codec is tested through off-target, where no
/// kernel filter exists at all.
///
/// The radiotap FLAGS field decides where the payload ends. A monitor-mode driver
/// that leaves the 802.11 FCS on the capture sets [`RT_F_FCS`], and those four
/// trailing CRC bytes are cut off here; the sealed payload is whatever lies
/// between the MAC header and the FCS. A frame flagged [`RT_F_BADFCS`] is
/// corrupt and is rejected before its header is read.
pub fn parse_frame(buf: &[u8], fleet_id: u16) -> Result<CapturedFrame<'_>, FrameReject> {
    let rt_len = match radiotap::declared_len(buf) {
        Some(n) => n,
        None => return Err(FrameReject::Malformed),
    };
    if buf.len() < rt_len + IEEE80211_HDR_LEN {
        return Err(FrameReject::Malformed);
    }
    let rt = &buf[..rt_len];
    let flags = radiotap_flags(rt).unwrap_or(0);
    if flags & RT_F_BADFCS != 0 {
        return Err(FrameReject::BadFcs);
    }
    let fcs_len = if flags & RT_F_FCS != 0 { FCS_LEN } else { 0 };
    if buf.len() < rt_len + IEEE80211_HDR_LEN + fcs_len {
        return Err(FrameReject::Malformed);
    }
    let mac = &buf[rt_len..buf.len() - fcs_len];
    let magic = u16::from_be_bytes([mac[MAGIC_OFFSET], mac[MAGIC_OFFSET + 1]]);
    if magic != SWARM_MAGIC {
        return Err(FrameReject::ForeignMagic);
    }
    let fleet = u32::from_be_bytes([
        mac[FLEET_OFFSET],
        mac[FLEET_OFFSET + 1],
        mac[FLEET_OFFSET + 2],
        mac[FLEET_OFFSET + 3],
    ]);
    if fleet != fleet_id as u32 {
        return Err(FrameReject::ForeignFleet);
    }
    Ok(CapturedFrame {
        payload: &mac[IEEE80211_HDR_LEN..],
        rssi_dbm: radiotap_rssi(rt),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing invariant of the whole design: our magic and wfb-ng's
    /// differ, so the two kernel filters on one shared adapter are mutually
    /// exclusive and neither bus ever sees the other's traffic.
    #[test]
    fn our_magic_cannot_collide_with_wfb_ng() {
        assert_ne!(SWARM_MAGIC, WFB_MAGIC);
        assert_eq!(SWARM_MAGIC, 0xAD03);
        assert_eq!(WFB_MAGIC, 0x5742);
        // They differ in the FIRST byte, so the filters diverge on one 16-bit
        // compare rather than needing the fleet word to disambiguate.
        assert_ne!(SWARM_MAGIC >> 8, WFB_MAGIC >> 8);
        // And, like wfb-ng's, our first address octet is multicast + locally
        // administered — correct for a frame every fleet member receives.
        let ours = (SWARM_MAGIC >> 8) as u8;
        let theirs = (WFB_MAGIC >> 8) as u8;
        assert_eq!(ours & 0b01, 0b01, "multicast bit set");
        assert_eq!(theirs & 0b01, 0b01, "wfb-ng sets it too");
    }

    #[test]
    fn the_mac_header_carries_the_magic_and_fleet_where_the_filter_reads_them() {
        let h = ieee80211_header(0x1234, 0xABCD);
        assert_eq!(h.len(), IEEE80211_HDR_LEN);
        assert_eq!(&h[0..2], &[0x08, 0x01], "data frame");
        assert_eq!(&h[4..10], &[0xFF; 6], "broadcast receiver");
        assert_eq!(&h[MAGIC_OFFSET..MAGIC_OFFSET + 2], &[0xAD, 0x03]);
        assert_eq!(
            &h[FLEET_OFFSET..FLEET_OFFSET + 4],
            &0x1234u32.to_be_bytes(),
            "fleet id is big-endian, matching wfb-ng's channel_id"
        );
        // The BSSID repeats the discriminator, as wfb-ng repeats its channel_id.
        assert_eq!(&h[16..22], &h[MAGIC_OFFSET..MAGIC_OFFSET + 6]);
        let control = u16::from_le_bytes([h[22], h[23]]);
        assert_eq!(control & 0x000F, 0, "fragment number 0");
        assert_eq!(control >> 4, 0xBCD, "12-bit sequence number");
        // Consecutive transmissions differ in the sequence number, not the
        // fragment number.
        let next = ieee80211_header(0x1234, 0xABCE);
        let next_control = u16::from_le_bytes([next[22], next[23]]);
        assert_eq!(next_control & 0x000F, 0, "fragment number stays 0");
        assert_eq!(next_control >> 4, (control >> 4) + 1);
    }

    /// Two fleets on one channel must not read each other's frames. This is the
    /// property `fleet_id` exists for.
    #[test]
    fn a_frame_round_trips_and_a_foreign_fleet_is_rejected() {
        let frame = build_frame(1, 0, b"sealed-payload");
        let got = parse_frame(&frame, 1).expect("our own fleet parses");
        assert_eq!(got.payload, b"sealed-payload");
        assert_eq!(parse_frame(&frame, 2), Err(FrameReject::ForeignFleet));
    }

    #[test]
    fn a_wfb_ng_frame_is_rejected_as_a_foreign_magic() {
        // wfb-ng's own header shape, with its magic and a channel_id.
        let mut frame = radiotap_header(1).to_vec();
        let mut mac = [0u8; IEEE80211_HDR_LEN];
        mac[0] = 0x08;
        mac[1] = 0x01;
        mac[4..10].fill(0xFF);
        mac[MAGIC_OFFSET..MAGIC_OFFSET + 2].copy_from_slice(&WFB_MAGIC.to_be_bytes());
        mac[FLEET_OFFSET..FLEET_OFFSET + 4].copy_from_slice(&0x0000_0100u32.to_be_bytes());
        frame.extend_from_slice(&mac);
        frame.extend_from_slice(&[0u8; 64]);
        assert_eq!(parse_frame(&frame, 1), Err(FrameReject::ForeignMagic));
    }

    #[test]
    fn truncated_and_bogus_radiotap_lengths_are_malformed_not_panics() {
        let frame = build_frame(1, 0, b"body");
        for cut in 0..FRAME_HEADER_LEN {
            assert_eq!(
                parse_frame(&frame[..cut], 1),
                Err(FrameReject::Malformed),
                "a {cut}-byte capture must be malformed"
            );
        }
        // A radiotap length that runs past the capture.
        let mut lying = frame.clone();
        lying[2] = 0xFF;
        lying[3] = 0x00;
        assert_eq!(parse_frame(&lying, 1), Err(FrameReject::Malformed));
        // A non-zero radiotap version is a format we cannot walk.
        let mut wrong_version = frame.clone();
        wrong_version[0] = 1;
        assert_eq!(parse_frame(&wrong_version, 1), Err(FrameReject::Malformed));
    }

    /// The parse must tolerate a longer radiotap header than we inject — the
    /// receiving driver decides the capture's field set, not us. Hardcoding 13
    /// bytes here would read every real capture at the wrong offset, and every
    /// beacon in the fleet would look like a foreign magic. This header declares
    /// FLAGS=F_FCS, so the capture carries the 4-byte CRC it promises and the
    /// payload must end before it.
    #[test]
    fn a_longer_receive_side_radiotap_header_is_walked_by_its_declared_length() {
        let mut frame = Vec::new();
        // 18-byte radiotap: version, pad, len, present(TSFT|FLAGS|DBM_ANTSIGNAL).
        let present: u32 = (1 << 0) | (1 << 1) | (1 << 5);
        frame.extend_from_slice(&[0x00, 0x00]);
        frame.extend_from_slice(&18u16.to_le_bytes());
        frame.extend_from_slice(&present.to_le_bytes());
        frame.extend_from_slice(&0u64.to_le_bytes()); // TSFT, 8-aligned at 8
        frame.push(RT_F_FCS); // FLAGS @16
        frame.push((-48i8) as u8); // DBM_ANTSIGNAL @17
        assert_eq!(frame.len(), 18);
        frame.extend_from_slice(&ieee80211_header(9, 0));
        frame.extend_from_slice(b"payload");
        frame.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // FCS

        let got = parse_frame(&frame, 9).expect("declared length is honoured");
        assert_eq!(
            got.payload, b"payload",
            "the FCS is not part of the payload"
        );
        assert_eq!(got.rssi_dbm, Some(-48), "the signal reading is carried out");

        // A capture too short to hold the FCS its flags declare is malformed.
        let cut = 18 + IEEE80211_HDR_LEN + FCS_LEN - 1;
        assert_eq!(parse_frame(&frame[..cut], 9), Err(FrameReject::Malformed));
    }

    /// Without F_FCS nothing is trimmed: the injected (and looped-back) frame has
    /// no FLAGS field at all and its payload runs to the end of the capture.
    #[test]
    fn a_capture_without_the_fcs_flag_keeps_its_whole_payload() {
        let mut frame = vec![0x00, 0x00, 9, 0x00];
        frame.extend_from_slice(&(1u32 << 1).to_le_bytes());
        frame.push(0x00); // FLAGS, nothing set
        frame.extend_from_slice(&ieee80211_header(9, 0));
        frame.extend_from_slice(b"payload!");
        assert_eq!(parse_frame(&frame, 9).unwrap().payload, b"payload!");
        assert_eq!(
            parse_frame(&build_frame(9, 0, b"payload!"), 9)
                .unwrap()
                .payload,
            b"payload!"
        );
    }

    /// A frame the receiver flagged as failing its FCS is corrupt. It is dropped
    /// as such before its header is trusted, so a bit-flipped wfb-ng frame is not
    /// counted as a foreign magic and a bit-flipped beacon is not a bad tag.
    #[test]
    fn a_bad_fcs_frame_is_rejected_before_its_header_is_read() {
        let mut frame = vec![0x00, 0x00, 9, 0x00];
        frame.extend_from_slice(&(1u32 << 1).to_le_bytes());
        frame.push(RT_F_FCS | RT_F_BADFCS);
        let mut mac = ieee80211_header(9, 0);
        mac[MAGIC_OFFSET] ^= 0x40; // the corruption hit the magic
        frame.extend_from_slice(&mac);
        frame.extend_from_slice(&[0u8; 54]);
        assert_eq!(parse_frame(&frame, 9), Err(FrameReject::BadFcs));
    }

    #[test]
    fn an_injected_frame_carries_no_signal_reading() {
        let frame = build_frame(1, 0, b"x");
        assert_eq!(parse_frame(&frame, 1).unwrap().rssi_dbm, None);
    }

    /// The injected header length must match what the parser skips, or a node
    /// would fail to read its own loopback frames — the exact case the neighbour
    /// table's self-filter relies on being parseable.
    #[test]
    fn the_injected_header_length_is_the_sum_of_its_two_parts() {
        assert_eq!(FRAME_HEADER_LEN, RADIOTAP_TX_LEN + IEEE80211_HDR_LEN);
        assert_eq!(FRAME_HEADER_LEN, 37);
        assert_eq!(build_frame(1, 0, &[0u8; 50]).len(), FRAME_HEADER_LEN + 50);
    }

    #[test]
    fn frame_kinds_decode_and_unknown_kinds_are_dropped() {
        assert_eq!(SwarmFrameKind::from_wire(1), Some(SwarmFrameKind::Beacon));
        assert_eq!(SwarmFrameKind::Beacon as u8, 1);
        for unknown in [0u8, 2, 3, 255] {
            assert_eq!(SwarmFrameKind::from_wire(unknown), None);
        }
    }
}
