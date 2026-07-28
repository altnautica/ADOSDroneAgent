//! Radiotap: the injection header we write, and the signal reading we walk out of
//! the one the driver hands back.
//!
//! Injection and capture are asymmetric and that asymmetry is load-bearing. We
//! emit a fixed 13-byte header (whatever the driver needs to pick a rate); the
//! driver hands *back* a header of its own choosing, whose length and field set
//! we do not control. So the write side is a constant and the read side is a
//! bitmap walk driven entirely by the header's own declared length.

/// The injected radiotap header length. Byte-for-byte the HT header `wfb_tx`
/// injects with (`vendor/wfb-ng/src/wifibroadcast.hpp:83`), so the driver sees the
/// same shape from both senders on the shared adapter.
pub const RADIOTAP_TX_LEN: usize = 13;

/// Offset of the MCS index inside the injected header
/// (`wifibroadcast.hpp:134`'s `MCS_IDX_OFF`).
const RADIOTAP_MCS_IDX_OFF: usize = 12;

/// The MCS index the beacon is injected at.
///
/// **0, not the video plane's 1.** MCS 0 (BPSK 1/2, 6.5 Mbps) needs roughly 3 dB
/// less SNR than MCS 1, and the beacon is the input to the collision-avoidance
/// layer: the one message that must still decode when the video link is already
/// failing. The cost is 48 µs per frame — see [`crate::AIRTIME_BUDGET`], where
/// even at N=24 the whole bus stays under 1% airtime either way.
pub const BEACON_MCS_INDEX: u8 = 0;

/// `MCS_KNOWN` from `wifibroadcast.hpp:81`: HAVE_BW | HAVE_MCS | HAVE_GI |
/// HAVE_FEC | HAVE_STBC. Declaring the same known-field set wfb-ng does keeps the
/// driver on one interpretation of the MCS field for both senders.
const RADIOTAP_MCS_KNOWN: u8 = 0x37;

/// Radiotap `it_present` bit for `IEEE80211_RADIOTAP_DBM_ANTSIGNAL`.
const RT_BIT_DBM_ANTSIGNAL: u32 = 5;
/// Radiotap `it_present` bit for the extension-word chain.
const RT_BIT_EXT: u32 = 31;

/// `(align, size)` of every radiotap field that can precede DBM_ANTSIGNAL:
/// TSFT, FLAGS, RATE, CHANNEL, FHSS.
///
/// Walking exactly these five is *sufficient*, not a partial implementation: the
/// signal field is present bit 5, so no field defined at a higher bit can shift
/// its offset, and no unknown-field bailout is reachable before we arrive at it.
const RT_FIELDS_BEFORE_SIGNAL: [(usize, usize); 5] = [(8, 8), (1, 1), (1, 1), (2, 4), (1, 2)];

/// Build the radiotap injection header.
///
/// Identical to wfb-ng's HT header with the MCS index substituted, including the
/// `IEEE80211_RADIOTAP_F_TX_NOACK` TX flag — a broadcast frame nobody
/// acknowledges, which is what stops the driver retrying a beacon that will never
/// be ACKed.
pub fn radiotap_header(mcs_index: u8) -> [u8; RADIOTAP_TX_LEN] {
    let mut h = [
        0x00,
        0x00, // radiotap version, pad
        0x0d,
        0x00, // it_len = 13, little-endian
        0x00,
        0x80,
        0x08,
        0x00, // it_present: TX_FLAGS | MCS
        0x08,
        0x00, // TX_FLAGS = F_TX_NOACK
        RADIOTAP_MCS_KNOWN,
        0x00,
        0x00, // MCS known-bitmap, flags, index
    ];
    h[RADIOTAP_MCS_IDX_OFF] = mcs_index;
    h
}

/// Read the total header length a captured radiotap header declares, or `None`
/// when the buffer is too short or the version is one we cannot walk.
pub fn declared_len(rt: &[u8]) -> Option<usize> {
    if rt.len() < 4 || rt[0] != 0 {
        return None;
    }
    let len = u16::from_le_bytes([rt[2], rt[3]]) as usize;
    if len < 4 {
        return None;
    }
    Some(len)
}

/// Read the antenna signal (dBm) out of a captured radiotap header, or `None`
/// when the capture did not include one.
///
/// `None` is a first-class answer, not a failure: some drivers omit the field
/// entirely, and a fabricated `0` or `-100` would render as a real measurement in
/// the operator's signal column.
pub fn radiotap_rssi(rt: &[u8]) -> Option<i8> {
    let declared = declared_len(rt)?;
    if declared > rt.len() || declared < 8 {
        return None;
    }
    let rt = &rt[..declared];

    // The present bitmap is a chain: bit 31 of a word means another word follows,
    // and the fields themselves begin after the LAST word. So the chain has to be
    // walked before any field offset can be computed.
    let mut off = 4;
    let mut first_word = None;
    loop {
        if off + 4 > rt.len() {
            return None;
        }
        let w = u32::from_le_bytes([rt[off], rt[off + 1], rt[off + 2], rt[off + 3]]);
        first_word.get_or_insert(w);
        off += 4;
        if w & (1 << RT_BIT_EXT) == 0 {
            break;
        }
    }
    // Only the first word's low bits address the standard-namespace fields walked
    // below; a present bit in a later word belongs to a vendor or extended
    // namespace and cannot precede bit 5 of the first.
    let present = first_word?;
    if present & (1 << RT_BIT_DBM_ANTSIGNAL) == 0 {
        return None;
    }
    for (bit, (align, size)) in RT_FIELDS_BEFORE_SIGNAL.iter().enumerate() {
        if present & (1 << bit) == 0 {
            continue;
        }
        off = align_up(off, *align).checked_add(*size)?;
    }
    // DBM_ANTSIGNAL is a 1-byte field with 1-byte alignment.
    (off < rt.len()).then(|| rt[off] as i8)
}

/// Round `off` up to the next multiple of `align`. Radiotap fields are aligned to
/// their natural size, measured from the start of the radiotap header.
fn align_up(off: usize, align: usize) -> usize {
    (off + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_injection_header_matches_the_wfb_ng_ht_header_with_our_mcs() {
        let h = radiotap_header(BEACON_MCS_INDEX);
        assert_eq!(h.len(), RADIOTAP_TX_LEN);
        // Byte-for-byte `radiotap_header_ht` from wifibroadcast.hpp:83.
        assert_eq!(
            h,
            [0x00, 0x00, 0x0d, 0x00, 0x00, 0x80, 0x08, 0x00, 0x08, 0x00, 0x37, 0x00, 0x00]
        );
        // The declared it_len must equal the emitted length, or a receiver's walk
        // lands mid-header.
        assert_eq!(declared_len(&h), Some(RADIOTAP_TX_LEN));
        // The MCS index is substituted, never appended.
        assert_eq!(radiotap_header(3)[RADIOTAP_MCS_IDX_OFF], 3);
        assert_eq!(radiotap_header(3).len(), RADIOTAP_TX_LEN);
    }

    #[test]
    fn declared_len_rejects_short_buffers_bad_versions_and_impossible_lengths() {
        assert_eq!(declared_len(&[]), None);
        assert_eq!(declared_len(&[0, 0, 13]), None);
        assert_eq!(
            declared_len(&[1, 0, 13, 0]),
            None,
            "version 1 is unwalkable"
        );
        assert_eq!(declared_len(&[0, 0, 3, 0]), None, "shorter than the header");
        assert_eq!(declared_len(&[0, 0, 0xFF, 0xFF]), Some(0xFFFF));
    }

    /// A capture with no signal field reads `None`, never a fabricated number.
    #[test]
    fn a_missing_signal_field_reads_none() {
        assert_eq!(radiotap_rssi(&radiotap_header(0)), None);
        assert_eq!(radiotap_rssi(&[]), None);
        // A length that runs past the buffer.
        assert_eq!(radiotap_rssi(&[0, 0, 0xFF, 0xFF, 0, 0, 0, 0]), None);
        // A version we cannot walk.
        assert_eq!(radiotap_rssi(&[1, 0, 8, 0, 0x20, 0, 0, 0]), None);
        // Present bit set but the header ends before the field.
        let mut rt = vec![0x00, 0x00, 8, 0x00];
        rt.extend_from_slice(&(1u32 << RT_BIT_DBM_ANTSIGNAL).to_le_bytes());
        assert_eq!(radiotap_rssi(&rt), None);
    }

    /// The alignment rules are what make the walk correct: TSFT is 8-aligned and
    /// CHANNEL is 2-aligned, so a naive byte sum lands on the wrong field.
    #[test]
    fn the_signal_offset_respects_radiotap_field_alignment() {
        // FLAGS | RATE | CHANNEL | DBM_ANTSIGNAL. After the 8-byte header:
        // FLAGS @8, RATE @9, CHANNEL 2-aligned @10..14, signal @14.
        let present: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << RT_BIT_DBM_ANTSIGNAL);
        let mut rt = vec![0x00, 0x00, 15, 0x00];
        rt.extend_from_slice(&present.to_le_bytes());
        rt.push(0x10); // FLAGS @8
        rt.push(0x02); // RATE  @9
        rt.extend_from_slice(&[0, 0, 0, 0]); // CHANNEL @10..14
        rt.push((-61i8) as u8); // DBM_ANTSIGNAL @14
        assert_eq!(rt.len(), 15);
        assert_eq!(radiotap_rssi(&rt), Some(-61));

        // Adding FHSS (bit 4, 2 bytes) shifts the signal by exactly 2.
        let mut rt2 = vec![0x00, 0x00, 17, 0x00];
        rt2.extend_from_slice(&(present | (1 << 4)).to_le_bytes());
        rt2.extend_from_slice(&[0x10, 0x02, 0, 0, 0, 0, 0, 0]);
        rt2.push((-61i8) as u8); // @16
        assert_eq!(rt2.len(), 17);
        assert_eq!(radiotap_rssi(&rt2), Some(-61));
    }

    /// TSFT is 8-aligned, so a header carrying it starts the field area at 8 with
    /// no pad — but a second present word would push it to 12 and misalign TSFT to
    /// 16. Pinning both cases catches an alignment computed from the wrong base.
    #[test]
    fn tsft_alignment_is_measured_from_the_header_start() {
        let present: u32 = (1 << 0) | (1 << RT_BIT_DBM_ANTSIGNAL);
        let mut rt = vec![0x00, 0x00, 17, 0x00];
        rt.extend_from_slice(&present.to_le_bytes());
        rt.extend_from_slice(&0u64.to_le_bytes()); // TSFT @8..16
        rt.push((-70i8) as u8); // signal @16
        assert_eq!(rt.len(), 17);
        assert_eq!(radiotap_rssi(&rt), Some(-70));
    }

    /// An extended present-word chain pushes the field area further out. Missing
    /// the chain would read the first field byte as the signal.
    #[test]
    fn an_extended_present_bitmap_chain_shifts_the_field_area() {
        let first: u32 = (1 << RT_BIT_DBM_ANTSIGNAL) | (1 << RT_BIT_EXT);
        let mut rt = vec![0x00, 0x00, 13, 0x00];
        rt.extend_from_slice(&first.to_le_bytes());
        rt.extend_from_slice(&0u32.to_le_bytes()); // second present word
        rt.push((-33i8) as u8); // fields start at 12, after BOTH words
        assert_eq!(rt.len(), 13);
        assert_eq!(radiotap_rssi(&rt), Some(-33));
    }

    /// The reading is signed: a real RSSI is always negative, so reading the byte
    /// as unsigned would report -48 dBm as +208.
    #[test]
    fn the_signal_reading_is_signed() {
        let mut rt = vec![0x00, 0x00, 9, 0x00];
        rt.extend_from_slice(&(1u32 << RT_BIT_DBM_ANTSIGNAL).to_le_bytes());
        rt.push((-48i8) as u8);
        assert_eq!(radiotap_rssi(&rt), Some(-48));
    }
}
