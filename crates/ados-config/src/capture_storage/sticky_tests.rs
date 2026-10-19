use std::fs;
use std::time::{Duration, SystemTime};

use super::tests::{usage, volume, Fake};
use super::*;

fn auto() -> CaptureStorageConfig {
    CaptureStorageConfig::default()
}

#[test]
fn an_nvme_choice_is_persisted_and_kept_while_its_mount_is_present() {
    let mut f = Fake::new();
    f.disk("mmcblk0", "0", &["mmcblk0p2"]);
    f.disk("nvme0n1", "0", &["nvme0n1p1"]);
    f.mount("/dev/mmcblk0p2", "root", "ext4", 64 * GIB, 40 * GIB);
    let nvme = f.mount("/dev/nvme0n1p1", "mnt/nvme", "ext4", 1000 * GIB, 800 * GIB);
    let chosen = nvme.join("ados/capture");

    assert_eq!(resolve_capture_root(&auto(), &f.roots), chosen);
    let sticky = read_sticky(&f.roots.sticky_file).expect("the choice is persisted");
    assert_eq!(sticky.mount, nvme.display().to_string());
    assert_eq!(sticky.root, chosen.display().to_string());

    // Under the 64 GiB selection threshold the volume still holds captures:
    // full is reported against it, never a silent move to the SD card.
    f.space("mnt/nvme", 1000 * GIB, GIB);
    assert_eq!(resolve_capture_root(&auto(), &f.roots), chosen);
}

#[test]
fn a_roomier_nvme_does_not_steal_a_present_sticky_root() {
    let mut f = Fake::new();
    f.disk("nvme0n1", "0", &["nvme0n1p1"]);
    f.disk("nvme1n1", "0", &["nvme1n1p1"]);
    let first = f.mount("/dev/nvme1n1p1", "mnt/b", "ext4", 1000 * GIB, 800 * GIB);
    assert_eq!(
        resolve_capture_root(&auto(), &f.roots),
        first.join("ados/capture")
    );
    // nvme0n1 sorts first and has room, but the persisted choice stands.
    f.mount("/dev/nvme0n1p1", "mnt/a", "ext4", 2000 * GIB, 1900 * GIB);
    f.space("mnt/b", 1000 * GIB, 2 * GIB);
    assert_eq!(
        resolve_capture_root(&auto(), &f.roots),
        first.join("ados/capture")
    );
}

#[test]
fn a_missing_sticky_mount_lets_auto_choose_again() {
    let mut f = Fake::new();
    f.disk("nvme0n1", "0", &["nvme0n1p1"]);
    f.disk("nvme1n1", "0", &["nvme1n1p1"]);
    let a = f.mount("/dev/nvme0n1p1", "mnt/a", "ext4", 1000 * GIB, 800 * GIB);
    assert_eq!(
        resolve_capture_root(&auto(), &f.roots),
        a.join("ados/capture")
    );

    // The volume is gone and nothing else qualifies: the fallback, and the
    // persisted choice is left for the volume's return.
    f.unmount("mnt/a");
    assert_eq!(
        resolve_capture_root(&auto(), &f.roots),
        f.roots.fallback_root
    );
    let kept = read_sticky(&f.roots.sticky_file).unwrap();
    assert_eq!(kept.mount, a.display().to_string());

    // A different NVMe volume qualifies: it is chosen and persisted.
    let b = f.mount("/dev/nvme1n1p1", "mnt/b", "ext4", 1000 * GIB, 800 * GIB);
    assert_eq!(
        resolve_capture_root(&auto(), &f.roots),
        b.join("ados/capture")
    );
    assert_eq!(
        read_sticky(&f.roots.sticky_file).unwrap().mount,
        b.display().to_string()
    );
}

#[test]
fn an_explicit_root_ignores_the_sticky_choice() {
    let mut f = Fake::new();
    f.disk("nvme0n1", "0", &["nvme0n1p1"]);
    f.mount("/dev/nvme0n1p1", "mnt/a", "ext4", 1000 * GIB, 800 * GIB);
    resolve_capture_root(&auto(), &f.roots);
    let cfg = CaptureStorageConfig {
        capture_root: "/data/capture".into(),
        ..auto()
    };
    assert_eq!(
        resolve_capture_root(&cfg, &f.roots),
        std::path::PathBuf::from("/data/capture")
    );
}

fn sidecar(state: CaptureState) -> CaptureStorageSidecar {
    let mut capture = usage(1, 850 * GIB, 1, 0, false);
    capture.state = state;
    CaptureStorageSidecar {
        version: SIDECAR_VERSION,
        updated_ns: 1,
        capture_root: "/mnt/nvme/ados/capture".into(),
        volume: Some(volume(800 * GIB)),
        capture,
        write_probe: None,
    }
}

#[test]
fn admission_from_the_sidecar_treats_unknown_as_full() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture-storage.json");
    let now = SystemTime::now();

    let missing = admit_from_sidecar(&path, now).unwrap_err();
    assert!(matches!(missing, Refused::Unknown(_)));
    assert_eq!(missing.code(), E_STORAGE_FULL);

    fs::write(&path, b"{not json").unwrap();
    assert!(matches!(
        admit_from_sidecar(&path, now),
        Err(Refused::Unknown(_))
    ));

    fs::write(
        &path,
        serde_json::to_vec(&sidecar(CaptureState::NearFull)).unwrap(),
    )
    .unwrap();
    let ok = admit_from_sidecar(&path, SystemTime::now()).unwrap();
    assert_eq!(ok.capture_root, "/mnt/nvme/ados/capture");

    let later = SystemTime::now() + SIDECAR_MAX_AGE + Duration::from_secs(1);
    let stale = admit_from_sidecar(&path, later).unwrap_err();
    assert!(matches!(stale, Refused::Unknown(_)));
    assert_eq!(stale.code(), E_STORAGE_FULL);

    fs::write(
        &path,
        serde_json::to_vec(&sidecar(CaptureState::Full)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        admit_from_sidecar(&path, SystemTime::now()),
        Err(Refused::Full)
    );
}

#[test]
fn the_freshness_window_outlasts_the_refresh_and_matches_the_registry() {
    assert!(SIDECAR_MAX_AGE >= SIDECAR_REFRESH * 3);
    // Registered with a window once codegen carries it; never a different one.
    if let Some(registered) = ados_protocol::paths::max_age(SIDECAR_ID) {
        assert_eq!(registered, SIDECAR_MAX_AGE);
    }
}
