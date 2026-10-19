use std::fs;
use std::path::{Path, PathBuf};

use super::*;

/// Reads `<mount>/.fake-space` (`<total> <free>`), the stand-in for statvfs.
fn fake_stat(mount: &Path) -> Option<FsSpace> {
    let text = fs::read_to_string(mount.join(".fake-space")).ok()?;
    let mut parts = text.split_whitespace().map(|p| p.parse::<u64>().ok());
    Some(FsSpace {
        total_bytes: parts.next()??,
        free_bytes: parts.next()??,
    })
}

struct Fake {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    roots: SysRoots,
    mounts: Vec<String>,
}

impl Fake {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        fs::create_dir_all(base.join("sys/block")).unwrap();
        let roots = SysRoots {
            sys_block: base.join("sys/block"),
            mounts: base.join("mounts"),
            fallback_root: base.join("var/ados/capture"),
            stat: fake_stat,
        };
        Fake {
            _tmp: tmp,
            base,
            roots,
            mounts: Vec::new(),
        }
    }

    fn disk(&self, disk: &str, rotational: &str, parts: &[&str]) {
        let d = self.roots.sys_block.join(disk);
        fs::create_dir_all(d.join("queue")).unwrap();
        fs::write(d.join("queue/rotational"), format!("{rotational}\n")).unwrap();
        for p in parts {
            fs::create_dir_all(d.join(p)).unwrap();
        }
    }

    /// Mount `device` (fs `fs`) at `<base>/<at>` with the given space.
    fn mount(&mut self, device: &str, at: &str, fs_type: &str, total: u64, free: u64) -> PathBuf {
        let mount = self.base.join(at);
        fs::create_dir_all(&mount).unwrap();
        fs::write(mount.join(".fake-space"), format!("{total} {free}")).unwrap();
        self.mounts.push(format!(
            "{device} {} {fs_type} rw,relatime 0 0",
            mount.display()
        ));
        fs::write(&self.roots.mounts, self.mounts.join("\n")).unwrap();
        mount
    }
}

fn usage(used: u64, budget: u64, sessions: u64, uploaded: u64, evict: bool) -> CaptureUsage {
    CaptureUsage {
        used_bytes: used,
        budget_bytes: budget,
        budget_pct: 85,
        floor_free_bytes: 5 * GIB,
        sessions,
        uploaded,
        evict_unuploaded: evict,
        state: CaptureState::Ok,
    }
}

fn volume(free: u64) -> Volume {
    Volume {
        mount: "/mnt/nvme".into(),
        device: "nvme0n1p1".into(),
        fs: "ext4".into(),
        total_bytes: 1000 * GIB,
        free_bytes: free,
        rotational: false,
    }
}

#[test]
fn auto_root_picks_an_nvme_volume_with_room() {
    let mut f = Fake::new();
    f.disk("mmcblk0", "0", &["mmcblk0p2"]);
    f.disk("nvme0n1", "0", &["nvme0n1p1"]);
    f.mount("/dev/mmcblk0p2", "root", "ext4", 64 * GIB, 40 * GIB);
    let nvme = f.mount("/dev/nvme0n1p1", "mnt/nvme", "ext4", 1000 * GIB, 800 * GIB);
    let root = resolve_capture_root(&CaptureStorageConfig::default(), &f.roots);
    assert_eq!(root, nvme.join("ados/capture"));
}

#[test]
fn auto_root_falls_back_without_a_roomy_nvme() {
    let mut f = Fake::new();
    f.disk("nvme0n1", "0", &["nvme0n1p1"]);
    f.mount("/dev/nvme0n1p1", "mnt/nvme", "ext4", 128 * GIB, 63 * GIB);
    let cfg = CaptureStorageConfig::default();
    assert_eq!(resolve_capture_root(&cfg, &f.roots), f.roots.fallback_root);

    // A rotational "nvme" (nonsense, but sysfs is the authority) is skipped too.
    let mut g = Fake::new();
    g.disk("nvme0n1", "1", &["nvme0n1p1"]);
    g.mount("/dev/nvme0n1p1", "mnt/nvme", "ext4", 1000 * GIB, 900 * GIB);
    assert_eq!(resolve_capture_root(&cfg, &g.roots), g.roots.fallback_root);

    // An unmounted NVMe disk is not a capture root.
    let mut h = Fake::new();
    h.disk("nvme0n1", "0", &["nvme0n1p1"]);
    h.mount("/dev/mmcblk0p2", "root", "ext4", 64 * GIB, 40 * GIB);
    assert_eq!(resolve_capture_root(&cfg, &h.roots), h.roots.fallback_root);
}

#[test]
fn an_explicit_root_is_taken_as_given() {
    let f = Fake::new();
    let cfg = CaptureStorageConfig {
        capture_root: "/data/capture".into(),
        ..CaptureStorageConfig::default()
    };
    assert_eq!(
        resolve_capture_root(&cfg, &f.roots),
        PathBuf::from("/data/capture")
    );
}

#[test]
fn volume_listing_skips_pseudo_filesystems() {
    let mut f = Fake::new();
    f.disk("nvme0n1", "0", &["nvme0n1p1"]);
    f.disk("sda", "1", &["sda1"]);
    f.mount("/dev/nvme0n1p1", "mnt/nvme", "ext4", 1000 * GIB, 800 * GIB);
    f.mount("/dev/sda1", "mnt/usb", "vfat", 32 * GIB, 30 * GIB);
    f.mount("tmpfs", "run", "tmpfs", GIB, GIB);
    f.mount("proc", "proc", "proc", 0, 0);
    f.mount("overlay", "overlay", "overlay", GIB, GIB);
    f.mount("/dev/loop0", "snap", "squashfs", GIB, 0);
    // A bind mount of an already-listed device.
    f.mount("/dev/nvme0n1p1", "bind", "ext4", 1000 * GIB, 800 * GIB);

    let vols = list_volumes(&f.roots);
    let devices: Vec<&str> = vols.iter().map(|v| v.device.as_str()).collect();
    assert_eq!(devices, ["nvme0n1p1", "sda1"]);
    assert!(!vols[0].rotational);
    assert!(vols[1].rotational);
    assert_eq!(vols[0].fs, "ext4");
    assert_eq!(vols[0].free_bytes, 800 * GIB);

    let inside = Path::new(&vols[0].mount).join("ados/capture/s1");
    assert_eq!(volume_for(&inside, &vols).unwrap().device, "nvme0n1p1");
}

#[test]
fn mount_fields_are_unescaped() {
    assert_eq!(volumes::unescape("/mnt/my\\040disk"), "/mnt/my disk");
    assert_eq!(volumes::unescape("/plain"), "/plain");
    assert_eq!(volumes::unescape("/trail\\04"), "/trail\\04");
}

#[test]
fn admission_refuses_under_the_floor() {
    let u = usage(10 * GIB, 850 * GIB, 3, 1, false);
    let below = volume(5 * GIB - GIB / 10);
    let err = admit_new_session(&u, &below).unwrap_err();
    assert_eq!(err.code(), E_STORAGE_FULL);
    assert!(err.to_string().starts_with("E_STORAGE_FULL"));
    assert_eq!(capture_state(&u, &below), CaptureState::Full);

    let above = volume(5 * GIB + GIB / 10);
    assert!(admit_new_session(&u, &above).is_ok());
    assert_eq!(capture_state(&u, &above), CaptureState::NearFull);
}

#[test]
fn admission_over_budget_depends_on_what_can_be_evicted() {
    let v = volume(100 * GIB);
    // Nothing uploaded and eviction of un-uploaded sessions off: full.
    assert!(admit_new_session(&usage(900 * GIB, 850 * GIB, 4, 0, false), &v).is_err());
    // An uploaded session can make room.
    assert!(admit_new_session(&usage(900 * GIB, 850 * GIB, 4, 1, false), &v).is_ok());
    // So can an un-uploaded one when the flag allows it.
    assert!(admit_new_session(&usage(900 * GIB, 850 * GIB, 4, 0, true), &v).is_ok());
}

#[test]
fn state_thresholds() {
    let roomy = volume(500 * GIB);
    assert_eq!(
        capture_state(&usage(100 * GIB, 850 * GIB, 1, 0, false), &roomy),
        CaptureState::Ok
    );
    assert_eq!(
        capture_state(&usage(765 * GIB, 850 * GIB, 1, 1, false), &roomy),
        CaptureState::NearFull,
        "90 % of the budget"
    );
    assert_eq!(
        capture_state(&usage(100 * GIB, 850 * GIB, 1, 0, false), &volume(9 * GIB)),
        CaptureState::NearFull,
        "under twice the floor"
    );
}

#[test]
fn budget_and_floor_arithmetic() {
    let cfg = CaptureStorageConfig::default();
    assert_eq!(cfg.floor_free_bytes(), 5 * GIB);
    assert_eq!(cfg.budget_bytes(1000), 850);
    assert_eq!(
        cfg.budget_bytes(u64::MAX),
        (u64::MAX as u128 * 85 / 100) as u64
    );
}

#[test]
fn sidecar_field_names_match_the_contract() {
    let sidecar = CaptureStorageSidecar {
        version: SIDECAR_VERSION,
        updated_ns: 1,
        capture_root: "/mnt/nvme/ados/capture".into(),
        volume: Some(volume(800 * GIB)),
        capture: usage(1, 2, 3, 4, false),
        write_probe: Some(WriteProbe {
            mbps: 1450.2,
            measured_at_ns: 120_000_000_000,
        }),
    };
    let v = serde_json::to_value(&sidecar).unwrap();
    let keys = |v: &serde_json::Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&v),
        [
            "capture",
            "capture_root",
            "updated_ns",
            "version",
            "volume",
            "write_probe"
        ]
    );
    assert_eq!(
        keys(&v["volume"]),
        [
            "device",
            "free_bytes",
            "fs",
            "mount",
            "rotational",
            "total_bytes"
        ]
    );
    assert_eq!(
        keys(&v["capture"]),
        [
            "budget_bytes",
            "budget_pct",
            "evict_unuploaded",
            "floor_free_bytes",
            "sessions",
            "state",
            "uploaded",
            "used_bytes"
        ]
    );
    assert_eq!(keys(&v["write_probe"]), ["mbps", "measured_at_ns"]);
    assert_eq!(v["capture"]["state"], "ok");
    // `version` leads the document.
    let text = serde_json::to_string(&sidecar).unwrap();
    assert!(text.starts_with("{\"version\":1,"), "{text}");
    let back: CaptureStorageSidecar = serde_json::from_str(&text).unwrap();
    assert_eq!(back, sidecar);

    for (state, wire) in [
        (CaptureState::Ok, "ok"),
        (CaptureState::NearFull, "near_full"),
        (CaptureState::Full, "full"),
    ] {
        assert_eq!(serde_json::to_value(state).unwrap(), wire);
    }
}

#[test]
fn config_slice_conforms() {
    crate::schema::assert_slice_conforms::<CaptureStorageConfig>(CaptureStorageConfig::POINTER);
}

#[test]
fn config_keys_load_from_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    fs::write(
        &path,
        "storage:\n  capture_root: /data/cap\n  capture_floor_free_gib: 8\n  capture_evict_unuploaded: true\n",
    )
    .unwrap();
    let (cfg, err) = CaptureStorageConfig::load(&path);
    assert!(err.is_none(), "{err:?}");
    assert_eq!(cfg.capture_root, "/data/cap");
    assert_eq!(cfg.capture_budget_pct, 85);
    assert_eq!(cfg.capture_floor_free_gib, 8);
    assert!(cfg.capture_evict_unuploaded);
}
