use ados_protocol::pilot::SensingTier;
use ados_protocol::voxmap::{
    VoxSnapshot, VoxTier, VoxmapMeta, VoxmapWriter, ESDF_CAP_DM, VOX_FREE, VOX_OCCUPIED,
    VOX_UNKNOWN,
};

use super::*;

const RES: f32 = 0.2;
const DIMS: [u16; 3] = [100, 100, 20];

/// A 20 × 20 × 4 m map at 0.2 m, window (0, 0, 0) at map point
/// (-10, -10, -6) (down −6 m to −2 m), every voxel `fill`, the body at map
/// `body`.
fn snap(seq: u64, tier: VoxTier, fill: u8, body: [f32; 3]) -> VoxSnapshot {
    let n = DIMS.iter().map(|&d| d as usize).product();
    VoxSnapshot {
        valid: true,
        seq,
        meta: VoxmapMeta {
            stamp_ns: 0,
            res_m: RES,
            origin_cell: [-50, -50, -30],
            dims: DIMS,
            map_from_body: [body[0], body[1], body[2], 1.0, 0.0, 0.0, 0.0],
            tier,
        },
        occupancy: vec![fill; n],
        esdf_dm: vec![ESDF_CAP_DM; n],
    }
}

/// Set the voxel holding map point `p`.
fn set(s: &mut VoxSnapshot, p: [f32; 3], v: u8) {
    let i = s.index_of_point(p).expect("inside the window");
    s.occupancy[i] = v;
    if v == VOX_OCCUPIED {
        s.esdf_dm[i] = 0;
    }
}

/// A trunk: every voxel of the column holding map point `ne`.
fn column(s: &mut VoxSnapshot, ne: [f32; 2]) {
    for k in 0..DIMS[2] {
        set(
            s,
            [ne[0], ne[1], -6.0 + (f32::from(k) + 0.5) * RES],
            VOX_OCCUPIED,
        );
    }
}

/// A world holding `s`, the vehicle at local (0, 0, −4) and at map `body`.
fn world(s: &VoxSnapshot) -> VoxWorld {
    let mut w = VoxWorld::default();
    assert!(w.offer(s, Some([0.0, 0.0, -4.0]), 1_000));
    w
}

#[test]
fn a_map_is_live_only_while_its_seq_advances() {
    let s = snap(2, VoxTier::Lidar3d, VOX_FREE, [0.0, 0.0, -4.0]);
    let mut w = world(&s);
    assert!(w.live(1_000) && w.live(1_300) && w.live_3d(1_300));
    assert!(!w.live(1_301), "300 ms without a new seq is stale");
    // The same publication again does not refresh it.
    assert!(!w.offer(&s, Some([0.0, 0.0, -4.0]), 1_200));
    assert!(!w.live(1_301));
    assert!(w.offer(
        &snap(4, VoxTier::Lidar3d, VOX_FREE, [0.0, 0.0, -4.0]),
        Some([0.0, 0.0, -4.0]),
        1_250
    ));
    assert!(w.live(1_550) && !w.live(1_551));
    // No local position: not tied to the frame, not live.
    assert!(w.offer(
        &snap(6, VoxTier::Lidar3d, VOX_FREE, [0.0, 0.0, -4.0]),
        None,
        1_600
    ));
    assert!(!w.live(1_600));
    // A torn copy is never taken.
    let mut torn = snap(8, VoxTier::Lidar3d, VOX_FREE, [0.0, 0.0, -4.0]);
    torn.valid = false;
    assert!(!w.offer(&torn, Some([0.0; 3]), 1_700));
}

#[test]
fn the_tier_is_the_maps_and_a_planar_map_is_not_3d() {
    for (vt, st) in [
        (VoxTier::Lidar3d, SensingTier::Lidar3d),
        (VoxTier::Depth, SensingTier::Depth),
        (VoxTier::Camera, SensingTier::Camera),
        (VoxTier::Planar, SensingTier::Planar),
        (VoxTier::Unknown, SensingTier::Planar),
    ] {
        let w = world(&snap(2, vt, VOX_FREE, [0.0; 3]));
        assert_eq!(w.tier(), st);
        assert_eq!(w.live_3d(1_000), st != SensingTier::Planar);
    }
}

#[test]
fn unknown_space_is_never_free() {
    // The map frame sits 5 m north of the local one.
    let mut s = snap(2, VoxTier::Lidar3d, VOX_UNKNOWN, [5.0, 0.0, -4.0]);
    set(&mut s, [7.1, 0.1, -3.9], VOX_FREE);
    set(&mut s, [8.1, 0.1, -3.9], VOX_OCCUPIED);
    let w = world(&s);
    // Local (2.1, 0.1) is map (7.1, 0.1).
    assert!(w.known_free([2.1, 0.1, -3.9]));
    assert!(w.clearance([2.1, 0.1, -3.9]) > 0.0);
    assert!(w.occupied([3.1, 0.1, -3.9]));
    assert!(!w.known_free([3.1, 0.1, -3.9]));
    assert_eq!(w.clearance([3.1, 0.1, -3.9]), 0.0);
    // Unknown, and outside the window.
    assert!(!w.known_free([1.1, 0.1, -3.9]) && !w.occupied([1.1, 0.1, -3.9]));
    assert_eq!(w.clearance([1.1, 0.1, -3.9]), 0.0);
    assert!(!w.known_free([40.1, 0.1, -3.9]));
}

#[test]
fn a_ray_stops_at_the_first_voxel_not_seen_free() {
    let mut s = snap(2, VoxTier::Lidar3d, VOX_FREE, [0.0, 0.0, -4.0]);
    column(&mut s, [5.1, 0.1]);
    set(&mut s, [0.1, -3.1, -3.9], VOX_UNKNOWN);
    let w = world(&s);
    let RayHit::Occupied(t) = w.ray([0.1, 0.1, -3.9], [9.0, 0.1, -3.9]) else {
        panic!("the trunk");
    };
    assert!((t - 4.9).abs() < 0.11, "{t}");
    let RayHit::Unknown(t) = w.ray([0.1, 0.1, -3.9], [0.1, -9.0, -3.9]) else {
        panic!("the unknown voxel");
    };
    assert!((t - 3.1).abs() < 0.11, "{t}");
    assert!(matches!(
        w.ray([0.1, 0.1, -3.9], [0.1, 30.0, -3.9]),
        RayHit::Outside(t) if (t - 9.9).abs() < 0.11
    ));
    assert_eq!(w.ray([0.1, 0.1, -3.9], [-5.0, 2.0, -3.1]), RayHit::Clear);
}

#[test]
fn the_slab_marks_only_columns_occupied_inside_it() {
    let mut s = snap(2, VoxTier::Lidar3d, VOX_FREE, [0.0, 0.0, -4.0]);
    set(&mut s, [3.1, 1.1, -4.1], VOX_OCCUPIED);
    // A branch 1.5 m over the vehicle.
    set(&mut s, [-2.1, 0.1, -5.5], VOX_OCCUPIED);
    let w = world(&s);
    let mut cells = Vec::new();
    w.slice(
        Band::around(-4.0, 0.33, 0.5, None, 0.4).unwrap(),
        &mut cells,
    );
    assert_eq!(cells.len(), 1);
    assert!(geo_close(cells[0], [3.1, 1.1]), "{cells:?}");
    w.slice(Band::around(-5.5, 0.2, 0.2, None, 0.4).unwrap(), &mut cells);
    assert_eq!(cells.len(), 1);
    assert!(geo_close(cells[0], [-2.1, 0.1]), "{cells:?}");
}

#[test]
fn the_ground_cut_leaves_the_floor_out_of_the_slab() {
    // The slab would reach 0.5 m under the ground; the cut ends it 0.4 m
    // over it.
    assert_eq!(
        Band::around(-1.0, 0.33, 1.5, Some(0.0), 0.4).map(|b| b.bottom_d),
        Some(-0.4)
    );
    assert_eq!(
        Band::around(-2.0, 0.33, 0.5, Some(0.0), 0.4).map(|b| b.bottom_d),
        Some(-1.5)
    );
    assert!(Band::around(-0.1, 0.2, 0.2, Some(0.0), 0.4).is_none());
}

#[test]
fn the_scan_reads_walls_and_where_seen_space_ends() {
    let mut s = snap(2, VoxTier::Depth, VOX_FREE, [0.0, 0.0, -4.0]);
    column(&mut s, [3.1, 0.1]);
    // South of the vehicle the camera never looked.
    for i in 0..35 {
        set(&mut s, [-2.1 - i as f32 * RES, 0.1, -3.9], VOX_UNKNOWN);
    }
    let w = world(&s);
    let walls = Band::around(-4.0, 0.33, 0.5, None, 0.4);
    let seen = Band::around(-4.0, 0.2, 0.2, None, 0.4);
    let bins = w.scan([0.1, 0.1, -3.9], 0.0, walls, seen, 20.0);
    assert_eq!(bins.len(), WORLD_SCAN_BINS);
    let ahead = bins[0];
    assert!((ahead.hit_m.unwrap() - 2.9).abs() < 0.11, "{ahead:?}");
    let behind = bins[180];
    assert!(
        behind.hit_m.is_none() && (behind.seen_m - 2.1).abs() < 0.11,
        "{behind:?}"
    );
    // East: free to the window's edge.
    let east = bins[90];
    assert!(
        east.hit_m.is_none() && (east.seen_m - 9.9).abs() < 0.11,
        "{east:?}"
    );
    // Yawed 90°, bin 0 looks east.
    let yawed = w.scan(
        [0.1, 0.1, -3.9],
        std::f32::consts::FRAC_PI_2,
        walls,
        seen,
        20.0,
    );
    assert_eq!(yawed[0], east);
}

#[test]
fn an_ekf_reset_keeps_the_map_where_it_is() {
    let mut s = snap(2, VoxTier::Lidar3d, VOX_FREE, [0.0, 0.0, -4.0]);
    set(&mut s, [3.1, 0.1, -3.9], VOX_OCCUPIED);
    let mut w = world(&s);
    assert!(w.occupied([3.1, 0.1, -3.9]));
    // The local estimate jumps 10 m north with the vehicle still.
    w.shift([10.0, 0.0]);
    assert!(w.occupied([13.1, 0.1, -3.9]));
    assert!(!w.occupied([3.1, 0.1, -3.9]));
}

#[test]
fn the_source_reads_each_publication_once() {
    // The writer takes only a directory it owns that nobody else may write,
    // under a parent only its owner may write (whatever the umask).
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), PermissionsExt::from_mode(0o700)).unwrap();
    let dir = root.path().join("voxmap");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o750)).unwrap();
    let path = dir.join("map-v1");
    let mut writer = VoxmapWriter::create(&path, DIMS, None).unwrap();
    let mut src = VoxSource::new(path.clone(), false);
    assert!(src.read(0).is_none(), "nothing published yet");
    let s = snap(0, VoxTier::Lidar3d, VOX_FREE, [1.0, 2.0, -4.0]);
    writer
        .publish(&s.meta, |occ, esdf| {
            occ.copy_from_slice(&s.occupancy);
            esdf.copy_from_slice(&s.esdf_dm);
        })
        .unwrap();
    let got = src.read(20).expect("the publication");
    assert_eq!(got.meta.map_from_body[..3], [1.0, 2.0, -4.0]);
    let seq = got.seq;
    assert!(src.read(40).is_none(), "nothing new");
    writer.publish(&s.meta, |_, _| {}).unwrap();
    assert!(src.read(60).is_some_and(|g| g.seq > seq));
    // The trusted open takes the file from the owner its directory records
    // (this process, here), and never from a directory others may write.
    assert!(VoxSource::new(path.clone(), true).read(0).is_some());
    std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o770)).unwrap();
    assert!(VoxSource::new(path, true).read(0).is_none());
}

fn geo_close(a: [f32; 2], b: [f32; 2]) -> bool {
    (a[0] - b[0]).abs() < 0.11 && (a[1] - b[1]).abs() < 0.11
}
