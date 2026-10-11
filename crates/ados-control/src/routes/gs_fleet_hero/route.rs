//! `POST /api/v1/ground-station/fleet/hero`: select the hero drone.

use super::*;

/// `POST /api/v1/ground-station/fleet/hero` — select the fleet's hero drone.
pub async fn post_fleet_hero(State(state): State<AppState>, body: Option<Json<Value>>) -> Response {
    if !is_ground_station(&state) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
        )
            .into_response();
    }

    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let Some(device_id) = body
        .get("device_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return detail(StatusCode::BAD_REQUEST, "device_id is required");
    };

    let Some(proxy) = state.aux_rpc_proxy.clone() else {
        return detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "relay-proxy not initialised on this node",
        );
    };

    let slots = registered_slots();

    // An unregistered target is refused BEFORE a single call goes out: an
    // operator typo must never demote the fleet to chase a drone that is not
    // there.
    let Some(plan) = plan_hero(&slots, device_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "detail": {"error": {"code": "E_UNKNOWN_DEVICE", "device_id": device_id}}
            })),
        )
            .into_response();
    };

    // The selection runs on its own task and this handler only awaits its
    // handle: a client that times out, or a connection that drops mid-request,
    // no longer aborts the calls in flight or skips recording and publishing
    // the hero.
    let caller = profile_caller(proxy, Arc::from(slots.as_slice()));
    let selected = select_hero(
        fleet_hero_state(),
        slots,
        plan,
        device_id.to_string(),
        caller,
        hero_sidecar(),
    )
    .await;
    let report = match selected {
        Ok(report) => report,
        Err(e) => {
            tracing::error!(error = %e, device_id = %device_id, "fleet_hero_selection_task_failed");
            return detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "hero selection task failed",
            );
        }
    };

    (
        outcome_status(&report),
        Json(outcome_body(device_id, &report)),
    )
        .into_response()
}

/// `200` when every registered drone confirmed; `502 Bad Gateway` when the
/// hero's OWN promotion failed, because the drone the operator picked is still
/// a thumbnail; `207 Multi-Status` when only demotions failed or have not
/// answered yet. Each carries the same per-slot body.
pub(super) fn outcome_status(report: &SelectionReport) -> StatusCode {
    if report.hero_failed() {
        StatusCode::BAD_GATEWAY
    } else if report.complete() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    }
}

/// The response body: the selected hero plus one row per registered slot, in
/// slot order. A `pending` row is a drone still being called when the report
/// was cut; it is neither a success nor a failure yet.
pub(super) fn outcome_body(hero: &str, report: &SelectionReport) -> Value {
    let resolved = report.resolved.iter().map(|o| {
        (
            o.slot,
            json!({
                "slot": o.slot,
                "device_id": o.device_id,
                "profile": o.profile.as_str(),
                "ok": o.ok,
                "pending": false,
                "error": o.error,
            }),
        )
    });
    let pending = report.pending.iter().map(|t| {
        (
            t.slot,
            json!({
                "slot": t.slot,
                "device_id": t.device_id,
                "profile": t.profile.as_str(),
                "ok": false,
                "pending": true,
                "error": Value::Null,
            }),
        )
    });
    let mut rows: Vec<(u8, Value)> = resolved.chain(pending).collect();
    rows.sort_by_key(|(slot, _)| *slot);
    json!({
        "hero": hero,
        "slots": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
    })
}
