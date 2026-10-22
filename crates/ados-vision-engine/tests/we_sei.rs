//! The simulator SEI parser against a captured-format byte fixture.
//!
//! `tests/fixtures/we-sei.h264` is two Annex-B access units the way the
//! simulator's camera daemon emits them (AUD, SPS, PPS, the frame-identity SEI,
//! then the slice), plus a closing AUD. [`synth_stream`] rebuilds it with the
//! writer's encoding (payload type 5, size 40, UUID then little-endian `seq`,
//! `sim_time_ns`, `cam` and a reserved zero, emulation prevention on), and the
//! test asserts the committed bytes are exactly that, so the fixture cannot
//! drift from the format.

use ados_vision_engine::sei::{frame_id_in_sei, nal_type, AnnexBScanner, WeFrameId, WE_SEI_UUID};

const FIXTURE: &[u8] = include_bytes!("fixtures/we-sei.h264");

/// The writer's payload: UUID, then `seq`, `sim_time_ns`, `cam` and a
/// reserved zero, all little-endian.
fn to_sei(id: &WeFrameId) -> [u8; 40] {
    let mut b = [0u8; 40];
    b[..16].copy_from_slice(&WE_SEI_UUID);
    b[16..24].copy_from_slice(&id.seq.to_le_bytes());
    b[24..32].copy_from_slice(&id.sim_time_ns.to_le_bytes());
    b[32..36].copy_from_slice(&id.cam.to_le_bytes());
    b
}

/// One SEI NAL as an encoder writes it: emulation prevention on.
fn sei_nal(id: &WeFrameId) -> Vec<u8> {
    let mut rbsp = vec![5u8, 40];
    rbsp.extend_from_slice(&to_sei(id));
    rbsp.push(0x80);
    let mut nal = vec![0x06];
    let mut zeros = 0;
    for b in rbsp {
        if zeros >= 2 && b <= 3 {
            nal.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        nal.push(b);
    }
    nal
}

const FIRST: WeFrameId = WeFrameId {
    seq: 42,
    sim_time_ns: 1_234_567_890,
    cam: 1,
};
/// All-zero fields force runs of zeros, so emulation prevention fires.
const SECOND: WeFrameId = WeFrameId {
    seq: 0,
    sim_time_ns: 0,
    cam: 0,
};

fn synth_stream() -> Vec<u8> {
    const SC: [u8; 4] = [0, 0, 0, 1];
    let mut s = Vec::new();
    let nal = |s: &mut Vec<u8>, bytes: &[u8]| {
        s.extend_from_slice(&SC);
        s.extend_from_slice(bytes);
    };
    nal(&mut s, &[0x09, 0xf0]);
    nal(&mut s, &[0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01]);
    nal(&mut s, &[0x68, 0xce, 0x3c, 0x80]);
    nal(&mut s, &sei_nal(&FIRST));
    nal(&mut s, &[0x65, 0x88, 0x84, 0x00, 0x33]);
    nal(&mut s, &[0x09, 0xf0]);
    nal(&mut s, &sei_nal(&SECOND));
    nal(&mut s, &[0x41, 0x9a, 0x00, 0x11]);
    nal(&mut s, &[0x09, 0xf0]);
    s
}

fn scan(stream: &[u8], chunk: usize) -> (Vec<WeFrameId>, Vec<u8>) {
    let mut scanner = AnnexBScanner::default();
    let mut scratch = Vec::new();
    let (mut ids, mut types) = (Vec::new(), Vec::new());
    for c in stream.chunks(chunk) {
        scanner.push(c);
        while let Some(n) = scanner.next_nal() {
            types.push(nal_type(&n));
            ids.extend(frame_id_in_sei(&n, &mut scratch));
        }
    }
    (ids, types)
}

#[test]
fn the_fixture_is_the_writer_format() {
    assert_eq!(FIXTURE, synth_stream().as_slice());
}

#[test]
fn the_fixture_yields_the_frame_identity() {
    for chunk in [1, 7, 4096] {
        let (ids, types) = scan(FIXTURE, chunk);
        assert_eq!(ids, vec![FIRST, SECOND], "chunk {chunk}");
        assert_eq!(ids[0].seq, 42);
        assert_eq!(ids[0].sim_time_ns, 1_234_567_890);
        assert_eq!(ids[0].cam, 1);
        // The closing AUD is held back until more bytes arrive.
        assert_eq!(types, vec![9, 7, 8, 6, 5, 9, 6, 1], "chunk {chunk}");
    }
}
