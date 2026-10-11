use super::*;

fn state() -> (tempfile::TempDir, State) {
    let dir = tempfile::tempdir().unwrap();
    let state = State {
        #[cfg(target_os = "linux")]
        output: Arc::new(Mutex::new(crate::GpioOutput::new())),
        #[cfg(target_os = "linux")]
        beeps: Arc::default(),
        sidecar_path: Arc::new(dir.path().join("gpio-output.json")),
    };
    (dir, state)
}

#[tokio::test]
async fn bad_request_replies_with_a_stable_error() {
    let (_dir, state) = state();
    let v = dispatch(b"not json", &state).await;
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
}

#[tokio::test]
async fn unknown_op_replies_with_a_stable_error() {
    let (_dir, state) = state();
    let v = dispatch(br#"{"op":"frob"}"#, &state).await;
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().starts_with("E_UNKNOWN_OP"));
}

#[tokio::test]
async fn status_on_a_fresh_service_reports_no_lines() {
    // Safe-by-default: nothing has been driven, so status is an empty list.
    let (_dir, state) = state();
    let v = dispatch(br#"{"op":"status"}"#, &state).await;
    assert_eq!(v["ok"], true);
    assert!(v["lines"].as_array().unwrap().is_empty());
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn beep_without_a_gpio_subsystem_is_refused() {
    // No line can be driven, so the beep must not be acknowledged.
    let (_dir, state) = state();
    let v = dispatch(
        br#"{"op":"beep","pin":18,"on_ms":50,"off_ms":50,"cycles":2}"#,
        &state,
    )
    .await;
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"], "E_NO_GPIO");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn beep_on_a_missing_chip_reports_the_drive_failure() {
    // Chip 250 does not exist, so the first phase cannot be driven and the
    // caller hears about it instead of a false ok.
    let (_dir, state) = state();
    let v = dispatch(
        br#"{"op":"beep","chip":250,"pin":18,"on_ms":50,"off_ms":50,"cycles":2}"#,
        &state,
    )
    .await;
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().starts_with("E_DRIVE_FAILED"));
}
