use super::*;

use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const DIMS: [u16; 3] = [20, 16, 8];

fn meta(stamp_ns: u64) -> VoxmapMeta {
    VoxmapMeta {
        stamp_ns,
        res_m: 0.15,
        origin_cell: [-10, 7, -4],
        dims: DIMS,
        map_from_body: [1.0, -2.0, -3.5, 1.0, 0.0, 0.0, 0.0],
        tier: VoxTier::Lidar3d,
    }
}

fn path_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("ados-voxmap-v1")
}

#[test]
fn header_fields_sit_at_their_documented_offsets() {
    let mut buf = [0u8; HEADER_LEN];
    meta(0x0102_0304_0506_0708).encode(9, &mut buf);
    assert_eq!(&buf[0..4], b"ADVX");
    assert_eq!(u16::from_le_bytes([buf[4], buf[5]]), 1);
    assert_eq!(buf[8], 9);
    assert_eq!(buf[16], 0x08);
    assert_eq!(f32::from_le_bytes(buf[24..28].try_into().unwrap()), 0.15);
    assert_eq!(i32::from_le_bytes(buf[28..32].try_into().unwrap()), -10);
    assert_eq!(i32::from_le_bytes(buf[36..40].try_into().unwrap()), -4);
    assert_eq!(u16::from_le_bytes([buf[40], buf[41]]), 20);
    assert_eq!(u16::from_le_bytes([buf[44], buf[45]]), 8);
    assert_eq!(f32::from_le_bytes(buf[56..60].try_into().unwrap()), -3.5);
    assert_eq!(f32::from_le_bytes(buf[60..64].try_into().unwrap()), 1.0);
    assert_eq!(buf[76], 1);
    let (seq, back) = VoxmapMeta::decode(&buf).unwrap();
    assert_eq!(seq, 9);
    assert_eq!(back, meta(0x0102_0304_0506_0708));
    assert_eq!(file_len(DIMS), 80 + 2 * 20 * 16 * 8);
}

#[test]
fn malformed_headers_are_refused() {
    let mut buf = [0u8; HEADER_LEN];
    meta(1).encode(2, &mut buf);
    let mut bad = buf;
    bad[0] = b'X';
    assert!(matches!(
        VoxmapMeta::decode(&bad),
        Err(VoxmapError::BadMagic)
    ));
    let mut bad = buf;
    bad[4] = 7;
    assert!(matches!(
        VoxmapMeta::decode(&bad),
        Err(VoxmapError::BadVersion(7))
    ));
    let mut bad = buf;
    bad[40] = 0;
    assert!(matches!(
        VoxmapMeta::decode(&bad),
        Err(VoxmapError::BadDims(_))
    ));
}

#[test]
fn a_publication_round_trips_and_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_in(&dir);
    let mut w = VoxmapWriter::create(&path, DIMS, None).unwrap();
    let mut r = VoxmapReader::open(&path).unwrap();
    let mut snap = VoxSnapshot::default();
    assert_eq!(r.read_into(&mut snap).unwrap(), ReadStatus::Stale);
    assert!(!snap.valid);

    let seq = w
        .publish(&meta(42), |occ, esdf| {
            for (i, (o, e)) in occ.iter_mut().zip(esdf.iter_mut()).enumerate() {
                *o = (i % 3) as u8;
                *e = (i % 41) as u8;
            }
        })
        .unwrap();
    assert_eq!(seq, 2);
    assert_eq!(r.read_into(&mut snap).unwrap(), ReadStatus::Fresh);
    assert!(snap.valid);
    assert_eq!(snap.seq, 2);
    assert_eq!(snap.meta, meta(42));
    assert_eq!(snap.occupancy.len(), 20 * 16 * 8);
    assert!(snap
        .occupancy
        .iter()
        .enumerate()
        .all(|(i, &o)| o == (i % 3) as u8));
    assert!(snap
        .esdf_dm
        .iter()
        .enumerate()
        .all(|(i, &e)| e == (i % 41) as u8));
    assert_eq!(snap.age_ns(50), 8);
    // Window voxel (1, 0, 0) is the second byte; map point of absolute voxel
    // (-9, 7, -4) lands there.
    assert_eq!(snap.index([1, 0, 0]), Some(1));
    assert_eq!(snap.index([20, 0, 0]), None);
    assert_eq!(
        snap.index_of_point([-9.0 * 0.15 + 0.01, 7.0 * 0.15 + 0.01, -0.59]),
        Some(1)
    );

    assert_eq!(r.read_into(&mut snap).unwrap(), ReadStatus::Stale);
    w.publish(&meta(43), |_, _| {}).unwrap();
    assert_eq!(r.read_into(&mut snap).unwrap(), ReadStatus::Fresh);
    assert_eq!(snap.meta.stamp_ns, 43);
    assert_eq!(snap.seq, 4);
}

#[test]
fn a_restarted_writer_continues_the_guard_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_in(&dir);
    let mut w = VoxmapWriter::create(&path, DIMS, None).unwrap();
    w.publish(&meta(1), |_, _| {}).unwrap();
    w.publish(&meta(2), |_, _| {}).unwrap();
    drop(w);
    let w = VoxmapWriter::create(&path, DIMS, None).unwrap();
    assert_eq!(w.seq(), 4);
}

#[test]
fn a_writer_stuck_mid_publish_reads_as_torn() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_in(&dir);
    let mut w = VoxmapWriter::create(&path, DIMS, None).unwrap();
    w.publish(&meta(1), |_, _| {}).unwrap();
    let raw = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    raw.write_all_at(&3u64.to_le_bytes(), offset::SEQ as u64)
        .unwrap();
    let mut r = VoxmapReader::open(&path).unwrap();
    let mut snap = VoxSnapshot::default();
    assert_eq!(r.read_into(&mut snap).unwrap(), ReadStatus::Torn);
    assert!(!snap.valid);
}

#[test]
fn wrong_dims_and_short_files_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_in(&dir);
    let mut w = VoxmapWriter::create(&path, DIMS, None).unwrap();
    let mut other = meta(1);
    other.dims = [4, 4, 4];
    assert!(matches!(
        w.publish(&other, |_, _| {}),
        Err(VoxmapError::DimsMismatch { .. })
    ));
    w.publish(&meta(1), |_, _| {}).unwrap();
    let raw = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    raw.set_len(100).unwrap();
    let mut r = VoxmapReader::open(&path).unwrap();
    let mut snap = VoxSnapshot::default();
    assert!(matches!(
        r.read_into(&mut snap),
        Err(VoxmapError::Truncated { got: 100, .. })
    ));
}

#[test]
fn trust_requires_the_group_and_an_owner_only_write_bit() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_in(&dir);
    let gid = nix::unistd::getegid();
    let Some(group) = nix::unistd::Group::from_gid(gid).ok().flatten() else {
        return;
    };
    let mut w = VoxmapWriter::create(&path, DIMS, Some(&group.name)).unwrap();
    w.publish(&meta(5), |_, _| {}).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
    assert!(VoxmapReader::open_trusted(&path, &group.name).is_ok());

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
    assert!(matches!(
        VoxmapReader::open_trusted(&path, &group.name),
        Err(VoxmapError::Untrusted(_))
    ));
    assert!(matches!(
        VoxmapReader::open_trusted(&path, "ados-no-such-group-xyz"),
        Err(VoxmapError::NoSuchGroup(_))
    ));
}

#[test]
fn symlinks_are_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    VoxmapWriter::create(&real, DIMS, None).unwrap();
    let link = path_in(&dir);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(VoxmapReader::open(&link).is_err());
    assert!(VoxmapWriter::create(&link, DIMS, None).is_err());
}

/// A writer publishing in bursts (back to back, then paced) while a reader
/// copies: every copy the reader keeps must be one publication whole (both
/// planes uniform and equal to the stamp), never a mix of two.
#[test]
fn concurrent_reads_never_keep_a_torn_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_in(&dir);
    let dims = [64, 64, 32];
    let mut w = VoxmapWriter::create(&path, dims, None).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let writer_done = done.clone();
    let writer = std::thread::spawn(move || {
        for k in 1..=400u64 {
            let mut m = meta(k);
            m.dims = dims;
            m.origin_cell = [k as i32, 0, 0];
            w.publish(&m, |occ, esdf| {
                occ.fill(k as u8);
                esdf.fill(!(k as u8));
            })
            .unwrap();
            if k % 2 == 0 {
                std::thread::sleep(std::time::Duration::from_micros(300));
            }
        }
        writer_done.store(true, Ordering::SeqCst);
    });

    let mut r = VoxmapReader::open(&path).unwrap();
    let mut snap = VoxSnapshot::default();
    let (mut fresh, mut torn, mut last) = (0u32, 0u32, 0u64);
    while !done.load(Ordering::SeqCst) || fresh == 0 {
        match r.read_into(&mut snap).unwrap() {
            ReadStatus::Fresh => {
                fresh += 1;
                let k = snap.meta.stamp_ns;
                assert!(k > last, "publications arrive in order");
                last = k;
                assert_eq!(snap.meta.origin_cell[0], k as i32);
                assert!(
                    snap.occupancy.iter().all(|&b| b == k as u8),
                    "torn occupancy"
                );
                assert!(snap.esdf_dm.iter().all(|&b| b == !(k as u8)), "torn esdf");
            }
            ReadStatus::Torn => torn += 1,
            ReadStatus::Stale => {}
        }
    }
    writer.join().unwrap();
    assert!(fresh >= 20, "the reader copied {fresh} publications");
    eprintln!("fresh {fresh}, torn {torn}");
}
