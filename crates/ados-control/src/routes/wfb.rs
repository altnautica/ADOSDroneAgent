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
//!   paired-at, the blake2b-8 key fingerprint, auto-pair flag, role). The
//!   role-appropriate key file (`tx.key` for a drone, `rx.key` for a ground
//!   station) is the paired signal; its presence + exact 64-byte size + a readable
//!   fingerprint are required, and the peer/paired-at/auto-pair come off the config.
//! - **`GET /api/wfb/pair/failover-status`** — the local-bind to cloud-relay
//!   failover state, from the store's most-recent `wfb.pair.failover` event, else
//!   the `/run/ados/wfb_failover.json` sidecar, defaulting to `"local"`.
//!
//! Every read is fault-tolerant: an absent store / sidecar / key file degrades to
//! the same empty/default shape the FastAPI route returns when its own source is
//! unavailable, never a 500. The routes carry no path params and never mutate, so
//! they are safe to serve natively while the channel/tx-power write routes stay on
//! the residual surface.

use std::path::{Path, PathBuf};
use std::process::Command;

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Standard WFB-ng channels (frequency + bandwidth lookup).
// ---------------------------------------------------------------------------

/// A WFB-ng channel: the channel number, its 5 GHz centre frequency, and the
/// bandwidth. Mirrors the Python `WfbChannel` (default bandwidth 20 MHz).
#[derive(Clone, Copy)]
struct WfbChannel {
    channel_number: i64,
    frequency_mhz: i64,
    bandwidth_mhz: i64,
}

/// The standard 5 GHz channels usable with WFB-ng on the RTL8812 family: the
/// U-NII-1 sub-band (36/40/44/48) and the U-NII-3 sub-band (149/153/157/161/165).
/// Mirrors the Python `STANDARD_CHANNELS` list exactly (each 20 MHz wide).
const STANDARD_CHANNELS: [WfbChannel; 9] = [
    WfbChannel { channel_number: 36, frequency_mhz: 5180, bandwidth_mhz: 20 },
    WfbChannel { channel_number: 40, frequency_mhz: 5200, bandwidth_mhz: 20 },
    WfbChannel { channel_number: 44, frequency_mhz: 5220, bandwidth_mhz: 20 },
    WfbChannel { channel_number: 48, frequency_mhz: 5240, bandwidth_mhz: 20 },
    WfbChannel { channel_number: 149, frequency_mhz: 5745, bandwidth_mhz: 20 },
    WfbChannel { channel_number: 153, frequency_mhz: 5765, bandwidth_mhz: 20 },
    WfbChannel { channel_number: 157, frequency_mhz: 5785, bandwidth_mhz: 20 },
    WfbChannel { channel_number: 161, frequency_mhz: 5805, bandwidth_mhz: 20 },
    WfbChannel { channel_number: 165, frequency_mhz: 5825, bandwidth_mhz: 20 },
];

/// Look up a channel by number, or `None` for an unknown number. Mirrors the
/// Python `get_channel`.
fn get_channel(channel_number: i64) -> Option<WfbChannel> {
    STANDARD_CHANNELS
        .iter()
        .find(|c| c.channel_number == channel_number)
        .copied()
}

// ---------------------------------------------------------------------------
// Config seam: the `video.wfb` slice the status base block reads.
// ---------------------------------------------------------------------------

/// The `video.wfb` fields the status base block seeds its zero-default block with.
/// Each is optional so an absent section reads the same value the loaded Python
/// config would for an unset field (the base block treats a missing field as the
/// Python default: `channel` defaults to `0`, the rest to JSON `null`).
#[derive(Debug, Clone, Default, Deserialize)]
struct WfbConfigSection {
    #[serde(default)]
    channel: Option<i64>,
    #[serde(default)]
    tx_power_dbm: Option<Value>,
    #[serde(default)]
    tx_power_max_dbm: Option<Value>,
    #[serde(default)]
    topology: Option<Value>,
    #[serde(default)]
    mcs_index: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct VideoSection {
    #[serde(default)]
    wfb: WfbConfigSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct StatusConfig {
    #[serde(default)]
    video: VideoSection,
}

impl StatusConfig {
    /// Load the `video.wfb` slice from the config path (`ADOS_CONFIG`, default
    /// `/etc/ados/config.yaml`). A missing or unparseable file yields the
    /// all-defaults slice, so the status route still answers a usable body.
    fn load() -> Self {
        let path = std::env::var("ADOS_CONFIG")
            .unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string());
        Self::load_from(Path::new(&path))
    }

    fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_norway::from_str(&text).unwrap_or_default(),
            Err(_) => StatusConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime-dir seam: the sidecar files the radio writes.
// ---------------------------------------------------------------------------

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
    let cfg = StatusConfig::load();

    // Store-first: the radio ships the full status body to the durable store each
    // heartbeat as a `link.wfb_status` event. The base regulatory domain (one live
    // `iw reg get`) is the value both paths carry; the stored body's `reg_domain`
    // (a different key) never overwrites it.
    if let Some((detail, ts_us)) = latest_wfb_status(&state).await {
        return Json(derive_wfb_status(&detail, ts_us, &cfg));
    }

    // Sidecar fallback: read `wfb-stats.json`, merge over the base, flip to
    // `"stale"` when the file mtime is older than 10 s.
    Json(build_status_from_stats_file(&cfg))
}

/// The most-recent full wfb-status snapshot + its emit timestamp, or `None`.
///
/// Queries the store for the newest `link.wfb_status` event and returns its
/// `detail` body (the full sidecar shape the radio shipped) plus the row's
/// `ts_us` (used for the staleness check). `None` when the store is unreachable,
/// holds no such event, or the `detail` is absent/non-object, so the caller falls
/// back to the sidecar file. Mirrors the Python `latest_wfb_status`.
async fn latest_wfb_status(state: &AppState) -> Option<(Map<String, Value>, i64)> {
    let rows = logd_query_events(state, "link.wfb_status", 1).await?;
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

/// Map a stored status body back to the exact `/api/wfb` shape: the config-seeded
/// base, the body merged over it, an event-age staleness flip, then the shared
/// finalize legs (frequency/bandwidth + `bitrate_mbps`). Mirrors the Python
/// `derive_wfb_status`.
fn derive_wfb_status(detail: &Map<String, Value>, ts_us: i64, cfg: &StatusConfig) -> Value {
    let mut merged = base_block(cfg);
    // The base `regulatory_domain` (the live `iw reg get`) stays put: the stored
    // body carries `reg_domain` (a different key), so this merge never overwrites it.
    for (k, v) in detail {
        merged.insert(k.clone(), v.clone());
    }
    // Event-age staleness, mirroring the file-mtime flip on the sidecar path.
    let now_us = now_unix_micros();
    if ts_us > 0 && now_us - ts_us > STALE_AGE_US {
        merged.insert("state".to_string(), json!("stale"));
    }
    finalize_status(merged)
}

/// Beyond this age (microseconds) a stored status event is treated as stale,
/// mirroring the sidecar path's `mtime > 10 s` flip. Mirrors `_STALE_AGE_US`.
const STALE_AGE_US: i64 = 10_000_000;

/// Compose a `/api/wfb` body from the `wfb-stats.json` sidecar.
///
/// Merges the file payload over the config-seeded base, flips `state` to
/// `"stale"` when the file is older than 10 s, re-asserts the live regulatory
/// domain, and finalizes. An absent / unparseable / non-object file degrades to
/// the bare base block. Mirrors the Python `_build_status_from_stats_file`.
fn build_status_from_stats_file(cfg: &StatusConfig) -> Value {
    let base = base_block(cfg);
    let path = wfb_stats_path();

    // The mtime drives the staleness flip; compute the file age in seconds.
    let age_s = match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime
            .elapsed()
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        Err(_) => return finalize_base(base),
    };

    let payload = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            // A well-formed-but-non-object body returns the bare base, matching the
            // Python `if not isinstance(payload, dict): return base`.
            Ok(_) => return Value::Object(base),
            Err(_) => return finalize_base(base),
        },
        Err(_) => return finalize_base(base),
    };

    let mut merged = base;
    for (k, v) in payload {
        merged.insert(k, v);
    }
    if age_s > 10.0 {
        merged.insert("state".to_string(), json!("stale"));
    }
    // Re-assert the live regulatory domain over the payload (the file's body must
    // not overwrite it). The base already set it from one `iw reg get`.
    let reg = regulatory_domain();
    merged.insert("regulatory_domain".to_string(), json!(reg));
    finalize_status(merged)
}

/// The non-object / read-error degrade path: the Python returns the bare base
/// block (no finalize) only on a non-object payload; a missing file / read error
/// also returns the bare base. Both land here returning the base unchanged, which
/// is the FastAPI `except: return base` behavior.
fn finalize_base(base: Map<String, Value>) -> Value {
    Value::Object(base)
}

/// The config-seeded zero-default status block both read paths merge the actual
/// values over. `regulatory_domain` is the LIVE `iw reg get` value. Mirrors the
/// Python `_base_block`.
fn base_block(cfg: &StatusConfig) -> Map<String, Value> {
    let wfb = &cfg.video.wfb;
    let channel = wfb.channel.unwrap_or(0);
    let tx_power = wfb.tx_power_dbm.clone().unwrap_or(Value::Null);
    let tx_power_max = wfb.tx_power_max_dbm.clone().unwrap_or(Value::Null);
    let topology = wfb.topology.clone().unwrap_or(Value::Null);
    let mcs = wfb.mcs_index.clone().unwrap_or(Value::Null);

    let mut block = Map::new();
    block.insert("state".to_string(), json!("disabled"));
    block.insert("interface".to_string(), json!(""));
    block.insert("channel".to_string(), json!(channel));
    block.insert("frequency_mhz".to_string(), json!(0));
    block.insert("bandwidth_mhz".to_string(), json!(0));
    block.insert(
        "adapter".to_string(),
        json!({"driver": "", "chipset": "", "supports_monitor": false}),
    );
    block.insert("adapter_chipset".to_string(), Value::Null);
    block.insert("adapter_injection_ok".to_string(), json!(false));
    block.insert("rssi_dbm".to_string(), json!(-100.0));
    block.insert("noise_dbm".to_string(), json!(-95.0));
    block.insert("snr_db".to_string(), json!(0.0));
    block.insert("packets_received".to_string(), json!(0));
    block.insert("packets_lost".to_string(), json!(0));
    block.insert("loss_percent".to_string(), json!(0.0));
    block.insert("fec_recovered".to_string(), json!(0));
    block.insert("fec_failed".to_string(), json!(0));
    block.insert("bitrate_kbps".to_string(), json!(0));
    block.insert("rx_silent_seconds".to_string(), Value::Null);
    block.insert("restart_count".to_string(), json!(0));
    block.insert("samples".to_string(), json!(0));
    block.insert("tx_power_dbm".to_string(), tx_power);
    block.insert("tx_power_max_dbm".to_string(), tx_power_max);
    block.insert("topology".to_string(), topology);
    block.insert("mcs_index".to_string(), mcs);
    block.insert(
        "regulatory_domain".to_string(),
        json!(regulatory_domain()),
    );
    block
}

/// Apply the route-computed legs on top of a base+payload merge: re-derive
/// `frequency_mhz` / `bandwidth_mhz` from the channel number and add the
/// `bitrate_mbps` forward-compat shim. Mirrors the Python `_finalize_status`.
fn finalize_status(mut merged: Map<String, Value>) -> Value {
    let channel = merged
        .get("channel")
        .and_then(json_to_i64)
        .unwrap_or(0);
    if let Some(ch) = get_channel(channel) {
        merged.insert("frequency_mhz".to_string(), json!(ch.frequency_mhz));
        merged.insert("bandwidth_mhz".to_string(), json!(ch.bandwidth_mhz));
    }
    let bitrate_mbps = match merged.get("bitrate_kbps").and_then(Value::as_f64) {
        Some(bk) if bk > 0.0 => round3(bk / 1000.0),
        _ => 0.0,
    };
    merged.insert("bitrate_mbps".to_string(), json!(bitrate_mbps));
    Value::Object(merged)
}

/// Best-effort `iw reg get` first-line parse, returning the two-letter country
/// code, `"global"`, or `"unknown"` on any failure. Mirrors the Python
/// `_read_regulatory_domain`.
fn regulatory_domain() -> String {
    let output = match Command::new("iw").args(["reg", "get"]).output() {
        Ok(o) => o,
        Err(_) => return "unknown".to_string(),
    };
    if !output.status.success() {
        return "unknown".to_string();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let stripped = line.trim();
        if let Some(rest) = stripped.strip_prefix("country ") {
            // Format: "country US: DFS-FCC" — keep the two-letter code.
            let code = rest.split(':').next().unwrap_or("").trim();
            if code.is_empty() {
                return "unknown".to_string();
            }
            return code.to_string();
        }
        if stripped.starts_with("global") {
            return "global".to_string();
        }
    }
    "unknown".to_string()
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
/// On this native front the radio's per-heartbeat link samples flow to the store
/// as `link.*` metrics, so the history is read from the store's aggregate and
/// reshaped into `{samples: [{timestamp, rssi_dbm, snr_db, loss_percent,
/// bitrate_kbps}], count}`. An unreachable store degrades to the native empty
/// history `{"samples": [], "count": 0}`. Mirrors the Python `get_wfb_history`
/// native branch.
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

/// The aggregate metrics that compose a history sample, paired with their
/// sample-row key. `agg=last` per bucket picks the reading at that instant.
/// Mirrors the Python `_HIST_KEY`.
const HIST_KEYS: [(&str, &str); 4] = [
    ("link.rssi_dbm", "rssi_dbm"),
    ("link.snr_db", "snr_db"),
    ("link.loss_percent", "loss_percent"),
    ("link.bitrate_kbps", "bitrate_kbps"),
];

/// Reshape the store's `link.*` metric aggregate into the route's sample list.
///
/// Aggregates the four metrics into time buckets via `/v1/aggregate` and groups
/// them by bucket instant into `{samples, count}`. `None` when the store is
/// unreachable / the response does not parse / has no usable buckets, so the route
/// falls back to the native empty history. Mirrors the Python `latest_wfb_history`.
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
    let query = encode_query(&params);
    let path = format!("/v1/aggregate?{query}");

    let (status, body) = logd_get(state, &path).await.ok()?;
    if status >= 400 {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&body).ok()?;
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

/// `GET /api/wfb/pair` → the pair-state snapshot.
///
/// The role-appropriate key file (`tx.key` for a drone, `rx.key` for a ground
/// station) is the paired signal: it must be present, exactly 64 bytes, and yield
/// a readable blake2b-8 fingerprint. The peer device-id, paired-at, and the
/// auto-pair flag come off the config (with the legacy `ground_station.*` fallback
/// on the GS profile). Mirrors the Python `pair_manager.status(role)`.
pub async fn get_wfb_pair_status(State(state): State<AppState>) -> Json<Value> {
    let paths = &state.pairing_paths;
    let cfg = crate::config::PairingConfig::load_from(&paths.config);
    let (_profile, role) = current_role(&cfg.agent.profile);

    // The role-appropriate key file: tx.key for a drone, rx.key for a GS.
    let key_path = if role == "drone" {
        paths.wfb_key_dir.join("tx.key")
    } else {
        paths.wfb_key_dir.join("rx.key")
    };

    // paired := the file exists AND is exactly 64 bytes. A readable fingerprint is
    // then required; a 64-byte file whose fingerprint cannot be read reverts paired
    // to false, matching the Python `except (OSError, ValueError): paired = False`.
    let mut paired = std::fs::metadata(&key_path)
        .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES as u64)
        .unwrap_or(false);
    let mut fingerprint: Value = Value::Null;
    if paired {
        match read_public_fingerprint(&key_path) {
            Some(fp) => fingerprint = json!(fp),
            None => paired = false,
        }
    }

    // Peer / paired-at / auto-pair off the raw config dict, mirroring the Python
    // `_load_config_dict()` read (a present-but-non-string peer/paired-at reads as
    // null, an absent auto-pair flag defaults to true).
    let raw = load_config_value(&paths.config);
    let wfb_section = raw
        .get("video")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("wfb"))
        .filter(|v| v.is_object());

    let mut peer = wfb_section
        .and_then(|w| w.get("paired_with_device_id"))
        .filter(|v| v.is_string())
        .cloned()
        .unwrap_or(Value::Null);
    let mut paired_at = wfb_section
        .and_then(|w| w.get("paired_at"))
        .filter(|v| v.is_string())
        .cloned()
        .unwrap_or(Value::Null);
    let auto_pair_enabled = wfb_section
        .and_then(|w| w.get("auto_pair_enabled"))
        .map(json_truthy)
        .unwrap_or(true);

    // GS-profile fallback: a rig migrated from an older config may carry the pair
    // state under `ground_station.*` without the `video.wfb.*` mirror.
    if role == "gs" && peer.is_null() {
        let gs = raw.get("ground_station").filter(|v| v.is_object());
        peer = gs
            .and_then(|g| g.get("paired_drone_id"))
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or(Value::Null);
        if paired_at.is_null() {
            paired_at = gs
                .and_then(|g| g.get("paired_at"))
                .filter(|v| v.is_string())
                .cloned()
                .unwrap_or(Value::Null);
        }
    }

    Json(json!({
        "paired": paired,
        "paired_with_device_id": peer,
        "paired_at": paired_at,
        "fingerprint": fingerprint,
        "auto_pair_enabled": auto_pair_enabled,
        "role": role,
    }))
}

/// The exact 64-byte size a complete WFB-ng key file is. Mirrors
/// `WFB_KEY_FILE_BYTES`.
const WFB_KEY_FILE_BYTES: usize = 64;

/// The byte offset of the peer-public half (the second 32 bytes) inside a 64-byte
/// WFB key file. Mirrors `WFB_PUBLIC_HALF_OFFSET`.
const WFB_PUBLIC_HALF_OFFSET: usize = 32;

/// The 16-hex-char public-key fingerprint of a WFB key file, or `None` when the
/// file is absent or not exactly 64 bytes. The peer-public half is the second 32
/// bytes; the fingerprint is `blake2b(pub, digest_size=8)` rendered as 16
/// lowercase hex chars. Byte-identical to `key_mgr.read_public_fingerprint`.
fn read_public_fingerprint(path: &Path) -> Option<String> {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;
    let data = std::fs::read(path).ok()?;
    if data.len() != WFB_KEY_FILE_BYTES {
        return None;
    }
    let mut hasher = Blake2bVar::new(8).ok()?;
    hasher.update(&data[WFB_PUBLIC_HALF_OFFSET..]);
    let mut out = [0u8; 8];
    hasher.finalize_variable(&mut out).ok()?;
    Some(hex::encode(out))
}

/// Resolve the bind-protocol role from the agent's profile, mirroring the Python
/// `_current_role(app)` → `_agent_role_from_profile`. The profile is the
/// hyphen-wire form (`"drone"` / `"ground-station"`); the role is `"drone"` only
/// when the profile is exactly `"drone"`, else `"gs"`.
fn current_role(config_profile: &str) -> (String, String) {
    let (profile, _role) = crate::profile::current_profile_and_role(config_profile);
    let bind_role = if profile == "drone" { "drone" } else { "gs" };
    (profile, bind_role.to_string())
}

/// Load `/etc/ados/config.yaml` as a raw JSON value (objects/arrays/scalars
/// preserved), tolerating absence / a parse error / a non-object root with an
/// empty object. Mirrors the Python `_load_config_dict` for the pair-status read.
fn load_config_value(path: &Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return json!({}),
    };
    match serde_norway::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

// ---------------------------------------------------------------------------
// GET /api/wfb/pair/failover-status — local-bind to cloud-relay failover state.
// ---------------------------------------------------------------------------

/// The failover states the route validates against. `failed` is tolerated but
/// never produced. Mirrors `_FAILOVER_STATES`.
const FAILOVER_STATES: [&str; 3] = ["local", "cloud_relay", "failed"];

/// `GET /api/wfb/pair/failover-status` → `{"failover_state": <state>}`.
///
/// Reads the store's most-recent `wfb.pair.failover` event, falling back to the
/// `wfb_failover.json` sidecar, defaulting to `"local"` when neither has a value.
/// An unrecognized sidecar value also reads as `"local"`. Mirrors the Python
/// `get_failover_status`.
pub async fn get_failover_status(State(state): State<AppState>) -> Json<Value> {
    if let Some(s) = latest_wfb_failover(&state).await {
        return Json(json!({"failover_state": s}));
    }

    let path = wfb_failover_path();
    if !path.exists() {
        return Json(json!({"failover_state": "local"}));
    }
    let state_str = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| "local".to_string()),
            // A well-formed-but-non-object body → "local", matching the Python
            // `data.get(...) if isinstance(data, dict) else "local"`.
            Ok(_) => "local".to_string(),
            Err(_) => return Json(json!({"failover_state": "local"})),
        },
        Err(_) => return Json(json!({"failover_state": "local"})),
    };
    let validated = if FAILOVER_STATES.contains(&state_str.as_str()) {
        state_str
    } else {
        "local".to_string()
    };
    Json(json!({"failover_state": validated}))
}

/// The store's most-recent failover state, validated to the accepted set, or
/// `None` when the store is unreachable / has no such event / carries an
/// unrecognized value, so the route falls back to the sidecar. Mirrors the Python
/// `latest_wfb_failover`.
async fn latest_wfb_failover(state: &AppState) -> Option<String> {
    let rows = logd_query_events(state, "wfb.pair.failover", 1).await?;
    let row = rows.first()?.as_object()?;
    let detail = row.get("detail")?.as_object()?;
    let s = detail.get("state")?.as_str()?;
    if FAILOVER_STATES.contains(&s) {
        Some(s.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// logd query seam: HTTP-over-UDS reads of the store's /v1 API.
// ---------------------------------------------------------------------------

/// Query the store for the newest `events` rows of one `event_kind`. Returns the
/// `data` array, or `None` when the store is unreachable / the response is an
/// error / does not parse. Mirrors the Python `query_rows("events", limit,
/// event_kind=...)`.
async fn logd_query_events(state: &AppState, event_kind: &str, limit: i64) -> Option<Vec<Value>> {
    let params = [
        ("kind", "events".to_string()),
        ("limit", limit.to_string()),
        ("event_kind", event_kind.to_string()),
    ];
    let query = encode_query(&params);
    let path = format!("/v1/query?{query}");
    let (status, body) = logd_get(state, &path).await.ok()?;
    if status >= 400 {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&body).ok()?;
    parsed
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.to_vec())
}

/// A minimal HTTP/1.1 `GET` over the logging-store query Unix socket, returning
/// the status code + the decoded body. The socket path comes from the app state's
/// logd client so a test redirects it. `Connection: close` reads the body to EOF;
/// a chunked body is de-chunked. Bounded so a runaway response cannot exhaust
/// memory. Mirrors the read side of the Python `query_rows` httpx-over-UDS call.
async fn logd_get(state: &AppState, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A hard ceiling on the response read; a normal events/aggregate page is a
    /// few KiB, so this only guards a runaway body.
    const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

    let socket = state.logd.socket_path();
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let head = format!("GET {path} HTTP/1.1\r\nHost: logd\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break; // EOF (Connection: close).
        }
        if raw.len() + n > MAX_READ_BYTES {
            return Err(std::io::Error::other("logd response too large"));
        }
        raw.extend_from_slice(&buf[..n]);
    }
    parse_http_response(&raw)
}

/// Split a raw HTTP/1.1 response into the status code + decoded body. De-chunks a
/// `Transfer-Encoding: chunked` body; otherwise returns the body after the header
/// terminator as-is.
fn parse_http_response(raw: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let sep = b"\r\n\r\n";
    let split = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| std::io::Error::other("malformed http response (no header terminator)"))?;
    let head = &raw[..split];
    let body = &raw[split + sep.len()..];

    let head_str = String::from_utf8_lossy(head);
    let status = head_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("malformed http status line"))?;

    let chunked = head_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        de_chunk(body)
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// De-chunk a `Transfer-Encoding: chunked` body: `<hexlen>\r\n<data>\r\n`
/// repeated until a zero-length chunk.
fn de_chunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let len_line = &rest[..crlf];
        let len = usize::from_str_radix(String::from_utf8_lossy(len_line).trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = crlf + 2;
        if rest.len() < data_start + len {
            out.extend_from_slice(&rest[data_start..]);
            break;
        }
        out.extend_from_slice(&rest[data_start..data_start + len]);
        let next = data_start + len;
        rest = if rest.len() >= next + 2 {
            &rest[next + 2..]
        } else {
            &[]
        };
    }
    out
}

// ---------------------------------------------------------------------------
// Small shared helpers.
// ---------------------------------------------------------------------------

/// Percent-encode a query-parameter list into a `key=value&...` string. Only the
/// characters the store's query values use (`-`, digits, letters, `s`) appear, so
/// a conservative reserved-character escape is sufficient; encode the reserved
/// `&`, `=`, `%`, `+`, and space to be safe.
fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Conservative percent-encoding for the query helper: pass through the
/// unreserved set (`A-Za-z0-9-._~`) verbatim and percent-encode every other byte.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Coerce a JSON number value to `i64`, accepting an integer or a float (the
/// channel field may arrive as either). `None` for a non-number. Mirrors the
/// Python `int(merged.get("channel") or 0)` over a numeric value.
fn json_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Python `bool(x)` truthiness over a JSON value, for the `auto_pair_enabled`
/// coercion: `null`/`false`/`0`/`0.0`/`""`/`[]`/`{}` are falsey, everything else
/// truthy. Mirrors `bool(wfb_section.get("auto_pair_enabled", True))`.
fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Round to three decimal places, matching the Python `round(x, 3)` the
/// `bitrate_mbps` shim uses.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// The current wall-clock time in microseconds since the Unix epoch, for the
/// event-age staleness check (mirrors the Python `time.time() * 1_000_000`).
fn now_unix_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

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

    /// The exact base block the Python `_base_block` builds for an all-default
    /// config (no `video.wfb` fields, regulatory domain forced to a known value).
    /// Re-asserting the live `regulatory_domain` is stubbed out by overwriting it
    /// after the build, so the test does not depend on `iw` being present.
    fn base_block_fixed_reg(cfg: &StatusConfig, reg: &str) -> Map<String, Value> {
        let mut b = base_block(cfg);
        b.insert("regulatory_domain".to_string(), json!(reg));
        b
    }

    #[test]
    fn base_block_is_the_zero_default_for_an_empty_config() {
        let cfg = StatusConfig::default();
        let b = base_block_fixed_reg(&cfg, "unknown");
        // The exact 27-field zero-default block the FastAPI `_base_block` returns.
        let want = json!({
            "state": "disabled",
            "interface": "",
            "channel": 0,
            "frequency_mhz": 0,
            "bandwidth_mhz": 0,
            "adapter": {"driver": "", "chipset": "", "supports_monitor": false},
            "adapter_chipset": null,
            "adapter_injection_ok": false,
            "rssi_dbm": -100.0,
            "noise_dbm": -95.0,
            "snr_db": 0.0,
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
        let cfg = StatusConfig {
            video: VideoSection {
                wfb: WfbConfigSection {
                    channel: Some(149),
                    tx_power_dbm: Some(json!(10)),
                    tx_power_max_dbm: Some(json!(15)),
                    topology: Some(json!("one-to-one")),
                    mcs_index: Some(json!(2)),
                },
            },
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
        // A merged body on channel 149 with 5000 kbps must re-derive 5745/20 and a
        // 5.0 mbps shim. Mirrors the FastAPI `_finalize_status`.
        let mut merged = Map::new();
        merged.insert("channel".to_string(), json!(149));
        merged.insert("bitrate_kbps".to_string(), json!(5000));
        let out = finalize_status(merged);
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
        let out = finalize_status(merged);
        assert_eq!(out["frequency_mhz"], json!(2442));
        assert_eq!(out["bandwidth_mhz"], json!(40));
        assert_eq!(out["bitrate_mbps"], json!(0.0));
    }

    #[test]
    fn derive_merges_body_over_base_and_keeps_live_reg_domain() {
        // The stored body carries link values + a `reg_domain` key (NOT
        // `regulatory_domain`), so the base's live `regulatory_domain` survives.
        let cfg = StatusConfig::default();
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
        let cfg = StatusConfig::default();
        let mut detail = Map::new();
        detail.insert("state".to_string(), json!("active"));
        detail.insert("channel".to_string(), json!(36));
        // An event 20 s old (> the 10 s threshold) flips to "stale".
        let ts_us = now_unix_micros() - 20_000_000;
        let out = derive_wfb_status(&detail, ts_us, &cfg);
        assert_eq!(out["state"], json!("stale"));
    }

    #[test]
    fn fingerprint_is_16_hex_of_blake2b_8_over_the_public_half() {
        // A 64-byte key file: first 32 bytes the secret half, second 32 the public
        // half. The fingerprint is blake2b-8 of the public half, 16 lowercase hex.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx.key");
        let mut bytes = vec![0u8; 64];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        std::fs::write(&path, &bytes).unwrap();

        // Compute the expected value the same way, independently.
        let expected = {
            use blake2::digest::{Update, VariableOutput};
            use blake2::Blake2bVar;
            let mut h = Blake2bVar::new(8).unwrap();
            h.update(&bytes[32..]);
            let mut out = [0u8; 8];
            h.finalize_variable(&mut out).unwrap();
            hex::encode(out)
        };
        let got = read_public_fingerprint(&path).unwrap();
        assert_eq!(got, expected);
        assert_eq!(got.len(), 16);
        assert!(got.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprint_rejects_a_wrong_size_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rx.key");
        std::fs::write(&path, vec![0u8; 32]).unwrap(); // half-size
        assert_eq!(read_public_fingerprint(&path), None);
        assert_eq!(read_public_fingerprint(&dir.path().join("absent.key")), None);
    }

    #[test]
    fn config_truthiness_matches_python_bool() {
        assert!(!json_truthy(&Value::Null));
        assert!(!json_truthy(&json!(false)));
        assert!(json_truthy(&json!(true)));
        assert!(!json_truthy(&json!(0)));
        assert!(json_truthy(&json!(1)));
        assert!(!json_truthy(&json!("")));
        assert!(json_truthy(&json!("x")));
        assert!(!json_truthy(&json!([])));
        assert!(!json_truthy(&json!({})));
    }

    #[test]
    fn failover_validation_accepts_the_known_set_and_rejects_others() {
        assert!(FAILOVER_STATES.contains(&"local"));
        assert!(FAILOVER_STATES.contains(&"cloud_relay"));
        assert!(FAILOVER_STATES.contains(&"failed"));
        assert!(!FAILOVER_STATES.contains(&"bogus"));
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
        // `except: return base` path returns the bare base, no finalize). Point
        // the run dir at an empty tempdir so the file is absent.
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let cfg = StatusConfig::default();
        let out = build_status_from_stats_file(&cfg);
        // The bare base carries the 27 keys with no `bitrate_mbps` shim (the
        // FastAPI absent-file path skips finalize).
        assert_eq!(out["state"], json!("disabled"));
        assert_eq!(out["channel"], json!(0));
        assert!(out.get("bitrate_mbps").is_none());
        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[test]
    fn pair_status_of_an_unpaired_drone_is_the_default_snapshot() {
        // No key file, an empty config: paired false, every config field null,
        // auto-pair defaults true, role drone. The golden shape the GCS pairing
        // card reads.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.yaml");
        std::fs::write(&cfg_path, "agent:\n  profile: drone\n").unwrap();
        let raw = load_config_value(&cfg_path);
        // Drive the pieces the handler composes, without the AppState wiring.
        let (_profile, role) = current_role("drone");
        assert_eq!(role, "drone");
        let key_path = dir.path().join("wfb").join("tx.key");
        let paired = std::fs::metadata(&key_path)
            .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES as u64)
            .unwrap_or(false);
        assert!(!paired);
        let wfb_section = raw
            .get("video")
            .filter(|v| v.is_object())
            .and_then(|v| v.get("wfb"))
            .filter(|v| v.is_object());
        let auto_pair = wfb_section
            .and_then(|w| w.get("auto_pair_enabled"))
            .map(json_truthy)
            .unwrap_or(true);
        assert!(auto_pair); // absent → defaults true

        let snapshot = json!({
            "paired": paired,
            "paired_with_device_id": Value::Null,
            "paired_at": Value::Null,
            "fingerprint": Value::Null,
            "auto_pair_enabled": auto_pair,
            "role": role,
        });
        let want = json!({
            "paired": false,
            "paired_with_device_id": null,
            "paired_at": null,
            "fingerprint": null,
            "auto_pair_enabled": true,
            "role": "drone",
        });
        assert_eq!(snapshot, want);
    }

    #[test]
    fn de_chunk_reassembles_a_chunked_body() {
        let chunked = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(de_chunk(chunked), b"hello world");
    }

    #[test]
    fn parse_http_response_reads_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{}");
    }

    #[test]
    fn percent_encode_escapes_reserved_chars() {
        // The unreserved set passes through; the space + `-` cases the query uses.
        assert_eq!(percent_encode("link.rssi_dbm"), "link.rssi_dbm");
        assert_eq!(percent_encode("-60s"), "-60s");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("k=v"), "k%3Dv");
    }
}
