use super::*;
use crate::fixtures::{self, Pvt};

fn mixed() -> (Vec<u8>, Vec<Frame>) {
    let pvt = fixtures::nav_pvt(Pvt::default());
    let gga = fixtures::nmea("GNGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,");
    let rtcm = fixtures::rtcm(1005, 17);
    let mut bytes = vec![0x00, 0xFF, b'x', 0xB5, 0x00, 0xD3, 0xFF];
    bytes.extend_from_slice(&pvt);
    bytes.extend_from_slice(b"garbage$\r\n");
    bytes.extend_from_slice(&gga);
    bytes.extend_from_slice(&[0x13, 0x37]);
    bytes.extend_from_slice(&rtcm);
    bytes.extend_from_slice(&fixtures::nav_dop(80));
    let (c, i, p) = fixtures::unframe(&pvt);
    let want = vec![
        Frame::Ubx {
            class: c,
            id: i,
            payload: p,
        },
        Frame::Nmea(String::from_utf8(gga[..gga.len() - 2].to_vec()).unwrap()),
        Frame::Rtcm(rtcm),
        Frame::Ubx {
            class: 0x01,
            id: 0x04,
            payload: {
                let mut d = vec![0u8; 18];
                d[12] = 80;
                d
            },
        },
    ];
    (bytes, want)
}

#[test]
fn splits_mixed_stream_with_garbage_in_one_push() {
    let (bytes, want) = mixed();
    let mut s = Splitter::new();
    assert_eq!(s.push(&bytes), want);
    assert!(s.garbage() > 0);
}

#[test]
fn splits_mixed_stream_across_every_chunk_size() {
    let (bytes, want) = mixed();
    for chunk in [1, 2, 3, 7, 13, 64] {
        let mut s = Splitter::new();
        let got: Vec<Frame> = bytes.chunks(chunk).flat_map(|c| s.push(c)).collect();
        assert_eq!(got, want, "chunk size {chunk}");
    }
}

#[test]
fn resyncs_after_a_corrupt_frame() {
    let mut bad = fixtures::nav_dop(50);
    let n = bad.len();
    bad[n - 1] ^= 0xFF;
    let mut bad_rtcm = fixtures::rtcm(1074, 30);
    bad_rtcm[10] ^= 0x01;
    let mut bytes = bad;
    bytes.extend_from_slice(&bad_rtcm);
    bytes.extend_from_slice(b"$GNRMC,bad*00\r\n");
    bytes.extend_from_slice(&fixtures::nav_dop(70));
    let got = Splitter::new().push(&bytes);
    assert_eq!(got.len(), 1);
    assert!(matches!(&got[0], Frame::Ubx { id: 0x04, payload, .. } if payload[12] == 70));
}

#[test]
fn checksums_match_reference_values() {
    // The MON-VER poll is the textbook UBX frame: B5 62 0A 04 00 00 0E 34.
    assert_eq!(
        ubx_frame(0x0A, 0x04, &[]),
        vec![0xB5, 0x62, 0x0A, 0x04, 0x00, 0x00, 0x0E, 0x34]
    );
    // CRC-24Q check value.
    assert_eq!(crc24q(b"123456789"), 0x00CD_E703);
}

#[test]
fn rtcm_message_type_reads_the_first_twelve_bits() {
    assert_eq!(rtcm_msg_type(&fixtures::rtcm(1230, 4)), Some(1230));
    assert_eq!(rtcm_msg_type(&fixtures::rtcm(1005, 0)), Some(1005));
}

#[test]
fn incomplete_frame_waits_for_more_bytes() {
    let f = fixtures::nav_dop(10);
    let mut s = Splitter::new();
    assert!(s.push(&f[..10]).is_empty());
    assert_eq!(s.push(&f[10..]).len(), 1);
    assert_eq!(s.garbage(), 0);
}
