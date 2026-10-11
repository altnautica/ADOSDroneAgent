use super::*;
use std::fs;

// --- unit_from_cgroup ---------------------------------------------------

#[test]
fn unit_from_cgroup_extracts_the_ados_unit() {
    assert_eq!(
        unit_from_cgroup("0::/system.slice/ados.slice/ados-video.service").as_deref(),
        Some("ados-video.service")
    );
    assert_eq!(
        unit_from_cgroup("0::/system.slice/ados-api.service").as_deref(),
        Some("ados-api.service")
    );
    assert_eq!(
        unit_from_cgroup("0::/system.slice/ados.slice/ados-wfb-rx.service\n").as_deref(),
        Some("ados-wfb-rx.service")
    );
    // A v1-style multi-line cgroup file still matches the unit token.
    assert_eq!(
        unit_from_cgroup(
            "12:pids:/system.slice/ados-cloud.service\n0::/system.slice/ados-cloud.service"
        )
        .as_deref(),
        Some("ados-cloud.service")
    );
}

#[test]
fn unit_from_cgroup_none_for_non_ados_or_empty() {
    assert_eq!(unit_from_cgroup("0::/system.slice/sshd.service"), None);
    assert_eq!(unit_from_cgroup("0::/user.slice/user-1000.slice"), None);
    assert_eq!(unit_from_cgroup(""), None);
    // A bare `ados-` with no `.service` suffix is not a unit.
    assert_eq!(unit_from_cgroup("0::/system.slice/ados.slice"), None);
}

// --- pss_kib_from_rollup ------------------------------------------------

#[test]
fn pss_kib_from_rollup_parses_the_pss_line() {
    let rollup = "55a0..-55a1.. ---p 00000000 00:00 0 [rollup]\n\
         Rss:              180000 kB\n\
         Pss:              165000 kB\n\
         Shared_Clean:      12000 kB\n";
    assert_eq!(pss_kib_from_rollup(rollup), 165000);
}

#[test]
fn pss_kib_from_rollup_zero_when_absent_or_malformed() {
    assert_eq!(pss_kib_from_rollup(""), 0);
    assert_eq!(pss_kib_from_rollup("Rss: 1000 kB\n"), 0);
    assert_eq!(pss_kib_from_rollup("Pss: not-a-number kB\n"), 0);
    assert_eq!(pss_kib_from_rollup("Pss:\n"), 0);
}

// --- scan_pss_by_unit_in groups + sums by unit (real /proc-shaped fakes) -

/// Lay down a `/proc/<pid>/{cgroup,smaps_rollup}` pair under `root`.
fn write_pid(root: &std::path::Path, pid: &str, cgroup: &str, rollup: &str) {
    let dir = root.join(pid);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("cgroup"), cgroup).unwrap();
    fs::write(dir.join("smaps_rollup"), rollup).unwrap();
}

#[test]
fn scan_groups_and_sums_by_unit_in_bytes() {
    // A unit with two PIDs (orchestrator + ffmpeg child) sums; a non-ados
    // unit and a non-numeric directory are skipped.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_pid(
        root,
        "100",
        "0::/system.slice/ados.slice/ados-video.service",
        "Pss: 10240 kB\n",
    );
    write_pid(
        root,
        "101",
        "0::/system.slice/ados.slice/ados-video.service",
        "Pss: 153600 kB\n",
    );
    write_pid(
        root,
        "200",
        "0::/system.slice/ados-api.service",
        "Pss: 81100 kB\n",
    );
    write_pid(
        root,
        "300",
        "0::/system.slice/sshd.service",
        "Pss: 9000 kB\n",
    );
    // A non-numeric entry (e.g. /proc/self) must be ignored, not parsed.
    fs::create_dir_all(root.join("self")).unwrap();

    let out = scan_pss_by_unit_in(root);
    // (10240 + 153600) KiB * 1024 = 167772160 bytes.
    assert_eq!(out.get("ados-video.service").copied(), Some(167_772_160));
    // 81100 KiB * 1024.
    assert_eq!(out.get("ados-api.service").copied(), Some(81_100 * 1024));
    assert!(!out.contains_key("sshd.service"));
}

#[test]
fn scan_skips_pids_with_no_rollup_or_zero_pss() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Zero PSS contributes nothing.
    write_pid(
        root,
        "10",
        "0::/system.slice/ados-cloud.service",
        "Pss: 0 kB\n",
    );
    // A cgroup with no smaps_rollup file at all is skipped.
    let dir = root.join("11");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("cgroup"), "0::/system.slice/ados-mavlink.service").unwrap();

    let out = scan_pss_by_unit_in(root);
    assert!(out.is_empty(), "no unit should appear: {out:?}");
}

#[test]
fn scan_missing_proc_root_is_empty_not_error() {
    let out = scan_pss_by_unit_in(std::path::Path::new("/no/such/proc/root"));
    assert!(out.is_empty());
}

// --- emit_samples ships one metric per unit -----------------------------

#[tokio::test]
async fn emit_samples_ships_one_tagged_metric_per_unit() {
    use ados_protocol::frame::{decode_len, HEADER_SIZE};
    use ados_protocol::logd::{IngestFrame, LOGD_MAX_FRAME};
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    let mut sock = std::env::temp_dir();
    sock.push(format!(
        "ados-svcmem-emit-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();

    // Accept and read the two framed metrics the emitter ships.
    let accept = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut got: Vec<(String, f64, String)> = Vec::new();
        for _ in 0..2 {
            let mut header = [0u8; HEADER_SIZE];
            stream.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, LOGD_MAX_FRAME, true).unwrap();
            let mut body = vec![0u8; len];
            stream.read_exact(&mut body).await.unwrap();
            if let IngestFrame::Telemetry(t) = IngestFrame::decode(&body).unwrap() {
                let unit = t
                    .tags
                    .get(TAG_UNIT)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                got.push((t.metric, t.value, unit));
            }
        }
        got
    });

    let emitter = IngestEmitter::with_socket("ados-supervisor", &sock);
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    totals.insert("ados-api.service".to_string(), 83_046_400);
    totals.insert("ados-video.service".to_string(), 167_772_160);
    emit_samples(&emitter, &totals);

    let mut got = tokio::time::timeout(Duration::from_secs(5), accept)
        .await
        .expect("metrics delivered")
        .expect("accept task ok");
    got.sort_by(|a, b| a.2.cmp(&b.2));
    assert_eq!(
        got,
        vec![
            (
                METRIC_MEMORY_PSS_BYTES.to_string(),
                83_046_400.0,
                "ados-api.service".to_string()
            ),
            (
                METRIC_MEMORY_PSS_BYTES.to_string(),
                167_772_160.0,
                "ados-video.service".to_string()
            ),
        ]
    );
    let _ = std::fs::remove_file(&sock);
}
