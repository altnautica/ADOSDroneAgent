use std::time::Duration;

use ados_config::capture_storage::{CaptureState, CaptureUsage, Volume, GIB, SIDECAR_VERSION};

use super::*;

fn sidecar(free: u64) -> CaptureStorageSidecar {
    CaptureStorageSidecar {
        version: SIDECAR_VERSION,
        updated_ns: 1,
        capture_root: "/x".into(),
        volume: Some(Volume {
            mount: "/".into(),
            device: "nvme0n1p1".into(),
            fs: "ext4".into(),
            total_bytes: 1000 * GIB,
            free_bytes: free,
            rotational: false,
        }),
        capture: CaptureUsage {
            used_bytes: 0,
            budget_bytes: 850 * GIB,
            budget_pct: 85,
            floor_free_bytes: 5 * GIB,
            sessions: 0,
            uploaded: 0,
            evict_unuploaded: false,
            state: CaptureState::Ok,
        },
        write_probe: None,
    }
}

/// A runtime past the first-probe delay, with `root` and a sidecar of `free`.
fn ready(root: &Path, free: u64) -> CaptureRuntime {
    let mut rt = CaptureRuntime::new();
    rt.started = Instant::now()
        .checked_sub(Duration::from_secs(120))
        .expect("the host has been up two minutes");
    rt.root = Some(root.to_path_buf());
    rt.last = Some(sidecar(free));
    rt
}

async fn finish(rt: &mut CaptureRuntime, path: &Path) {
    while rt.probe_task.as_ref().is_some_and(|t| !t.is_finished()) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    rt.service_probe(path).await;
}

#[tokio::test]
async fn no_room_is_not_a_probe() {
    let tmp = tempfile::tempdir().unwrap();
    let side = tmp.path().join("capture-storage.json");
    let mut rt = ready(tmp.path(), 5 * GIB);
    rt.service_probe(&side).await;
    assert!(rt.probe_task.is_none());
    assert!(
        rt.last_probe.is_none(),
        "a skipped probe is retried next tick"
    );

    rt.last = Some(sidecar(800 * GIB));
    rt.service_probe(&side).await;
    assert!(rt.probe_task.is_some());
    assert!(rt.last_probe.is_some());
}

#[tokio::test]
async fn a_failed_probe_keeps_the_last_good_value() {
    let tmp = tempfile::tempdir().unwrap();
    let side = tmp.path().join("capture-storage.json");
    let mut rt = ready(tmp.path(), 800 * GIB);
    rt.service_probe(&side).await;
    finish(&mut rt, &side).await;
    let good = rt.probe.clone().expect("the probe measured the temp dir");
    assert_eq!(rt.last.as_ref().unwrap().write_probe, Some(good.clone()));

    // A root that is gone fails the next probe; the measurement stands.
    rt.root = Some(tmp.path().join("gone"));
    rt.last_probe = None;
    rt.service_probe(&side).await;
    finish(&mut rt, &side).await;
    assert_eq!(rt.probe, Some(good.clone()));
    assert_eq!(rt.last.as_ref().unwrap().write_probe, Some(good));
}

#[tokio::test]
async fn an_uncreatable_root_is_logged_once_and_not_probed() {
    let tmp = tempfile::tempdir().unwrap();
    let side = tmp.path().join("capture-storage.json");
    let mut rt = ready(tmp.path(), 800 * GIB);
    rt.note_root_error(tmp.path(), Some("denied"));
    assert_eq!(rt.root_error.as_deref(), Some("denied"));
    rt.service_probe(&side).await;
    assert!(rt.probe_task.is_none());
    rt.note_root_error(tmp.path(), None);
    assert!(rt.root_error.is_none());
}
