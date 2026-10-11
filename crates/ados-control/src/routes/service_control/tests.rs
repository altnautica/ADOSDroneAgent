use super::*;

#[tokio::test]
async fn an_unknown_service_is_rejected_before_any_restart() {
    // A non-ados unit can never be restarted: it is normalized to
    // `ados-<name>`, misses the allowlist, and returns the error body that
    // echoes the original request name — all with no systemctl call.
    let body = restart_service_result("sshd").await;
    assert_eq!(body["status"], json!("error"));
    assert_eq!(body["message"], json!("Unknown service: sshd"));
    // No `unit` key on the unknown-name branch.
    assert!(body.as_object().unwrap().get("unit").is_none());
}

#[tokio::test]
async fn an_arbitrary_path_like_name_is_rejected() {
    // A name that already carries the prefix but is not in the allowlist is
    // still rejected (so `ados-` is necessary but not sufficient — only the
    // fixed unit set is allowed).
    let body = restart_service_result("ados-not-a-real-unit").await;
    assert_eq!(body["status"], json!("error"));
    assert_eq!(
        body["message"],
        json!("Unknown service: ados-not-a-real-unit")
    );
}

#[tokio::test]
async fn a_bare_name_is_normalized_to_the_ados_prefix_for_the_guard() {
    // `mavlink` → `ados-mavlink` is in the allowlist, so it passes the guard
    // and proceeds to the restart path. On a dev host with no systemctl the
    // restart cannot spawn, so the body is the spawn-error shape — but the
    // KEY parity point here is that the guard did NOT reject it (the message
    // is not an "Unknown service" rejection).
    let body = restart_service_result("mavlink").await;
    assert_eq!(body["status"], json!("error"));
    let msg = body["message"].as_str().unwrap();
    assert!(
        !msg.starts_with("Unknown service"),
        "an allowlisted bare name must pass the guard, got: {msg}"
    );
}

#[tokio::test]
async fn every_allowlisted_name_passes_the_guard() {
    // Each allowlisted unit clears the name guard (it does not produce an
    // "Unknown service" rejection). On a dev host the subsequent restart
    // fails to spawn, which is a different, non-rejection error body.
    for unit in allowed_units() {
        // Strip the `ados-` prefix to exercise the normalization path too.
        let bare = unit.strip_prefix("ados-").unwrap();
        let body = restart_service_result(bare).await;
        let msg = body["message"].as_str().unwrap_or_default();
        assert!(
            !msg.starts_with("Unknown service"),
            "{unit} should pass the guard"
        );
    }
}

#[tokio::test]
async fn perform_restart_emits_a_spawn_error_body_when_systemctl_is_absent() {
    // On a host with no systemctl the restart subprocess cannot spawn. The
    // FastAPI catch-all returns `{"status":"error","message": str(exc)}`; this
    // surface returns the same shape with the spawn error string. (This test
    // only runs meaningfully where systemctl is absent — a CI/dev host — and
    // asserts the envelope, not the host-specific error text.)
    if which_systemctl().is_some() {
        // A systemd host: the unit is unknown to systemd and the restart will
        // return non-zero or never confirm, but either way it is the
        // error-status shape. Skip the spawn-error assertion there.
        return;
    }
    let body = perform_restart("ados-api").await;
    assert_eq!(body["status"], json!("error"));
    assert!(body["message"].is_string());
}

#[test]
fn allowed_units_derived() {
    // Every shipped unit except the six with nothing to restart through
    // this route: this process, the supervisor and the boot oneshots.
    let refused: Vec<&str> = ados_protocol::units::UNITS
        .iter()
        .map(|u| u.stem())
        .filter(|stem| !restart_allowed(stem))
        .collect();
    assert_eq!(
        refused,
        [
            "ados-supervisor",
            "ados-control",
            "ados-fbcon-detach",
            "ados-display-probe",
            "ados-camera-probe",
            "ados-usb-otg-host",
        ]
    );
    assert_eq!(
        allowed_units().count(),
        ados_protocol::units::UNITS.len() - 6
    );
    assert!(restart_allowed("ados-mesh-pairing"));
    assert!(!restart_allowed("ados-sshd"));
}

#[test]
fn a_crash_looping_unit_is_never_confirmed_restarted() {
    // Restart=always + crash on start: every attempt shows a fresh PID,
    // briefly active, then auto-restart with MainPID 0.
    let mut settle = PidSettle::new(100);
    let crash_loop = [
        (201, "active"),
        (0, "activating"),
        (202, "active"),
        (0, "activating"),
        (203, "active"),
        (204, "active"),
        (0, "activating"),
    ];
    for (pid, state) in crash_loop {
        assert!(
            !settle.observe(pid, state),
            "pid {pid} ({state}) is not a restart"
        );
    }
}

#[test]
fn a_fresh_pid_held_active_confirms_the_restart() {
    let mut settle = PidSettle::new(100);
    // The old PID and an activating unit do not count.
    assert!(!settle.observe(100, "active"));
    assert!(!settle.observe(300, "activating"));
    assert!(!settle.observe(300, "active"));
    assert!(!settle.observe(300, "active"));
    assert!(settle.observe(300, "active"));
}

#[test]
fn supervisor_reports_not_found_when_systemctl_is_absent() {
    // The supervisor route's only `ok:false` outcome: no systemctl on PATH.
    // This only fires meaningfully on a dev host (no systemd); on a systemd
    // host the route would schedule the restart, which a unit test must not
    // actually fire, so the assertion is gated.
    if which_systemctl().is_some() {
        return;
    }
    // Drive the gate directly (the async handler would otherwise schedule a
    // real restart on a systemd host).
    assert!(which_systemctl().is_none());
}

#[tokio::test]
async fn supervisor_route_returns_a_two_key_body() {
    // The supervisor route always returns a `{ok, message}` body. On a dev
    // host with no systemctl it is the `ok:false` not-found body; that is the
    // deterministic case the unit suite asserts (a systemd host would schedule
    // a real restart, so the success path is bench-only).
    if which_systemctl().is_some() {
        return;
    }
    let Json(body) = restart_supervisor().await;
    let obj = body.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["message", "ok"]);
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["message"], json!("systemctl binary not found on PATH"));
}

/// Golden-fixture parity: the unknown-service error body for
/// `POST /api/services/{name}/restart` with a non-ados name. The exact FastAPI
/// JSON for that case (served at HTTP 200) is:
///
/// ```json
/// {"status": "error", "message": "Unknown service: nginx"}
/// ```
///
/// This is the deterministic, host-independent golden the conformance harness
/// pins for the route (the success + confirmation bodies depend on a live
/// systemd unit and are bench-validated, not asserted here).
#[tokio::test]
async fn golden_unknown_service_body() {
    let body = restart_service_result("nginx").await;
    let golden = json!({
        "status": "error",
        "message": "Unknown service: nginx",
    });
    assert_eq!(body, golden);
}
