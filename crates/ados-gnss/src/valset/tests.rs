use super::*;
use crate::fixtures::unframe;

/// Decode a VALSET frame into (layers, [(key, value bytes)]).
fn decode(frame: &[u8]) -> (u8, Vec<(u32, Vec<u8>)>) {
    let (class, id, p) = unframe(frame);
    assert_eq!((class, id), (0x06, 0x8A));
    assert_eq!(p[0], 0x00, "VALSET version 0");
    assert_eq!(&p[2..4], &[0, 0]);
    let mut items = Vec::new();
    let mut o = 4;
    while o < p.len() {
        let key = u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
        let n = value_len(key);
        items.push((key, p[o + 4..o + 4 + n].to_vec()));
        o += 4 + n;
    }
    (p[1], items)
}

fn u32le(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

#[test]
fn survey_in_sets_mode_duration_accuracy_and_outputs() {
    let (layers, items) = decode(&survey_in(RxPort::Uart1, 300, 2.0));
    assert_eq!(layers, LAYER_RAM);
    assert_eq!(items[0], (0x2003_0001, vec![1]));
    assert_eq!(items[1], (0x4003_0010, u32le(300)));
    assert_eq!(items[2], (0x4003_0011, u32le(20_000)));
    let outputs: Vec<u32> = items[3..].iter().map(|(k, _)| *k).collect();
    assert_eq!(
        outputs,
        vec![
            0x2091_02BE, // RTCM 1005 UART1
            0x2091_035F, // 1074
            0x2091_0364, // 1084
            0x2091_0369, // 1094
            0x2091_036E, // 1124
            0x2091_0304, // 1230
            0x2091_0089, // NAV-SVIN UART1
        ]
    );
    assert!(items[3..].iter().all(|(_, v)| v == &vec![1]));
}

#[test]
fn usb_port_selects_usb_output_keys() {
    let (_, items) = decode(&survey_in(RxPort::Usb, 60, 0.5));
    assert_eq!(items[2], (0x4003_0011, u32le(5_000)));
    let outputs: Vec<u32> = items[3..].iter().map(|(k, _)| *k).collect();
    assert_eq!(
        outputs,
        vec![
            0x2091_02C0,
            0x2091_0361,
            0x2091_0366,
            0x2091_036B,
            0x2091_0370,
            0x2091_0306,
            0x2091_008B
        ]
    );
    assert_eq!(
        RTCM_MSGOUT.map(|(m, _, _)| m),
        [1005, 1074, 1084, 1094, 1124, 1230]
    );
}

#[test]
fn fixed_sets_llh_with_high_precision_parts() {
    let (layers, items) = decode(&fixed(
        RxPort::Uart1,
        47.123_456_789,
        -122.987_654_321,
        512.3456,
    ));
    assert_eq!(layers, LAYER_RAM);
    let i = |v: i32| v.to_le_bytes().to_vec();
    assert_eq!(items[0], (TMODE_MODE, vec![2]));
    assert_eq!(items[1], (TMODE_POS_TYPE, vec![1]));
    assert_eq!(items[2], (0x4003_0009, i(471_234_567)));
    assert_eq!(items[3], (0x4003_000A, i(-1_229_876_543)));
    assert_eq!(items[4], (0x4003_000B, i(51_234)));
    assert_eq!(items[5], (0x2003_000C, vec![89]));
    assert_eq!(items[6], (0x2003_000D, vec![(-21i8) as u8]));
    assert_eq!(items[7], (0x2003_000E, vec![56]));
    assert_eq!(items.len(), 8 + 7);
}

#[test]
fn off_and_mon_ver_poll() {
    let (_, items) = decode(&off());
    assert_eq!(items, vec![(TMODE_MODE, vec![0])]);
    assert_eq!(
        poll_mon_ver(),
        vec![0xB5, 0x62, 0x0A, 0x04, 0x00, 0x00, 0x0E, 0x34]
    );
}
