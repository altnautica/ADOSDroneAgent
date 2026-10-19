use super::*;

fn payload(seq: u64, sim: u64, cam: u32) -> Vec<u8> {
    let mut b = WE_SEI_UUID.to_vec();
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&sim.to_le_bytes());
    b.extend_from_slice(&cam.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

#[test]
fn the_uuid_is_the_documented_string() {
    let s: String = WE_SEI_UUID.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(s, "5745454e47494e458000000000000001");
}

#[test]
fn a_payload_parses_little_endian_and_a_foreign_one_does_not() {
    let p = payload(0x0102_0304_0506_0708, 4_100_000_000, 3);
    assert_eq!(&p[16..24], &[8, 7, 6, 5, 4, 3, 2, 1]);
    assert_eq!(
        WeFrameId::from_payload(&p),
        Some(WeFrameId {
            seq: 0x0102_0304_0506_0708,
            sim_time_ns: 4_100_000_000,
            cam: 3
        })
    );
    let mut other = p.clone();
    other[0] ^= 1;
    assert_eq!(WeFrameId::from_payload(&other), None);
    assert_eq!(WeFrameId::from_payload(&p[..39]), None);
}

#[test]
fn a_sei_nal_with_another_message_first_still_yields_the_id() {
    // A foreign unregistered message, then ours, then the trailing bits.
    let mut rbsp = vec![5u8, 20];
    rbsp.extend_from_slice(&[0xab; 20]);
    rbsp.push(5);
    rbsp.push(WE_SEI_LEN as u8);
    rbsp.extend_from_slice(&payload(7, 9, 2));
    rbsp.push(0x80);
    let mut nal = vec![NAL_SEI];
    nal.extend_from_slice(&rbsp);
    let mut scratch = Vec::new();
    assert_eq!(
        frame_id_in_sei(&nal, &mut scratch),
        Some(WeFrameId {
            seq: 7,
            sim_time_ns: 9,
            cam: 2
        })
    );
    // Not an SEI NAL at all.
    assert_eq!(frame_id_in_sei(&[0x65, 0x88], &mut scratch), None);
}

#[test]
fn emulation_prevention_is_removed() {
    let mut out = Vec::new();
    unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 5], &mut out);
    assert_eq!(out, [0, 0, 1, 0, 0, 0, 5]);
}

#[test]
fn the_scanner_splits_nals_across_arbitrary_chunks() {
    let stream = [
        0, 0, 0, 1, 0x09, 0xf0, // AUD (4-byte start code)
        0, 0, 1, 0x67, 0x42, // SPS (3-byte start code)
        0, 0, 0, 1, 0x65, 0x88, 0x84, // IDR slice
        0, 0, 0, 1, 0x09, 0xf0, // next AUD terminates the IDR
    ];
    for chunk in [1usize, 2, 5, 64] {
        let mut s = AnnexBScanner::default();
        let mut nals = Vec::new();
        for c in stream.chunks(chunk) {
            s.push(c);
            while let Some(n) = s.next_nal() {
                nals.push(n);
            }
        }
        assert_eq!(
            nals,
            vec![vec![0x09, 0xf0], vec![0x67, 0x42], vec![0x65, 0x88, 0x84]],
            "chunk {chunk}"
        );
    }
}

#[test]
fn picture_starts_are_first_slices_only() {
    assert!(starts_picture(&[0x65, 0x88]));
    assert!(starts_picture(&[0x41, 0x9a]));
    assert!(!starts_picture(&[0x41, 0x1a]));
    assert!(!starts_picture(&[0x06, 0x80]));
}
