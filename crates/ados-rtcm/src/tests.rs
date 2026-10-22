use super::*;

fn msg(len: usize) -> Vec<u8> {
    (0..len).map(|i| i as u8).collect()
}

/// `(flags, len)` of every fragment.
fn shape(out: &[Fragment]) -> Vec<(u8, u8)> {
    out.iter().map(|f| (f.flags, f.len)).collect()
}

/// Fragmented flags for fragment `id` of sequence `seq`.
fn fl(id: u8, seq: u8) -> u8 {
    1 | id << 1 | seq << 3
}

#[test]
fn up_to_180_bytes_is_one_unfragmented_message() {
    let mut f = Fragmenter::default();
    for len in [3, 179, 180] {
        let m = msg(len);
        let out = f.fragment(&m).unwrap();
        assert_eq!(shape(&out), vec![(0, len as u8)], "{len} bytes");
        assert_eq!(&out[0].data[..len], &m[..]);
    }
    // No sequence id was spent on them.
    assert_eq!(f.fragment(&msg(181)).unwrap()[0].flags >> 3, 0);
}

#[test]
fn over_180_bytes_fragments_and_ends_with_a_short_fragment() {
    let mut f = Fragmenter::default();
    assert_eq!(
        shape(&f.fragment(&msg(181)).unwrap()),
        vec![(fl(0, 0), 180), (fl(1, 0), 1)]
    );
    // An exact multiple below 720 gets a zero-length terminator.
    assert_eq!(
        shape(&f.fragment(&msg(360)).unwrap()),
        vec![(fl(0, 1), 180), (fl(1, 1), 180), (fl(2, 1), 0)]
    );
    assert_eq!(
        shape(&f.fragment(&msg(540)).unwrap()),
        vec![
            (fl(0, 2), 180),
            (fl(1, 2), 180),
            (fl(2, 2), 180),
            (fl(3, 2), 0)
        ]
    );
    assert_eq!(
        shape(&f.fragment(&msg(541)).unwrap()),
        vec![
            (fl(0, 3), 180),
            (fl(1, 3), 180),
            (fl(2, 3), 180),
            (fl(3, 3), 1)
        ]
    );
    let m = msg(719);
    let out = f.fragment(&m).unwrap();
    assert_eq!(
        shape(&out),
        vec![
            (fl(0, 4), 180),
            (fl(1, 4), 180),
            (fl(2, 4), 180),
            (fl(3, 4), 179)
        ]
    );
    assert_eq!(&out[3].data[..179], &m[540..]);
    // Four full fragments complete on their own: no terminator.
    assert_eq!(
        shape(&f.fragment(&msg(720)).unwrap()),
        vec![
            (fl(0, 5), 180),
            (fl(1, 5), 180),
            (fl(2, 5), 180),
            (fl(3, 5), 180)
        ]
    );
}

#[test]
fn over_720_bytes_is_refused_without_spending_a_sequence() {
    let mut f = Fragmenter::default();
    assert_eq!(f.fragment(&msg(721)), Err(TooLong(721)));
    assert_eq!(f.fragment(&msg(200)).unwrap()[0].flags >> 3, 0);
}

#[test]
fn fragment_counts_match_the_fragmenter() {
    for len in [179, 180, 181, 360, 540, 541, 719, 720] {
        let n = Fragmenter::default().fragment(&msg(len)).unwrap().len();
        assert_eq!(message_fragments(len), Ok(n), "{len} bytes");
    }
    assert_eq!(message_fragments(721), Err(TooLong(721)));
}

#[test]
fn sequence_wraps_at_32() {
    let mut f = Fragmenter::default();
    for _ in 0..32 {
        f.fragment(&[0u8; 200]).unwrap();
    }
    assert_eq!(f.fragment(&[0u8; 200]).unwrap()[0].flags >> 3, 0);
}

#[test]
fn crc24q_matches_the_check_value() {
    assert_eq!(crc24q(b"123456789"), 0xCD_E7_03);
}

#[test]
fn stream_splits_on_crc_valid_frames_and_resyncs() {
    let a = encode_rtcm3(&[1u8; 20]);
    let b = encode_rtcm3(&[2u8; 300]);
    let mut corrupt = encode_rtcm3(&[3u8; 10]);
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    // Leading junk, a stray preamble, a frame with a bad CRC, then good ones.
    let mut s = vec![0x00, 0x42, 0xD3];
    s.extend(&a);
    s.extend(&corrupt);
    s.extend(&b);
    s.extend([0xD3, 0x00]); // truncated trailing header
    let parts = split_rtcm3(&s);
    assert_eq!(parts, vec![&a[..], &b[..]]);
    assert_eq!(fragment_count(&s), Ok(1 + 2));
    // Nothing that is not RTCM3 makes a message.
    assert!(split_rtcm3(&[1, 2, 3, 4, 5, 6, 7]).is_empty());
    assert_eq!(fragment_count(&[0u8; 900]), Ok(0));
}

#[test]
fn splitter_joins_a_frame_split_at_every_byte_boundary() {
    let a = encode_rtcm3(&(0..40u8).collect::<Vec<_>>());
    let b = encode_rtcm3(&[9u8; 300]);
    let mut stream = vec![0x00, 0xD3];
    stream.extend(&a);
    stream.extend(&b);
    for cut in 0..=stream.len() {
        let mut s = Rtcm3Splitter::default();
        let mut got = s.push(&stream[..cut]);
        got.extend(s.push(&stream[cut..]));
        assert_eq!(got, vec![a.clone(), b.clone()], "cut at {cut}");
        assert_eq!(s.pending(), 0, "cut at {cut}");
    }
}

#[test]
fn splitter_holds_only_a_bounded_partial_frame_and_resyncs() {
    let mut s = Rtcm3Splitter::default();
    // Junk without a preamble is not held.
    assert!(s.push(&[1u8; 5000]).is_empty());
    assert_eq!(s.pending(), 0);
    // A stray preamble claiming a long frame is held until it cannot be a
    // frame, then a real one behind it still comes through.
    let good = encode_rtcm3(&[4u8; 50]);
    assert!(s.push(&[0xD3, 0x03, 0xFF, 7, 7]).is_empty());
    assert!(s.pending() < 1029);
    assert_eq!(s.push(&good), vec![good.clone()]);
    assert_eq!(s.pending(), 0);
    // A corrupt frame is dropped, the next one survives.
    let mut bad = encode_rtcm3(&[5u8; 20]);
    bad[10] ^= 1;
    let mut chunk = bad;
    chunk.extend(&good);
    assert_eq!(s.push(&chunk), vec![good]);
}
