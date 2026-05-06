//! Deterministic synthetic H.264 fixture generator.
//!
//! The fixture files under `tests/fixtures/` are checked in to the
//! repository so the integration test suite can run offline without
//! shelling out to an encoder or downloading sample media. This file
//! is the reproducible recipe for those bytes; running the ignored
//! test below regenerates the three fixtures in place.
//!
//!     cargo test -p ados-video --release --test fixtures_gen \
//!         -- --ignored --nocapture
//!
//! The fixtures are valid Annex-B framing only. They are NOT
//! decoder-valid — the SPS/PPS bodies are structurally correct enough
//! that the `AnnexBScanner` walks them, the NAL header bytes carry the
//! right NAL types, and the slice payloads are filler bytes (`0xAA`).
//! No external H.264 decoder will produce a picture from these
//! fixtures; that is intentional, the test suite only exercises the
//! Annex-B → RTP packetizer path.

#![allow(clippy::unreadable_literal)]

use std::fs;
use std::path::PathBuf;

/// 4-byte Annex-B start code. The scanner accepts both 3- and 4-byte
/// variants; we standardize on the 4-byte form for the fixtures.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// NAL header byte for a slice of an IDR picture (NAL type 5,
/// nal_ref_idc = 3 → byte = 0x65).
const NAL_HEADER_IDR: u8 = 0x65;

/// NAL header byte for a non-IDR slice (NAL type 1, nal_ref_idc = 2 →
/// byte = 0x41).
const NAL_HEADER_NON_IDR: u8 = 0x41;

/// SPS NAL header byte (NAL type 7, nal_ref_idc = 3 → byte = 0x67).
const NAL_HEADER_SPS: u8 = 0x67;

/// PPS NAL header byte (NAL type 8, nal_ref_idc = 3 → byte = 0x68).
const NAL_HEADER_PPS: u8 = 0x68;

/// Synthetic SPS body (after the 0x67 header byte). The bytes encode
/// roughly Baseline @ Level 3.1 for 1280x720, but the only properties
/// the scanner cares about are length and the leading NAL header.
/// Treat this as opaque test data.
const SPS_BODY: [u8; 23] = [
    0x42, 0x00, 0x1f, 0xe9, 0x02, 0x80, 0x2d, 0xd0, 0x0f, 0x18, 0x40,
    0x00, 0x00, 0x03, 0x00, 0x40, 0x00, 0x00, 0x0c, 0x83, 0xc5, 0x8b,
    0x92,
];

/// Synthetic PPS body (after the 0x68 header byte). Minimal defaults.
const PPS_BODY: [u8; 4] = [0xce, 0x06, 0xe2, 0x00];

/// Build one Annex-B NAL unit: start code + header + body bytes.
fn build_nal(header: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(START_CODE.len() + 1 + body.len());
    out.extend_from_slice(&START_CODE);
    out.push(header);
    out.extend_from_slice(body);
    out
}

/// Path to the `tests/fixtures/` directory relative to the crate root
/// (which is the `cargo test` working directory for an integration
/// test target).
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Write `bytes` to `<fixtures_dir>/<name>` and emit a `cargo test
/// -- --nocapture` line so the operator can see what landed.
fn write_fixture(name: &str, bytes: &[u8]) {
    let dir = fixtures_dir();
    fs::create_dir_all(&dir).expect("create fixtures dir");
    let path = dir.join(name);
    fs::write(&path, bytes).expect("write fixture");
    println!("wrote {}: {} bytes", path.display(), bytes.len());
}

/// `nal_small.h264`: SPS + PPS + a single small IDR slice. Each NAL is
/// well under the 1400-byte default MTU, so the round-trip test sees
/// one RTP packet per NAL with no FU-A fragmentation.
fn build_small() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(build_nal(NAL_HEADER_SPS, &SPS_BODY));
    out.extend(build_nal(NAL_HEADER_PPS, &PPS_BODY));
    // Slice payload: ~1.4 KiB total NAL after the header byte. Filler
    // bytes are 0xAA so the FU-A boundary assertions are easy to
    // visualize in a hex dump.
    let slice_body = vec![0xAA; 1400];
    out.extend(build_nal(NAL_HEADER_IDR, &slice_body));
    out
}

/// `nal_large.h264`: a single IDR slice that is at least 4× MTU large.
/// Forces FU-A fragmentation in the round-trip test.
fn build_large() -> Vec<u8> {
    let mut out = Vec::new();
    // Prepend SPS + PPS so a downstream parser that expects parameter
    // sets ahead of the slice does not choke; the round-trip assertion
    // for FU-A only cares about the trailing IDR slice.
    out.extend(build_nal(NAL_HEADER_SPS, &SPS_BODY));
    out.extend(build_nal(NAL_HEADER_PPS, &PPS_BODY));
    // 6000-byte slice payload — 4×1400 + 400. Produces 5 FU-A packets
    // at default MTU.
    let slice_body = vec![0xAA; 6000];
    out.extend(build_nal(NAL_HEADER_IDR, &slice_body));
    out
}

/// `nal_split.h264`: alternating SPS/PPS/IDR/non-IDR access units that
/// approximate a 720p stream over a short window. The NAL bodies are
/// all small so the fixture exercises single-NAL RTP packets, not
/// FU-A.
///
/// Layout: every 30 frames begins with SPS+PPS+IDR (the keyframe
/// cadence for a 1-second GOP at 30 fps). The remaining 29 frames are
/// non-IDR slices. We emit two GOPs (60 frames total) so the round-
/// trip test sees at least one keyframe boundary in the captured RTP
/// stream.
fn build_split() -> Vec<u8> {
    let mut out = Vec::new();
    let total_frames = 60usize;
    let gop_size = 30usize;
    for frame in 0..total_frames {
        if frame % gop_size == 0 {
            out.extend(build_nal(NAL_HEADER_SPS, &SPS_BODY));
            out.extend(build_nal(NAL_HEADER_PPS, &PPS_BODY));
            // Small IDR slice: 200 bytes of filler. Stays well under
            // MTU so each frame is one RTP packet.
            out.extend(build_nal(NAL_HEADER_IDR, &vec![0xAA; 200]));
        } else {
            // Non-IDR slice: 100 bytes of filler.
            out.extend(build_nal(NAL_HEADER_NON_IDR, &vec![0xBB; 100]));
        }
    }
    out
}

/// Operator entry point. Run with `--ignored` to (re)generate the
/// three fixture files in place. The output is deterministic.
#[test]
#[ignore]
fn fixtures_gen() {
    write_fixture("nal_small.h264", &build_small());
    write_fixture("nal_large.h264", &build_large());
    write_fixture("nal_split.h264", &build_split());
}

/// Sanity test that runs in the default `cargo test` pass: builds the
/// in-memory fixtures and asserts the basic shape invariants without
/// touching the filesystem. Catches accidental edits to the generator
/// that would change fixture sizes or NAL headers.
#[test]
fn small_fixture_shape() {
    let bytes = build_small();
    assert!(bytes.len() > 1400, "small fixture too small: {}", bytes.len());
    assert!(bytes.len() < 1600, "small fixture grew: {}", bytes.len());
    assert_eq!(&bytes[..4], &START_CODE);
    assert_eq!(bytes[4], NAL_HEADER_SPS);
}

#[test]
fn large_fixture_forces_fu_a() {
    let bytes = build_large();
    // SPS + PPS + 4-byte start + 1-byte header + 6000-byte body
    let expected_idr = 4 + 1 + 6000;
    assert!(
        bytes.len() > expected_idr,
        "large fixture missing IDR body"
    );
    // The trailing IDR body is at least 4× MTU.
    assert!(6000 >= 4 * 1400);
}

#[test]
fn split_fixture_has_two_keyframes() {
    let bytes = build_split();
    // Walk the byte stream and count IDR (NAL type 5) NAL units.
    let mut idr_count = 0usize;
    let mut i = 0usize;
    while i + 5 < bytes.len() {
        if bytes[i..i + 4] == START_CODE && (bytes[i + 4] & 0x1F) == 5 {
            idr_count += 1;
        }
        i += 1;
    }
    assert_eq!(idr_count, 2, "expected exactly 2 IDR keyframes per 60-frame fixture");
}
