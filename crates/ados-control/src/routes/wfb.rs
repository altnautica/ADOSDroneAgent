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
//! the same empty/default shape the FastAPI route returns when its own source is
//! unavailable, never a 500. The routes carry no path params and never mutate, so
//! they are safe to serve natively while the channel/tx-power write routes stay on
//! the residual surface.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ados_protocol::wfb_status::{
    build_status_from_stats_file_at, derive_wfb_status, WfbStatusConfig,
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

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets + sentinels resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The live wfb stats sidecar (`/run/ados/wfb-stats.json`), written ~once per
/// second by whichever manager owns the radio.
fn wfb_stats_path() -> PathBuf {
    run_dir().join("wfb-stats.json")
}

/// The local-bind to cloud-relay failover sidecar (`/run/ados/wfb_failover.json`),
/// written by the always-on auto-pair supervisor.
fn wfb_failover_path() -> PathBuf {
    run_dir().join("wfb_failover.json")
}

// ---------------------------------------------------------------------------
// GET /api/wfb — link status.
// ---------------------------------------------------------------------------

/// `GET /api/wfb` → the current WFB-ng link status.
///
/// Reads the radio's full status body from the store's most-recent
/// `link.wfb_status` event, falling back to the `wfb-stats.json` sidecar when the
/// store is unreachable or has captured no event yet. Both paths produce a
/// byte-identical body: the config-seeded base, the producer payload merged over
/// it, the live `regulatory_domain` re-asserted, the frequency/bandwidth
/// re-derived from the channel, and the `bitrate_mbps` shim. Guaranteed 200.
pub async fn get_wfb_status(State(state): State<AppState>) -> Json<Value> {
    let cfg = WfbStatusConfig::load(&status_config_path());

    // Store-first: the radio ships the full status body to the durable store each
    // heartbeat as a `link.wfb_status` event. The base regulatory domain (one live
    // `iw reg get`) is the value both paths carry; the stored body's `reg_domain`
    // (a different key) never overwrites it.
    if let Some((detail, ts_us)) = latest_wfb_status(&state).await {
        return Json(derive_wfb_status(&detail, ts_us, &cfg));
    }

    // Sidecar fallback: read `wfb-stats.json`, merge over the base, flip to
    // `"stale"` when the file mtime is older than 10 s.
    Json(build_status_from_stats_file_at(&cfg, &wfb_stats_path()))
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

/// `GET /api/wfb/pair` → the pair-state snapshot ([`crate::wfb_pair_state`])
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

/// How fresh a failover reading must be to be reported as the current state: 10
/// seconds, the same window the ground-station snapshot routes apply.
const FAILOVER_FRESH_S: f64 = 10.0;

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

    let path = wfb_failover_path();
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
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    // A future mtime makes the age unprovable (a backward clock step on an
    // RTC-less SBC), which must not read as fresh.
    let age_s = now.duration_since(modified).ok()?.as_secs_f64();
    if age_s > FAILOVER_FRESH_S {
        return None;
    }
    match serde_json::from_str::<Value>(&std::fs::read_to_string(path).ok()?) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
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
mod tests {
    use super::*;
    // The derivation these tests pin lives in the shared module now; the pins
    // stay here because this route is one of its two callers and its served
    // body is what they defend.
    use ados_protocol::wfb_status::{finalize_wfb_status, now_unix_micros, wfb_base_block};

    /// The exact base block the Python `_base_block` builds for an all-default
    /// config (no `video.wfb` fields, regulatory domain forced to a known value).
    /// Re-asserting the live `regulatory_domain` is stubbed out by overwriting it
    /// after the build, so the test does not depend on `iw` being present.
    fn base_block_fixed_reg(cfg: &WfbStatusConfig, reg: &str) -> Map<String, Value> {
        let mut b = wfb_base_block(cfg);
        b.insert("regulatory_domain".to_string(), json!(reg));
        b
    }

    #[test]
    fn base_block_is_the_zero_default_for_an_empty_config() {
        let cfg = WfbStatusConfig::default();
        let b = base_block_fixed_reg(&cfg, "unknown");

        // The signal triple is the point of this test. This block is the body a
        // node with NO radio serves — no adapter, no producer, nothing ever
        // sampled — and it used to carry `rssi_dbm: -100.0`, `noise_dbm: -95.0`,
        // `snr_db: 0.0`: a complete, credible weak link that the GCS signal
        // meter, the durable link series and the operator all read as a
        // measurement. Only `rssi_dbm` was folded back to null downstream, so
        // the noise floor and the SNR rode the heartbeat verbatim. Asserted by
        // name as well as inside the shape below, so a future edit cannot quietly
        // substitute another plausible number.
        for key in ["rssi_dbm", "noise_dbm", "snr_db"] {
            assert_eq!(
                b.get(key),
                Some(&Value::Null),
                "{key} must carry no reading on a node with no radio"
            );
        }

        // The 27-field key set, pinned as a whole so a field cannot appear or
        // vanish from the served body unnoticed.
        let want = json!({
            "state": "disabled",
            "interface": "",
            "channel": 0,
            "frequency_mhz": 0,
            "bandwidth_mhz": 0,
            "adapter": {"driver": "", "chipset": "", "supports_monitor": false},
            "adapter_chipset": null,
            // Null, never false: the base means "no adapter scan reading", and
            // a false would be a fabricated measured no-injection claim.
            "adapter_injection_ok": null,
            "rssi_dbm": null,
            "noise_dbm": null,
            "snr_db": null,
            "packets_received": 0,
            "packets_lost": 0,
            "loss_percent": 0.0,
            "fec_recovered": 0,
            "fec_failed": 0,
            "bitrate_kbps": 0,
            "rx_silent_seconds": null,
            "restart_count": 0,
            "samples": 0,
            "tx_power_dbm": null,
            "tx_power_max_dbm": null,
            "topology": null,
            "mcs_index": null,
            "regulatory_domain": "unknown",
        });
        assert_eq!(Value::Object(b), want);
    }

    #[test]
    fn base_block_seeds_the_config_wfb_fields() {
        let cfg = WfbStatusConfig {
            channel: 149,
            tx_power_dbm: json!(10),
            tx_power_max_dbm: json!(15),
            topology: json!("one-to-one"),
            mcs_index: json!(2),
        };
        let b = base_block_fixed_reg(&cfg, "US");
        assert_eq!(b["channel"], json!(149));
        assert_eq!(b["tx_power_dbm"], json!(10));
        assert_eq!(b["tx_power_max_dbm"], json!(15));
        assert_eq!(b["topology"], json!("one-to-one"));
        assert_eq!(b["mcs_index"], json!(2));
        assert_eq!(b["regulatory_domain"], json!("US"));
    }

    #[test]
    fn finalize_derives_frequency_bandwidth_and_bitrate_mbps() {
        // A merged body on channel 149 with 5000 kbps must re-derive 5745/20 and a 5.0 mbps shim.
        let mut merged = Map::new();
        merged.insert("channel".to_string(), json!(149));
        merged.insert("bitrate_kbps".to_string(), json!(5000));
        let out = finalize_wfb_status(merged);
        assert_eq!(out["frequency_mhz"], json!(5745));
        assert_eq!(out["bandwidth_mhz"], json!(20));
        assert_eq!(out["bitrate_mbps"], json!(5.0));
    }

    #[test]
    fn finalize_unknown_channel_leaves_freq_bandwidth_untouched_and_zero_bitrate() {
        // An unknown channel does not re-derive freq/bandwidth (stay as merged in),
        // and a zero/absent bitrate yields a 0.0 shim.
        let mut merged = Map::new();
        merged.insert("channel".to_string(), json!(7)); // not a standard WFB channel
        merged.insert("frequency_mhz".to_string(), json!(2442));
        merged.insert("bandwidth_mhz".to_string(), json!(40));
        let out = finalize_wfb_status(merged);
        assert_eq!(out["frequency_mhz"], json!(2442));
        assert_eq!(out["bandwidth_mhz"], json!(40));
        assert_eq!(out["bitrate_mbps"], json!(0.0));
    }

    #[test]
    fn derive_merges_body_over_base_and_keeps_live_reg_domain() {
        // The stored body carries link values + a `reg_domain` key (NOT
        // `regulatory_domain`), so the base's live `regulatory_domain` survives.
        let cfg = WfbStatusConfig::default();
        let mut detail = Map::new();
        detail.insert("state".to_string(), json!("active"));
        detail.insert("channel".to_string(), json!(149));
        detail.insert("rssi_dbm".to_string(), json!(-55.0));
        detail.insert("bitrate_kbps".to_string(), json!(8000));
        detail.insert("reg_domain".to_string(), json!("XX")); // different key, ignored
                                                              // A fresh ts_us so the staleness flip does not fire.
        let ts_us = now_unix_micros();
        let out = derive_wfb_status(&detail, ts_us, &cfg);
        assert_eq!(out["state"], json!("active"));
        assert_eq!(out["rssi_dbm"], json!(-55.0));
        assert_eq!(out["frequency_mhz"], json!(5745));
        assert_eq!(out["bandwidth_mhz"], json!(20));
        assert_eq!(out["bitrate_mbps"], json!(8.0));
        // The live regulatory_domain (here whatever `iw` reports, possibly
        // "unknown") is never the stored `reg_domain`.
        assert_ne!(out["regulatory_domain"], json!("XX"));
        assert!(out.get("reg_domain").is_some()); // the stored extra is preserved
    }

    #[test]
    fn derive_flips_state_to_stale_for_an_old_event() {
        let cfg = WfbStatusConfig::default();
        let mut detail = Map::new();
        detail.insert("state".to_string(), json!("active"));
        detail.insert("channel".to_string(), json!(36));
        // An event 20 s old (> the 10 s threshold) flips to "stale".
        let ts_us = now_unix_micros() - 20_000_000;
        let out = derive_wfb_status(&detail, ts_us, &cfg);
        assert_eq!(out["state"], json!("stale"));
    }

    #[test]
    fn an_aged_failover_sidecar_does_not_read_as_a_current_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb_failover.json");
        std::fs::write(&path, r#"{"state":"cloud_relay","version":1}"#).unwrap();

        let fresh =
            fresh_failover_sidecar(&path, SystemTime::now()).expect("a just-written file is fresh");
        assert_eq!(fresh.get("state"), Some(&json!("cloud_relay")));

        let aged = SystemTime::now() + std::time::Duration::from_secs_f64(FAILOVER_FRESH_S + 5.0);
        assert!(
            fresh_failover_sidecar(&path, aged).is_none(),
            "a file older than the window must not be readable as current"
        );
    }

    /// An absent sidecar is not a state at all — in particular not `"local"`.
    #[test]
    fn an_absent_failover_sidecar_is_not_a_state() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fresh_failover_sidecar(&dir.path().join("nope.json"), SystemTime::now()).is_none());
    }

    /// The body an unresolvable failover reading produces: `null` + `stale: true`,
    /// never the healthy `"local"`.
    #[test]
    fn an_unknown_failover_state_reports_null_and_stale_not_local() {
        // A recognised state is reported as current.
        assert_eq!(
            failover_body(Some("cloud_relay")),
            json!({"failover_state": "cloud_relay", "stale": false})
        );
        // Nothing current, an absent `state`, and a value this build does not
        // recognise all report the same honest unknown.
        let unknown = json!({"failover_state": Value::Null, "stale": true});
        assert_eq!(failover_body(None), unknown);
        assert_eq!(failover_body(Some("bogus")), unknown);
        // The key is present and explicitly null rather than dropped.
        assert!(failover_body(None)
            .as_object()
            .unwrap()
            .contains_key("failover_state"));
    }

    #[test]
    fn iso_from_us_renders_a_z_suffixed_utc_string() {
        // Epoch zero → 1970-01-01T00:00:00Z.
        assert_eq!(iso_from_us(0), "1970-01-01T00:00:00Z");
        // 2021-01-01T00:00:00Z = 1609459200 s = 1609459200000000 us.
        assert_eq!(iso_from_us(1_609_459_200_000_000), "2021-01-01T00:00:00Z");
        // A sub-second remainder keeps 6-digit microseconds, matching isoformat().
        assert_eq!(
            iso_from_us(1_609_459_200_500_000),
            "2021-01-01T00:00:00.500000Z"
        );
    }

    #[test]
    fn history_query_defaults_seconds_to_60() {
        // serde defaults the field when `?seconds=` is absent.
        let q: HistoryQuery = serde_urlencoded_like("").unwrap();
        assert_eq!(q.seconds, 60);
        let q: HistoryQuery = serde_urlencoded_like("seconds=30").unwrap();
        assert_eq!(q.seconds, 30);
    }

    /// Parse a query string into the typed query struct via serde_json's value
    /// path (the crate has no urlencoded dep; this exercises the `default` attr
    /// for the absent-field case, which is the behavior under test).
    fn serde_urlencoded_like(qs: &str) -> Result<HistoryQuery, String> {
        // Translate the tiny query grammar used in the test into a JSON object.
        let mut map = serde_json::Map::new();
        for pair in qs.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or("bad pair")?;
            let parsed: i64 = v.parse().map_err(|_| "bad int")?;
            map.insert(k.to_string(), json!(parsed));
        }
        serde_json::from_value(Value::Object(map)).map_err(|e| e.to_string())
    }

    #[test]
    fn build_status_from_an_absent_sidecar_is_the_finalized_base() {
        // With no wfb-stats.json the route returns the base block (the FastAPI
        // `except: return base` path returns the bare base, no finalize). The
        // stats path is threaded in explicitly (an absent file under a tempdir), so
        // the test never mutates the process-global `ADOS_RUN_DIR`.
        let dir = tempfile::tempdir().unwrap();
        let stats = dir.path().join("wfb-stats.json");
        let cfg = WfbStatusConfig::default();
        let out = build_status_from_stats_file_at(&cfg, &stats);
        // The bare base carries the 27 keys with no `bitrate_mbps` shim (the
        // FastAPI absent-file path skips finalize).
        assert_eq!(out["state"], json!("disabled"));
        assert_eq!(out["channel"], json!(0));
        assert!(out.get("bitrate_mbps").is_none());
    }

    /// A pair-status snapshot with the given identity fields.
    fn snapshot(
        paired: bool,
        peer: Value,
        fingerprint: Value,
        role: &'static str,
    ) -> Map<String, Value> {
        crate::wfb_pair_state::PairStatus {
            paired,
            peer,
            paired_at: Value::Null,
            fingerprint,
            auto_pair_enabled: true,
            role,
        }
        .to_json()
    }

    #[test]
    fn the_pair_read_serves_the_fleet_slot_table() {
        // Which drone holds which slot is the first question a fleet link fault
        // raises, and it was returned ONLY by the pair WRITE — so reading it
        // meant re-pairing a drone, which is exactly what an operator
        // diagnosing a live fleet must not do, or opening the registry file
        // over a shell.
        use ados_groundlink::FleetRegistry;
        let mut registry = FleetRegistry::default();
        registry.allocate("drone-a").unwrap();
        registry.allocate("drone-b").unwrap();
        let slots = crate::routes::gs_wfb_pair::slot_table(&registry);

        let body = pair_snapshot(
            snapshot(true, json!("drone-a"), json!("0123456789abcdef"), "gs"),
            slots,
        );
        let table = body["slots"].as_array().expect("the read carries a roster");
        assert_eq!(table.len(), 2);
        assert_eq!(table[0]["slot"], 1);
        assert_eq!(table[0]["device_id"], "drone-a");
        assert_eq!(table[1]["slot"], 2);
        assert_eq!(table[1]["device_id"], "drone-b");
        assert!(table[0]["paired_at_ms"].as_u64().unwrap() > 0);

        // The existing fields are untouched, so a client reading the old shape
        // is unaffected.
        assert_eq!(body["paired"], true);
        assert_eq!(body["role"], "gs");
        assert_eq!(body["fingerprint"], "0123456789abcdef");
    }

    #[test]
    fn the_pair_read_never_serves_a_slots_relay_secret() {
        // A slot carries a per-pair relay secret. Rendering the roster on a
        // SECOND route is exactly how that would escape, so the read goes
        // through the same explicit-field renderer the write does rather than
        // building its own table.
        use ados_groundlink::FleetRegistry;
        let mut registry = FleetRegistry::default();
        registry.allocate("drone-a").unwrap();
        let secret = registry
            .slots()
            .next()
            .unwrap()
            .relay_secret
            .clone()
            .expect("allocation issues a secret");

        let body = pair_snapshot(
            snapshot(true, json!("drone-a"), Value::Null, "gs"),
            crate::routes::gs_wfb_pair::slot_table(&registry),
        );
        let rendered = serde_json::to_string(&body).unwrap();
        assert!(!secret.is_empty());
        assert!(
            !rendered.contains(&secret),
            "the relay secret reached the pair read: {rendered}"
        );
        assert!(!rendered.contains("relay_secret"));
    }

    #[test]
    fn a_node_holding_no_registry_serves_an_empty_roster_not_a_null() {
        // A drone holds no registry — slots are issued by the ground station.
        // An empty array keeps the client on one code path.
        let body = pair_snapshot(
            snapshot(false, Value::Null, Value::Null, "drone"),
            Vec::new(),
        );
        assert_eq!(body["slots"], json!([]));
    }
}
