use super::*;
use crate::dispatch::CommandStatus;

#[test]
fn send_command_forwards_the_full_cmd_args_body() {
    // The relay enqueues send_command as { cmd, args } — the exact CommandRequest
    // body. It must reach /api/command verbatim; forwarding only the inner args
    // array would POST a bare list and fail deserialization.
    let r = route_for(
        "send_command",
        &serde_json::json!({"cmd": "arm", "args": []}),
    )
    .unwrap();
    assert_eq!(r.method, Method::Post);
    assert_eq!(r.path, "/api/command");
    assert_eq!(r.body, Some(serde_json::json!({"cmd": "arm", "args": []})));

    let t = route_for(
        "send_command",
        &serde_json::json!({"cmd": "takeoff", "args": [25]}),
    )
    .unwrap();
    assert_eq!(
        t.body,
        Some(serde_json::json!({"cmd": "takeoff", "args": [25]}))
    );
}

#[test]
fn restart_service_maps_to_named_route() {
    let r = route_for(
        "restart_service",
        &serde_json::json!({"name": "ados-video"}),
    )
    .unwrap();
    assert_eq!(r.method, Method::Post);
    assert_eq!(r.path, "/api/services/ados-video/restart");
}

#[test]
fn restart_service_sanitizes_the_unit_segment() {
    // A path-traversal attempt is stripped to safe characters; the local
    // route's own allowlist is the real gate, this is defense-in-depth.
    let r = route_for(
        "restart_service",
        &serde_json::json!({"name": "../../etc/shadow"}),
    )
    .unwrap();
    assert_eq!(r.path, "/api/services/....etcshadow/restart");
}

#[test]
fn restart_service_without_name_has_no_route() {
    assert!(route_for("restart_service", &serde_json::json!({})).is_none());
    assert!(route_for("restart_service", &serde_json::json!({"name": ""})).is_none());
}

#[test]
fn scan_and_get_peripherals_map() {
    let scan = route_for("scan_peripherals", &serde_json::Value::Null).unwrap();
    assert_eq!(scan.method, Method::Post);
    assert_eq!(scan.path, "/api/peripherals/scan");
    let get = route_for("get_peripherals", &serde_json::Value::Null).unwrap();
    assert_eq!(get.method, Method::Get);
    assert_eq!(get.path, "/api/peripherals");
}

#[test]
fn get_logs_carries_level_and_limit_query() {
    let r = route_for(
        "get_logs",
        &serde_json::json!({"level": "WARN", "limit": 200}),
    )
    .unwrap();
    assert_eq!(r.path, "/api/logs");
    assert!(r.query.contains(&("level".to_string(), "WARN".to_string())));
    assert!(r.query.contains(&("limit".to_string(), "200".to_string())));
}

#[test]
fn fleet_reads_map() {
    assert_eq!(
        route_for("get_peers", &serde_json::Value::Null)
            .unwrap()
            .path,
        "/api/fleet/peers"
    );
    assert_eq!(
        route_for("get_enrollment", &serde_json::Value::Null)
            .unwrap()
            .path,
        "/api/fleet/enrollment"
    );
}

#[test]
fn unmapped_command_returns_none() {
    assert!(route_for("totally_unknown", &serde_json::Value::Null).is_none());
    // Plugin commands are handled in-process, not via loopback.
    assert!(route_for("plugin.enable", &serde_json::json!({"pluginId": "p"})).is_none());
}

#[test]
fn interpret_ok_body_is_completed() {
    let r = interpret(
        "restart_service",
        200,
        serde_json::json!({"status": "ok", "message": "Restarted ados-video"}),
    );
    assert_eq!(r.status, CommandStatus::Completed);
    assert_eq!(r.result["message"], "Restarted ados-video");
    assert_eq!(r.data.unwrap()["status"], "ok");
}

#[test]
fn interpret_error_body_on_200_is_failed() {
    // The service-restart route returns `{"status":"error"}` with HTTP 200
    // for an unknown / rejected unit. That must ack failed, never completed.
    let r = interpret(
        "restart_service",
        200,
        serde_json::json!({"status": "error", "message": "Unknown service: bogus"}),
    );
    assert_eq!(r.status, CommandStatus::Failed);
    assert_eq!(r.result["message"], "Unknown service: bogus");
}

#[test]
fn interpret_http_error_is_failed() {
    let r = interpret("get_logs", 503, serde_json::json!({"detail": "store down"}));
    assert_eq!(r.status, CommandStatus::Failed);
}

#[test]
fn interpret_array_body_is_completed() {
    // `/api/fleet/peers` returns a bare array; no `status` field, HTTP 200.
    let r = interpret("get_peers", 200, serde_json::json!([]));
    assert_eq!(r.status, CommandStatus::Completed);
    assert!(r.data.unwrap().is_array());
}
