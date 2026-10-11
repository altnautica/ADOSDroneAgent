use super::*;
use tokio::time::Instant;

#[tokio::test]
async fn a_command_that_never_returns_is_bounded_and_reports_failure() {
    // The SUP-WATCHDOG-DECOUPLED shape: a network-repair call that hangs
    // forever. It must come back inside the ceiling as a plain failure so
    // the monitor pass advances to the next backend.
    let started = Instant::now();
    let ok = run_status_within("sh", &["-c", "sleep 600"], Duration::from_millis(300)).await;
    let waited = started.elapsed();
    assert!(!ok, "a timed-out repair must report failure, not success");
    assert!(
        waited < Duration::from_secs(5),
        "the call must be bounded by the ceiling, waited {waited:?}"
    );
}

#[tokio::test]
async fn a_capturing_command_that_never_returns_is_bounded() {
    let started = Instant::now();
    let out = run_output_within("sh", &["-c", "sleep 600"], Duration::from_millis(300)).await;
    assert!(out.is_none(), "a timed-out probe must report no output");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_prompt_command_still_returns_its_status_and_output() {
    assert!(run_status("true", &[]).await);
    assert!(!run_status("false", &[]).await);
    let out = run_output("sh", &["-c", "printf ados"]).await;
    assert_eq!(out.as_deref(), Some("ados"));
}

#[tokio::test]
async fn a_missing_binary_is_a_failure_not_a_hang() {
    assert!(!run_status("ados-no-such-binary-in-this-tree", &[]).await);
    assert!(run_output("ados-no-such-binary-in-this-tree", &[])
        .await
        .is_none());
}

#[tokio::test]
async fn binary_available_answers_for_a_shell_builtin_path() {
    assert!(binary_available("sh").await);
    assert!(!binary_available("ados-no-such-binary-in-this-tree").await);
}
