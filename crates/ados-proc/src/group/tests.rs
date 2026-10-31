use super::*;

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn spawns_and_kills_a_process_group() {
    let a = args(&["30"]);
    let mut p = GroupChild::spawn(
        "t",
        Spec {
            program: "sleep",
            args: &a,
            stdout: StdoutMode::Null,
            stderr: StderrMode::Null,
        },
    )
    .expect("spawn sleep");
    assert!(p.pid().is_some());
    assert!(p.is_running());
    p.kill().await;
    assert!(!p.is_running());
}

#[tokio::test]
async fn terminate_reaps_a_sleeper_inside_the_grace_window() {
    let a = args(&["60"]);
    let mut p = GroupChild::spawn(
        "t",
        Spec {
            program: "sleep",
            args: &a,
            stdout: StdoutMode::Null,
            stderr: StderrMode::Null,
        },
    )
    .expect("spawn sleep");
    let started = std::time::Instant::now();
    p.terminate(Duration::from_secs(3)).await;
    assert!(!p.is_running());
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn append_log_is_emptied_at_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("child.log");
    std::fs::write(&log, b"stale contents").unwrap();
    let a = args(&["-c", "printf abc >&2"]);
    let mut p = GroupChild::spawn(
        "t",
        Spec {
            program: "sh",
            args: &a,
            stdout: StdoutMode::Null,
            stderr: StderrMode::AppendLog(log.clone()),
        },
    )
    .unwrap();
    assert!(p.wait().await.unwrap().success());
    assert_eq!(std::fs::read(&log).unwrap(), b"abc");
}

/// Terminating the leader kills a member it forked, not just the leader.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn terminate_kills_a_grandchild_in_the_group() {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let a = args(&["-c", "sleep 60 & echo $!; wait"]);
    let mut p = GroupChild::spawn(
        "t",
        Spec {
            program: "sh",
            args: &a,
            stdout: StdoutMode::Piped,
            stderr: StderrMode::Null,
        },
    )
    .unwrap();
    let mut lines = crate::lines::BoundedLines::new(p.take_stdout().unwrap());
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("the shell reports its background pid")
        .expect("a pid line");
    let grandchild = Pid::from_raw(line.trim().parse().expect("numeric pid"));
    assert!(kill(grandchild, None).is_ok(), "the grandchild is alive");

    p.terminate(Duration::from_secs(2)).await;
    assert!(!p.is_running());

    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        if kill(grandchild, None) == Err(Errno::ESRCH) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the grandchild outlived the group terminate"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(p);
}
