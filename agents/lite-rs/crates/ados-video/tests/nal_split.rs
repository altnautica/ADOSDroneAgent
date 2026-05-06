//! Annex-B scanner coverage for byte streams whose NAL boundaries land
//! across a chunk boundary. Real captures exhibit this constantly: a
//! libcamera-vid stdout pull or a V4L2 dequeue routinely splits a 4-byte
//! start code (`00 00 00 01`) such that the `00 00 00` lands in one
//! buffer and the `01` lands in the next.
//!
//! The scanner buffers across `push()` calls, so the API contract is:
//! once the trailing start code that delimits the *next* unit arrives,
//! the previous unit is emitted in full, regardless of how many chunks
//! it took to assemble.
//!
//! This file pins three edge cases:
//!
//!   1. A 4-byte start code split across two `push()` calls.
//!   2. A 3-byte start code split across two `push()` calls.
//!   3. A large IDR NAL split across two `push()` calls — the unit must
//!      still be emitted as one piece, and the keyframe detector must
//!      report `true` on the assembled body.

use ados_video::nal::{is_keyframe_unit, AnnexBScanner};

/// Sentinel start code we append after each unit so the scanner has a
/// trailing delimiter. The scanner only emits a unit once it sees the
/// next start code, so every test case ends with this sentinel.
const SENTINEL: [u8; 5] = [0, 0, 0, 1, 0x41]; // 4-byte start + P-slice

#[test]
fn four_byte_start_code_split_across_pushes() {
    let mut s = AnnexBScanner::default();

    // First chunk ends with the first three bytes of a 4-byte start
    // code: `00 00 00`. The scanner must NOT decide on this alone.
    s.push(&[0, 0, 0]);
    assert!(
        s.next_unit().is_none(),
        "scanner must not emit on a partial start code",
    );

    // Second chunk delivers the trailing `01` plus a small SPS body
    // and the sentinel start code that marks the next unit.
    s.push(&[1, 0x67, 0x42, 0x00, 0x1e]);
    s.push(&SENTINEL);

    let unit = s.next_unit().expect("unit emitted after split start code");
    assert_eq!(
        unit,
        vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e],
        "split 4-byte start code must reassemble the SPS unit",
    );
}

#[test]
fn three_byte_start_code_split_across_pushes() {
    let mut s = AnnexBScanner::default();

    // First chunk ends mid-3-byte start code.
    s.push(&[0, 0]);
    assert!(s.next_unit().is_none());

    // Second chunk delivers the trailing `01` and the unit body.
    s.push(&[1, 0x68, 0xce, 0x06, 0xe2]);
    s.push(&SENTINEL);

    let unit = s.next_unit().expect("unit emitted after split start code");
    // `find_start_code` on `[0,0,1,0x68,...]` returns a 3-byte span,
    // so the emitted unit starts at the first `00`.
    assert_eq!(
        unit,
        vec![0, 0, 1, 0x68, 0xce, 0x06, 0xe2],
        "split 3-byte start code must reassemble the PPS unit",
    );
}

#[test]
fn large_idr_split_across_pushes_reassembles_with_keyframe_flag() {
    // Build a synthetic 8 KiB IDR slice. The body bytes after the NAL
    // header are arbitrary; we just need enough volume to be confident
    // the scanner is not silently truncating across the split.
    let mut idr_body: Vec<u8> = vec![0u8, 0, 0, 1, 0x65]; // 4-byte start + IDR header
    idr_body.extend((0..8 * 1024).map(|i| ((i & 0xFF) ^ 0x5A) as u8));

    // Cut the NAL roughly in half so the back half lands on the second
    // push; pick a cut point that does not coincide with a fake start
    // code in the synthetic body to keep the scanner's parse stable.
    let cut = idr_body.len() / 2;
    let (first, second) = idr_body.split_at(cut);

    let mut s = AnnexBScanner::default();
    s.push(first);
    // Mid-stream the scanner has only one start code (the IDR's own)
    // and no trailing delimiter, so nothing is emitted yet.
    assert!(
        s.next_unit().is_none(),
        "scanner must hold the IDR until a trailing start code arrives",
    );

    s.push(second);
    s.push(&SENTINEL);

    let unit = s.next_unit().expect("IDR emitted after sentinel");
    assert_eq!(
        unit.len(),
        idr_body.len(),
        "scanner must emit the full IDR body without truncation",
    );
    assert_eq!(
        &unit[..5],
        &[0, 0, 0, 1, 0x65],
        "emitted unit must keep the leading start code + NAL header",
    );

    // The fifth byte of the unit is the NAL header (`0x65` = IDR slice).
    let nal_header = unit[4];
    assert!(
        is_keyframe_unit(nal_header),
        "NAL header 0x{nal_header:02x} must be detected as a keyframe",
    );
}

#[test]
fn contains_keyframe_detects_idr_in_split_buffer_after_reassembly() {
    // Cross-check: even if the scanner emits in two stages, the
    // emitted unit's header byte must independently pass the keyframe
    // check used by the V4L2 backend's lightweight pre-scan.
    let mut s = AnnexBScanner::default();
    s.push(&[0, 0, 0]);
    s.push(&[1, 0x65, 0xAA, 0xBB]);
    s.push(&SENTINEL);

    let unit = s.next_unit().expect("idr unit");
    assert!(is_keyframe_unit(unit[4]));
}
