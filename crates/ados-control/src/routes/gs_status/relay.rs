//! The `mesh` sub-block + the relay/receiver routes (store-first, sidecar-fallback).

use super::*;

/// The `/status` `mesh` sub-block from the store's `mesh.state` event, projecting
/// the five fields the live route reads off `mesh-state.json` (`up`, `peer_count`
/// = neighbor count, `selected_gateway`, `partition`, `mesh_id`). `None` when the
/// store is unreachable, holds no such event, or holds only a stale one, so the
/// caller falls back to the sidecar.
pub(super) async fn latest_status_mesh_block(state: &AppState) -> Option<Value> {
    let detail = latest_event_detail(state, "mesh.state").await?;
    Some(mesh_block_from_snapshot(&detail))
}

/// Project the `/status` `mesh` block from a FRESH mesh snapshot body (the stored
/// event detail or the sidecar). Flags are truthiness-coerced and lists reduced to
/// their length, and the block carries `stale: false` so a client always reads the
/// verdict off the same key instead of inferring it from a key's absence.
pub(super) fn mesh_block_from_snapshot(snap: &Map<String, Value>) -> Value {
    let peer_count = snap
        .get("neighbors")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    json!({
        "up": snap.get("up").map(json_truthy).unwrap_or(false),
        "peer_count": peer_count,
        "selected_gateway": snap.get("selected_gateway").cloned().unwrap_or(Value::Null),
        "partition": snap.get("partition").map(json_truthy).unwrap_or(false),
        "mesh_id": snap.get("mesh_id").cloned().unwrap_or(Value::Null),
        "stale": false,
    })
}

/// The `mesh` block when no current snapshot exists: the same five keys, every
/// value `null`, `stale: true`.
///
/// The keys stay so a consumer reading `up` gets `null` (unknown) rather than
/// finding the key gone and coercing it — and `up: false` is NOT used, because
/// "the mesh is down" is a measurement this node cannot make when nothing is
/// reporting.
pub(super) fn mesh_block_stale() -> Value {
    json!({
        "up": Value::Null,
        "peer_count": Value::Null,
        "selected_gateway": Value::Null,
        "partition": Value::Null,
        "mesh_id": Value::Null,
        "stale": true,
    })
}

/// The `/status` `mesh` block from the `mesh-state.json` sidecar, projecting the
/// same five fields. An absent / unparseable / non-object / **stale** file yields
/// the `stale: true` block: the mesh poll loop rewrites this file each tick, so a
/// file older than the freshness window is the last thing a dead loop wrote, and
/// it used to be served verbatim as the current mesh state.
pub(super) fn mesh_block_from_sidecar(path: &Path, now: SystemTime) -> Value {
    let Some(snap) = read_fresh_json(path, now) else {
        return mesh_block_stale();
    };
    // Best-effort schema-drift signal (never reject): warn when the mesh-state
    // sidecar was written by an agent with a different schema version, then read
    // anyway. The writer const lives in the groundlink crate, so compare against
    // the shared registry.
    let got = snap.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("mesh-state") {
        ados_protocol::sidecar::check_sidecar_version("mesh-state", got, ours);
    }
    mesh_block_from_snapshot(&snap)
}

/// A snapshot body served as live, stamped `stale: false`.
pub(super) fn fresh_snapshot_body(mut detail: Map<String, Value>) -> Value {
    detail.insert("stale".to_string(), Value::Bool(false));
    Value::Object(detail)
}

/// The reply for a relay/receiver counter route with no current snapshot: every
/// key the last known snapshot carried, nulled, plus `stale: true`.
///
/// Nulling the stale snapshot's own keys rather than emitting a bare
/// `{"stale": true}` keeps the key set a client already reads intact — a key that
/// vanishes reads as "not reported" and gets coerced to 0 or false downstream,
/// which is the same failure in a different costume. With no snapshot at all the
/// key set is genuinely unknown, so the body is just the marker.
pub(super) fn stale_snapshot_body(last_known: Map<String, Value>) -> Value {
    let mut out: Map<String, Value> = last_known
        .into_iter()
        .map(|(k, _v)| (k, Value::Null))
        .collect();
    out.insert("stale".to_string(), Value::Bool(true));
    Value::Object(out)
}

/// `GET /api/v1/ground-station/wfb/relay/status` → relay-side fragment counters.
///
/// `404` `E_WRONG_ROLE` off a relay node. On a relay, reads the store's
/// most-recent `gs.relay_state` event (the relay loop ships the same body it
/// writes to the sidecar), falling back to the `/run/ados/wfb-relay.json`
/// sidecar. Both reads are age-gated to `SNAPSHOT_FRESH_S`; with neither
/// current the body is the nulled `stale: true` shape, never the counters a dead
/// relay loop last wrote.
pub async fn get_wfb_relay_status(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };
    if role != "relay" {
        return wrong_role("relay");
    }
    if let Some(detail) = latest_event_detail(&state, "gs.relay_state").await {
        return Json(fresh_snapshot_body(detail)).into_response();
    }
    let path = wfb_relay_path(&state.run_dir);
    match read_fresh_json(&path, SystemTime::now()) {
        Some(snap) => Json(fresh_snapshot_body(snap)).into_response(),
        None => Json(stale_snapshot_body(
            crate::sidecar::read_object(&path).unwrap_or_default(),
        ))
        .into_response(),
    }
}

/// `GET /api/v1/ground-station/wfb/receiver/relays` → per-relay fragment counters.
///
/// `404` `E_WRONG_ROLE` off a receiver node. On a receiver, reads the store's
/// most-recent `gs.receiver_state` event projected to `{relays}`, falling back to
/// the `/run/ados/wfb-receiver.json` sidecar. Both age-gated; with neither
/// current, `relays` is `null` under `stale: true` — NOT the empty list, which
/// reads as "looked, found no relays".
pub async fn get_wfb_receiver_relays(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };
    if role != "receiver" {
        return wrong_role("receiver");
    }
    if let Some(detail) = latest_event_detail(&state, "gs.receiver_state").await {
        return Json(slice_receiver_relays(&detail)).into_response();
    }
    match read_fresh_json(&wfb_receiver_path(&state.run_dir), SystemTime::now()) {
        Some(snap) => Json(slice_receiver_relays(&snap)).into_response(),
        None => Json(json!({"relays": Value::Null, "stale": true})).into_response(),
    }
}

/// `GET /api/v1/ground-station/wfb/receiver/combined` → combined FEC output stats.
///
/// `404` `E_WRONG_ROLE` off a receiver node. On a receiver, reads the store's
/// `gs.receiver_state` event projected to `{fragments_after_dedup, fec_repaired,
/// output_kbps, up}`, falling back to the `/run/ados/wfb-receiver.json` sidecar
/// (same projection + per-key defaults). Both age-gated; with neither current
/// every counter is `null` under `stale: true`, because a zeroed counter and a
/// `up: false` are readings this node cannot make when nothing is reporting.
pub async fn get_wfb_receiver_combined(State(state): State<AppState>) -> Response {
    let role = match ground_station_role(&state) {
        Some(r) => r,
        None => return profile_mismatch(),
    };
    if role != "receiver" {
        return wrong_role("receiver");
    }
    if let Some(detail) = latest_event_detail(&state, "gs.receiver_state").await {
        return Json(slice_receiver_combined(&detail)).into_response();
    }
    match read_fresh_json(&wfb_receiver_path(&state.run_dir), SystemTime::now()) {
        Some(snap) => Json(slice_receiver_combined(&snap)).into_response(),
        None => Json(receiver_combined_stale()).into_response(),
    }
}

/// Project the `/wfb/receiver/relays` shape from a FRESH receiver-state body:
/// `{relays}` (the key defaulting to the empty list when the snapshot itself
/// omits it — that IS a current reading of "no relays") plus `stale: false`.
pub(super) fn slice_receiver_relays(detail: &Map<String, Value>) -> Value {
    json!({
        "relays": detail.get("relays").cloned().unwrap_or_else(|| json!([])),
        "stale": false,
    })
}

/// Project the `/wfb/receiver/combined` shape from a FRESH receiver-state body,
/// applying the same per-key defaults the live route applies so an omitted key
/// coalesces identically whether it is absent from the stored detail or the
/// sidecar.
pub(super) fn slice_receiver_combined(detail: &Map<String, Value>) -> Value {
    json!({
        "fragments_after_dedup": detail.get("fragments_after_dedup").cloned().unwrap_or_else(|| json!(0)),
        "fec_repaired": detail.get("fec_repaired").cloned().unwrap_or_else(|| json!(0)),
        "output_kbps": detail.get("output_kbps").cloned().unwrap_or_else(|| json!(0)),
        "up": detail.get("up").cloned().unwrap_or(Value::Bool(false)),
        "stale": false,
    })
}

/// The `/wfb/receiver/combined` shape with nothing current: the same four keys,
/// nulled, under `stale: true`.
pub(super) fn receiver_combined_stale() -> Value {
    json!({
        "fragments_after_dedup": Value::Null,
        "fec_repaired": Value::Null,
        "output_kbps": Value::Null,
        "up": Value::Null,
        "stale": true,
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/wfb — the live radio channel and TX power.
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/wfb` → `{channel, tx_power_dbm}` read from the
/// live radio sidecar (`wfb-stats.json`): the channel the radio is actually
/// tuned to and its effective TX power. Each is `null` when the sidecar is
/// absent, older than `SNAPSHOT_FRESH_S`, or does not carry it — never a
/// configured default dressed up as a reading. `404` `E_PROFILE_MISMATCH` off a
/// ground-station node.
pub async fn get_wfb(State(state): State<AppState>) -> Response {
    if ground_station_role(&state).is_none() {
        return profile_mismatch();
    }
    Json(wfb_view_from(
        &wfb_stats_path(&state.run_dir),
        SystemTime::now(),
    ))
    .into_response()
}

/// The path-injectable core of [`get_wfb`].
pub(super) fn wfb_view_from(path: &Path, now: SystemTime) -> Value {
    let live = read_fresh_json(path, now).unwrap_or_default();
    let channel = live
        .get("actual_channel")
        .and_then(json_to_i64)
        .filter(|&c| c > 0);
    let tx_power_dbm = live.get("tx_power_dbm").filter(|v| v.is_number()).cloned();
    json!({
        "channel": channel,
        "tx_power_dbm": tx_power_dbm,
    })
}
