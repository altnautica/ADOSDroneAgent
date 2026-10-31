//! WFB-ng link-status + pair read routes.
//!
//! Four read-only routes that the GCS radio panel + pairing card poll:
//!
//! - **`GET /api/wfb`** — the current link status (state, RSSI, channel, packet
//!   stats, adapter info). On this native front the radio runs in a sibling
//!   process (no in-process manager to call), so the status comes from the durable
//!   logging store's most-recent `link.wfb_status` event, falling back to the
//!   `/run/ados/wfb-stats.json` sidecar the radio also writes. Both paths start
//!   from the same config-seeded base block, merge the producer body over it,
//!   re-assert the live regulatory domain, re-derive frequency/bandwidth from the
//!   channel, and add the `bitrate_mbps` shim, so the two reads are byte-identical.
//! - **`GET /api/wfb/history`** — link-quality history for the last N seconds
//!   (`?seconds=`, default 60, clamped 1..300), reshaped from the store's `link.*`
//!   metric aggregate into `{samples, count}`. An unreachable store degrades to the
//!   native empty history `{"samples": [], "count": 0}`.
//! - **`GET /api/wfb/pair`** — the pair-state snapshot (paired, peer device-id,
//!   paired-at, the blake2b-8 key fingerprint, auto-pair flag, role) plus the
//!   fleet slot table. The role-appropriate key file (`tx.key` for a drone,
//!   `rx.key` for a ground station) is the paired signal; its presence + exact
//!   64-byte size + a readable fingerprint are required, and the
//!   peer/paired-at/auto-pair come off the config. The slot table is the fleet
//!   registry, rendered through the same function the pair write uses.
//! - **`GET /api/wfb/pair/failover-status`** — the local-bind to cloud-relay
//!   failover state, from the store's most-recent `wfb.pair.failover` event, else
//!   the `/run/ados/wfb_failover.json` sidecar. Both are age-gated; with neither
//!   current the state is `null` under `stale: true`, never the healthy
//!   `"local"`.
//!
//! Every read is fault-tolerant: an absent store / sidecar / key file degrades to
//! an empty/default shape, never a 500. The routes carry no path params and never mutate, so
//! they are safe to serve natively while the channel/tx-power write routes stay on
//! the residual surface.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ados_protocol::paths;
use ados_protocol::sidecar::read_fresh_at;
use ados_protocol::wfb_status::{
    build_status_from_stats_file_at, derive_wfb_status, now_unix_micros, WfbStatusConfig,
    WFB_STALE_AGE_US,
};
use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Runtime-dir seam: the sidecar files the radio writes.
// ---------------------------------------------------------------------------

/// The config path the status base block seeds from (`ADOS_CONFIG`, default
/// `/etc/ados/config.yaml`). The shared loader takes a path because the cloud
/// relay resolves the same config differently.
fn status_config_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The live wfb stats sidecar (`<run dir>/wfb-stats.json`), written ~once per
/// second by whichever manager owns the radio.
fn wfb_stats_path(run_dir: &Path) -> PathBuf {
    run_dir.join(paths::WFB_STATS_JSON)
}

/// The local-bind to cloud-relay failover sidecar (`<run dir>/wfb_failover.json`),
/// written by the always-on auto-pair supervisor.
fn wfb_failover_path(run_dir: &Path) -> PathBuf {
    run_dir.join(paths::WFB_FAILOVER_JSON)
}

// ---------------------------------------------------------------------------
// GET /api/wfb — link status.
// ---------------------------------------------------------------------------

/// `GET /api/wfb` → the current WFB-ng link status.
///
/// Reads the radio's full status body from the store's most-recent
/// `link.wfb_status` event while that row is fresh, else from the
/// `wfb-stats.json` sidecar (see `choose_wfb_status`). Both paths produce a
/// byte-identical body: the config-seeded base, the producer payload merged over
/// it, the live `regulatory_domain` re-asserted, the frequency/bandwidth
/// re-derived from the channel, and the `bitrate_mbps` shim. Guaranteed 200.
pub async fn get_wfb_status(State(state): State<AppState>) -> Json<Value> {
    let cfg = WfbStatusConfig::load(&status_config_path());
    let store = latest_wfb_status(&state).await;
    Json(choose_wfb_status(
        store,
        &cfg,
        &wfb_stats_path(&state.run_dir),
        now_unix_micros(),
    ))
}

/// Pick the WFB status source.
///
/// A fresh store row wins: the radio ships its full status body there on every
/// change and at least every 8 s. A store row older than the staleness window
/// does not: the radio rewrites `wfb-stats.json` every heartbeat independently
/// of the store, so when its event emission lags or stops the sidecar still
/// carries the live link. The stale row is used only when there is no sidecar at
/// all, where it reports `state: "stale"` rather than nothing.
pub(crate) fn choose_wfb_status(
    store: Option<(Map<String, Value>, i64)>,
    cfg: &WfbStatusConfig,
    sidecar: &Path,
    now_us: i64,
) -> Value {
    match store {
        Some((detail, ts_us)) if ts_us > 0 && now_us - ts_us <= WFB_STALE_AGE_US => {
            derive_wfb_status(&detail, ts_us, cfg)
        }
        Some((detail, ts_us)) if !sidecar.exists() => derive_wfb_status(&detail, ts_us, cfg),
        _ => build_status_from_stats_file_at(cfg, sidecar),
    }
}

/// The most-recent full wfb-status snapshot + its emit timestamp, or `None`.
///
/// Queries the store for the newest `link.wfb_status` event and returns its `detail`
/// body (the full sidecar shape the radio shipped) plus the row's `ts_us` (used for the
/// staleness check). `None` when the store is unreachable, holds no such event, or the
/// `detail` is absent/non-object, so the caller falls back to the sidecar file.
async fn latest_wfb_status(state: &AppState) -> Option<(Map<String, Value>, i64)> {
    let rows = state
        .logd
        .rows("events", 1, Some("link.wfb_status"))
        .await?;
    let row = rows.first()?.as_object()?;
    let detail = row.get("detail")?.as_object()?;
    if detail.is_empty() {
        return None;
    }
    let ts_us = row
        .get("ts_us")
        .and_then(Value::as_f64)
        .map(|v| v as i64)
        .unwrap_or(0);
    Some((detail.clone(), ts_us))
}

// ---------------------------------------------------------------------------
// GET /api/wfb/history — link-quality history.
// ---------------------------------------------------------------------------

/// The `?seconds=` query for the history route. Defaults to 60 when absent,
/// matching the Python default; the value is clamped to `[1, 300]` in the handler.
#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    #[serde(default = "default_history_seconds")]
    seconds: i64,
}

fn default_history_seconds() -> i64 {
    60
}

/// `GET /api/wfb/history?seconds=N` → the link-quality history.
///
/// On this native front the radio's per-heartbeat link samples flow to the store as
/// `link.*` metrics, so the history is read from the store's aggregate and reshaped
/// into `{samples: [{timestamp, rssi_dbm, snr_db, loss_percent, bitrate_kbps}],
/// count}`. An unreachable store degrades to the native empty history `{"samples": [],
/// "count": 0}`.
pub async fn get_wfb_history(
    State(state): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> Json<Value> {
    let seconds = q.seconds.clamp(1, 300);
    if let Some(hist) = latest_wfb_history(&state, seconds).await {
        return Json(hist);
    }
    Json(json!({"samples": [], "count": 0}))
}

/// The aggregate metrics that compose a history sample, paired with their sample-row
/// key. `agg=last` per bucket picks the reading at that instant.
const HIST_KEYS: [(&str, &str); 4] = [
    ("link.rssi_dbm", "rssi_dbm"),
    ("link.snr_db", "snr_db"),
    ("link.loss_percent", "loss_percent"),
    ("link.bitrate_kbps", "bitrate_kbps"),
];

/// Reshape the store's `link.*` metric aggregate into the route's sample list.
///
/// Aggregates the four metrics into time buckets via `/v1/aggregate` and groups them by
/// bucket instant into `{samples, count}`. `None` when the store is unreachable / the
/// response does not parse / has no usable buckets, so the route falls back to the
/// native empty history.
async fn latest_wfb_history(state: &AppState, seconds: i64) -> Option<Value> {
    let seconds = seconds.clamp(1, 300);
    let mut params: Vec<(&str, String)> = vec![
        ("since", format!("-{seconds}s")),
        ("bucket", "auto".to_string()),
        ("agg", "last".to_string()),
    ];
    for (metric, _key) in HIST_KEYS {
        params.push(("metric", metric.to_string()));
    }
    let parsed = state.logd.query_json("/v1/aggregate", &params).await?;
    let buckets = parsed.get("data")?.as_array()?;

    // Group the per-metric buckets into one sample per bucket instant. A BTreeMap
    // keeps the samples sorted by the bucket timestamp, matching the Python
    // `sorted(by_ts.items())`.
    let mut by_ts: std::collections::BTreeMap<i64, Map<String, Value>> =
        std::collections::BTreeMap::new();
    for b in buckets {
        let Some(obj) = b.as_object() else { continue };
        let metric = obj.get("metric").and_then(Value::as_str);
        let key = metric.and_then(|m| {
            HIST_KEYS
                .iter()
                .find(|(name, _)| *name == m)
                .map(|(_, k)| *k)
        });
        let bucket_us = obj.get("bucket_us").and_then(Value::as_f64);
        let (Some(key), Some(bucket_us)) = (key, bucket_us) else {
            continue;
        };
        let slot = by_ts.entry(bucket_us as i64).or_default();
        slot.insert(
            key.to_string(),
            obj.get("value").cloned().unwrap_or(Value::Null),
        );
    }

    let samples: Vec<Value> = by_ts
        .into_iter()
        .map(|(ts, mut vals)| {
            // `timestamp` is the first key in the Python dict literal
            // (`{"timestamp": ..., **vals}`); serde_json preserves insertion order
            // with the `preserve_order` feature off — both ends emit a JSON object,
            // and the harness compares by key, so the order is not load-bearing.
            let mut obj = Map::new();
            obj.insert("timestamp".to_string(), json!(iso_from_us(ts)));
            obj.append(&mut vals);
            Value::Object(obj)
        })
        .collect();
    let count = samples.len();
    Some(json!({"samples": samples, "count": count}))
}

// ---------------------------------------------------------------------------
// GET /api/wfb/pair — pair-state snapshot.
// ---------------------------------------------------------------------------

/// `GET /api/wfb/pair` → the pair-state snapshot (`crate::wfb_pair_state`)
/// plus the fleet slot table.
///
/// `slots` is the fleet roster: which drone holds which slot, and when it was
/// issued. It used to be returned only by the pair WRITE, so reading it meant
/// re-pairing a drone or opening the registry file by hand — neither of which an
/// operator diagnosing a live fleet link can do.
pub async fn get_wfb_pair_status(State(state): State<AppState>) -> Json<Value> {
    let paths = &state.pairing_paths;
    let role = crate::wfb_pair_state::bind_role(paths);
    let status = crate::wfb_pair_state::status(&paths.config, &paths.wfb_key_dir, role);

    // The fleet roster, served through the same renderer the pair write uses so
    // both stay field-for-field identical and neither can leak a slot's relay
    // secret. Empty on a drone, which has no registry: a fleet's slots are
    // issued by the ground station and only it holds the table.
    let slots =
        crate::routes::gs_wfb_pair::slot_table(&crate::routes::gs_wfb_pair::load_registry());

    Json(pair_snapshot(status.to_json(), slots))
}

/// Compose the `GET /api/wfb/pair` body.
///
/// Split out so the response SHAPE is a unit under test. The fleet roster was
/// missing from this body for the whole life of the route and nothing failed,
/// because nothing asserted what the read is supposed to contain.
fn pair_snapshot(mut status: Map<String, Value>, slots: Vec<Value>) -> Value {
    status.insert("slots".to_string(), Value::Array(slots));
    Value::Object(status)
}

// ---------------------------------------------------------------------------
// GET /api/wfb/pair/failover-status — local-bind to cloud-relay failover state.
// ---------------------------------------------------------------------------

/// The failover states the route validates against. `failed` is tolerated but never
/// produced.
const FAILOVER_STATES: [&str; 3] = ["local", "cloud_relay", "failed"];

/// How fresh a failover reading must be to be reported as the current state: the
/// registry window for `wfb_failover`, the same window the ground-station
/// snapshot routes apply.
const FAILOVER_FRESH_S: f64 = paths::WFB_FAILOVER_MAX_AGE_MS as f64 / 1000.0;

/// `GET /api/wfb/pair/failover-status` →
/// `{"failover_state": <state>|null, "stale": <bool>}`.
///
/// Reads the store's most-recent `wfb.pair.failover` event, falling back to the
/// `wfb_failover.json` sidecar. Both are age-gated; with neither current the
/// state is `null` under `stale: true`.
///
/// ## Why `"local"` is not the fallback any more
///
/// `local` is the good state — the pair lane is bound locally, no cloud relay in
/// the path. Reporting it for "we have no idea" meant that a node which had
/// failed over to the cloud relay and then lost the producer of this event read
/// as locally bound, and that a node with no failover tracking at all read as
/// affirmatively healthy. Both directions of that mistake change what an
/// operator does about the link. The `stale` flag is always present so the
/// verdict is read off a key rather than inferred from one's absence.
pub async fn get_failover_status(State(state): State<AppState>) -> Json<Value> {
    if let Some(s) = latest_wfb_failover(&state).await {
        return Json(failover_body(Some(&s)));
    }

    let path = wfb_failover_path(&state.run_dir);
    let Some(map) = fresh_failover_sidecar(&path, SystemTime::now()) else {
        return Json(failover_body(None));
    };
    // Best-effort schema-drift signal (never reject): warn on a producer/reader
    // version mismatch, then read anyway.
    let got = map.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("wfb_failover") {
        ados_protocol::sidecar::check_sidecar_version("wfb_failover", got, ours);
    }
    Json(failover_body(map.get("state").and_then(Value::as_str)))
}

/// The response body for a resolved (or unresolvable) failover state.
///
/// A state inside [`FAILOVER_STATES`] is reported with `stale: false`. Anything
/// else — no current source, an absent `state`, or a value this build does not
/// recognise — is `failover_state: null` with `stale: true`. It is NOT coerced to
/// `"local"`: that is the healthy state, and claiming it for an unknown one
/// tells the operator the pair lane is locally bound when nothing established
/// that.
fn failover_body(state: Option<&str>) -> Value {
    match state {
        Some(s) if FAILOVER_STATES.contains(&s) => json!({"failover_state": s, "stale": false}),
        _ => json!({"failover_state": Value::Null, "stale": true}),
    }
}

/// The failover sidecar as an object, only when its mtime is within
/// [`FAILOVER_FRESH_S`] of `now`. `None` for an absent / unreadable / unparseable
/// / non-object / aged file.
///
/// The producer rewrites this file whole on each transition check, so its mtime
/// is the age of the reading. A file left behind by a dead producer used to be
/// served verbatim forever.
fn fresh_failover_sidecar(path: &Path, now: SystemTime) -> Option<Map<String, Value>> {
    // A future mtime makes the age unprovable (a backward clock step on an
    // RTC-less SBC), which reads as stale, never as fresh.
    let window = std::time::Duration::from_millis(paths::WFB_FAILOVER_MAX_AGE_MS);
    read_fresh_at::<Map<String, Value>>(path, Some(window), now).fresh()
}

/// The store's most-recent failover state, validated to the accepted set, or
/// `None` when the store is unreachable / has no such event / carries an
/// unrecognized value / the row is older than [`FAILOVER_FRESH_S`], so the route
/// falls back to the sidecar and then to the stale reply.
async fn latest_wfb_failover(state: &AppState) -> Option<String> {
    let rows = state
        .logd
        .rows("events", 1, Some("wfb.pair.failover"))
        .await?;
    let row = rows.first()?.as_object()?;
    let ts_us = row.get("ts_us").and_then(Value::as_i64)?;
    let now_us = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_micros() as i64;
    if (now_us - ts_us) as f64 / 1_000_000.0 > FAILOVER_FRESH_S {
        return None;
    }
    let detail = row.get("detail")?.as_object()?;
    let s = detail.get("state")?.as_str()?;
    if FAILOVER_STATES.contains(&s) {
        Some(s.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Small shared helpers.
// ---------------------------------------------------------------------------

/// Render a microsecond-epoch timestamp as an ISO-8601 UTC string ending in `Z`,
/// matching the Python `_iso_from_us` (`datetime.fromtimestamp(...).isoformat()`
/// with `+00:00` replaced by `Z`). Microsecond precision is preserved when the
/// timestamp carries a sub-second remainder, matching `isoformat()`.
fn iso_from_us(ts_us: i64) -> String {
    let secs = ts_us.div_euclid(1_000_000);
    let micros = ts_us.rem_euclid(1_000_000);
    let base = iso8601_from_unix_secs(secs);
    if micros == 0 {
        format!("{base}Z")
    } else {
        // `datetime.isoformat()` emits 6-digit microseconds when non-zero.
        format!("{base}.{micros:06}Z")
    }
}

/// Format a Unix-epoch second count as `YYYY-MM-DDTHH:MM:SS` (UTC, no offset).
/// The civil-from-days conversion keeps it correct across month/year/leap
/// boundaries without a date-time dependency.
fn iso8601_from_unix_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Howard Hinnant's civil_from_days: days since the Unix epoch → (y, m, d).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}")
}

#[cfg(test)]
mod tests;
