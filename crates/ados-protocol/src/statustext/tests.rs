use super::*;

fn chunk(s: &str) -> [u8; CHUNK_LEN] {
    let mut b = [0u8; CHUNK_LEN];
    b[..s.len()].copy_from_slice(s.as_bytes());
    b
}

const LONG: &str = "PreArm: MAV_GCS_SYSID ('1') and SYSID_MYGCS ('255') must not be set; use MAVn_OPTIONS bit 1 instead now";

fn parts() -> Vec<&'static str> {
    vec![&LONG[..50], &LONG[50..100], &LONG[100..]]
}

#[test]
fn three_tagged_chunks_make_one_message() {
    let mut a = StatusTextAssembler::new();
    let p = parts();
    assert_eq!(a.push(1, 1, 3, &chunk(p[0]), 7, 0, 0), None);
    assert_eq!(a.push(1, 1, 3, &chunk(p[1]), 7, 1, 10), None);
    assert_eq!(
        a.push(1, 1, 3, &chunk(p[2]), 7, 2, 20),
        Some((3, LONG.to_owned()))
    );
    assert!(a.flush_stale(5000).is_empty());
}

#[test]
fn an_id_zero_message_passes_straight_through() {
    let mut a = StatusTextAssembler::new();
    assert_eq!(
        a.push(1, 1, 4, &chunk("PreArm: Battery low"), 0, 0, 0),
        Some((4, "PreArm: Battery low".into()))
    );
    // A full 50-byte id-0 text is complete too.
    let full = &LONG[..50];
    assert_eq!(
        a.push(1, 1, 4, &chunk(full), 0, 0, 0),
        Some((4, full.trim().to_owned()))
    );
}

#[test]
fn a_stale_partial_is_flushed_after_a_second() {
    let mut a = StatusTextAssembler::new();
    assert_eq!(a.push(1, 1, 3, &chunk(&LONG[..50]), 9, 0, 100), None);
    assert!(a.flush_stale(1099).is_empty());
    assert_eq!(a.flush_stale(1100), vec![(3, LONG[..50].to_owned())]);
    assert!(a.flush_stale(5000).is_empty());
}

#[test]
fn interleaved_senders_stay_separate() {
    let mut a = StatusTextAssembler::new();
    let p = parts();
    let other = "Compass not calibrated, please calibrate compass n";
    assert_eq!(a.push(1, 1, 3, &chunk(p[0]), 7, 0, 0), None);
    assert_eq!(a.push(2, 1, 3, &chunk(other), 7, 0, 0), None);
    assert_eq!(a.push(1, 1, 3, &chunk(p[1]), 7, 1, 0), None);
    assert_eq!(
        a.push(2, 1, 3, &chunk("ow"), 7, 1, 0),
        Some((3, format!("{other}ow")))
    );
    assert_eq!(
        a.push(1, 1, 3, &chunk(p[2]), 7, 2, 0),
        Some((3, LONG.to_owned()))
    );
}

#[test]
fn chunks_arriving_out_of_order_join_in_sequence() {
    let mut a = StatusTextAssembler::new();
    let p = parts();
    assert_eq!(a.push(1, 1, 3, &chunk(p[1]), 7, 1, 0), None);
    assert_eq!(a.push(1, 1, 3, &chunk(p[0]), 7, 0, 0), None);
    assert_eq!(
        a.push(1, 1, 3, &chunk(p[2]), 7, 2, 0),
        Some((3, LONG.to_owned()))
    );
}

#[test]
fn untagged_chunks_join_until_a_short_one() {
    let mut a = StatusTextAssembler::new();
    let p = parts();
    assert!(a.push_untagged(1, 1, 3, &chunk(p[0]), 0).is_empty());
    assert!(a.push_untagged(1, 1, 3, &chunk(p[1]), 0).is_empty());
    assert_eq!(
        a.push_untagged(1, 1, 3, &chunk(p[2]), 0),
        vec![(3, LONG.to_owned())]
    );
    // A short text is complete on its own.
    assert_eq!(
        a.push_untagged(1, 1, 4, &chunk("Arm: ok"), 0),
        vec![(4, "Arm: ok".into())]
    );
}

#[test]
fn an_untagged_chunk_at_another_severity_ends_the_pending_text() {
    let mut a = StatusTextAssembler::new();
    assert!(a.push_untagged(1, 1, 3, &chunk(&LONG[..50]), 0).is_empty());
    assert_eq!(
        a.push_untagged(1, 1, 6, &chunk("Info"), 0),
        vec![(3, LONG[..50].to_owned()), (6, "Info".into())]
    );
}
