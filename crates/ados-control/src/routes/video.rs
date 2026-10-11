//! Video pipeline read routes: latency and the encoder/radio config snapshot.
//!
//! Two read-only routes the GCS Video Link panel + the LCD page poll:
//!
//! - **`GET /api/video/latency`** — the most-recent SEI-probe latency, or an
//!   explicit statement that there is no current one. Reads the durable store
//!   first (the store's sidecar tailer samples the same `lcd-latency.json` the
//!   local tap writes into a `video.latency.*` metric series + a
//!   `video.latency_source` event), falling back to a live read of
//!   `lcd-latency.json` when the store is unreachable or the probe has produced
//!   nothing. BOTH paths are freshness-gated on the same window: a sample or a
//!   file older than it reports `{"latency_ms": null, "source": "stale"}` with
//!   the age, never the last number the probe happened to produce. An absent
//!   file reports `{"latency_ms": null, "source": "unavailable"}`.
//! - **`GET /api/video/config`** — the composite encoder + radio config snapshot.
//!   The static radio/encoder blocks come from `/etc/ados/config.yaml`; the dynamic
//!   `adaptive` / `hopping` / `link` blocks come from the sidecars the wfb-side
//!   services persist under the runtime dir (`wfb-stats.json` for both `adaptive`
//!   and `link`, `hop-supervisor.json` for `hopping`), defaulting to the
//!   config-seeded stub when a sidecar is absent.
//!
//! Every read is fault-tolerant: an absent store / sidecar / config degrades to the
//! same empty/default shape the route always returns when its source is
//! unavailable, never a 500. The config write (`POST /api/video/config`) lives in
//! `video_config_write.rs` and answers with this module's config body.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use ados_protocol::paths;
use ados_protocol::sidecar::{file_age, read_fresh_at, Reading};

use crate::state::AppState;

mod config;
mod latency;
mod metrics;
pub(crate) use config::*;
use latency::*;
use metrics::*;

// ---------------------------------------------------------------------------
// Runtime-dir + config seam paths.
// ---------------------------------------------------------------------------

/// The SEI-latency state file (`/run/ados/lcd-latency.json`), written by the
/// local tap when the SEI latency probe is enabled.
fn lcd_latency_path() -> PathBuf {
    paths::run_path("lcd-latency")
}

/// Compose the config route's `adaptive` block: `available` from config, with the
/// live controller state merged over it from the wfb stats sidecar.
///
/// The live half is sourced from `wfb-stats.json`, which the radio rewrites on
/// every tick. It used to be read from `bitrate-controller.json` -- a file with
/// NO writer, because its only producer was a Python `BitrateController` that is
/// never instantiated anywhere. The merge could therefore never fire and this
/// block was permanently `{available}` alone. The on-box dashboard reads
/// `adaptive.adaptive_bitrate_enabled` and rendered blank forever as a result,
/// which looked like a stale key name and was really a reader pointed at a dead
/// file: the live sidecar publishes that exact key, alongside the rest of the
/// controller's state.
///
/// `stats` is `None` for BOTH an absent sidecar and a stale one — see
/// [`read_stats_if_fresh`].
fn build_adaptive_block(available: bool, stats: Option<&Map<String, Value>>) -> Map<String, Value> {
    let mut adaptive = Map::new();
    adaptive.insert("available".to_string(), json!(available));
    if let Some(snap) = stats {
        for key in ADAPTIVE_KEYS {
            if let Some(v) = snap.get(*key) {
                adaptive.insert((*key).to_string(), v.clone());
            }
        }
    }
    adaptive
}

/// Read the wfb stats sidecar only while it is fresh enough to describe the radio
/// NOW, applying the same [`LINK_STALE_AFTER_S`] ceiling the `link` block uses on
/// this same file.
///
/// Absent and stale both return `None`. A stale snapshot's controller state
/// describes a radio that may be dead: a frozen `adaptive_bitrate_enabled: true`
/// beside a frozen recommended rung reads as a live adapting link, which is the
/// healthy-looking dead link an honest surface forbids — the same reasoning the `link`
/// block already documents for its own counters.
fn read_stats_if_fresh(path: &Path) -> Option<Map<String, Value>> {
    read_fresh_at(path, Some(secs(LINK_STALE_AFTER_S)), SystemTime::now()).fresh()
}

/// The `adaptive` block's live fields, read out of `wfb-stats.json`. The radio
/// writes all of these on every tick; `available` is seeded from config
/// separately and is not in this list.
const ADAPTIVE_KEYS: &[&str] = &[
    "adaptive_bitrate_enabled",
    "recommended_bitrate_kbps",
    "encoder_bitrate_kbps",
    "recommended_tier_idx",
    "link_preset",
    "mcs_index",
    "mcs_ladder_cap",
    "fec_k",
    "fec_n",
];

/// The hop-supervisor snapshot file (`/run/ados/hop-supervisor.json`), persisted
/// by the frequency hopper.
fn hop_supervisor_path() -> PathBuf {
    paths::run_path("hop-supervisor")
}

/// The live wfb stats sidecar (`/run/ados/wfb-stats.json`), the link-liveness
/// source the config route's `link` block reads.
fn wfb_stats_path() -> PathBuf {
    paths::run_path("wfb-stats")
}

/// Read a JSON snapshot file written by a sidecar producer, returning the parsed
/// object, or `None` on any read / parse failure or a non-object body.
fn read_state_file(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

// ===========================================================================
// GET /api/video/latency
// ===========================================================================

/// `GET /api/video/latency` → the most-recent SEI-probe latency, or an explicit
/// statement that there is no current one.
///
/// Reads the store first; falls back to the live `lcd-latency.json` read when the
/// store is unreachable, the SEI probe has produced no samples, or every sample
/// in the store is older than [`LATENCY_STALE_AFTER_S`]. Guaranteed 200.
pub async fn get_video_latency(State(state): State<AppState>) -> Json<Value> {
    if let Some(derived) = latest_video_latency(&state).await {
        return Json(derived);
    }
    Json(read_latency_live())
}

// ===========================================================================
// GET /api/video/config
// ===========================================================================

/// `GET /api/video/config` → the composite encoder + radio + adaptive + hopping +
/// link snapshot.
///
/// The static `radio` / `encoder` blocks are projected from `/etc/ados/config.yaml`
/// (the loaded config object's `video.wfb` + `video.camera` slices, with the
/// Pydantic field defaults applied for any absent field). The dynamic `adaptive` /
/// `hopping` / `link` blocks come from the controller sidecar files; an absent
/// sidecar degrades each to the config-seeded stub. Guaranteed 200.
pub async fn get_video_config() -> Json<Value> {
    Json(video_config_body(&VideoConfig::load()))
}

/// The `GET /api/video/config` body for a loaded config. The config write route
/// (`POST /api/video/config`) answers with this same body, re-read after it
/// persists, plus its `warnings`.
pub(crate) fn video_config_body(cfg: &VideoConfig) -> Value {
    let wfb = &cfg.video.wfb;
    let camera = &cfg.video.camera;

    let radio = json!({
        "channel": wfb.channel,
        "band": wfb.band,
        "mcs_index": wfb.mcs_index,
        "fec_k": wfb.fec_k,
        "fec_n": wfb.fec_n,
        "tx_power_dbm": wfb.tx_power_dbm,
        "preset": wfb.wfb_link_preset,
    });
    let encoder = json!({
        "bitrate_kbps": camera.bitrate_kbps,
        "width": camera.width,
        "height": camera.height,
        "fps": camera.fps,
        "codec": camera.codec,
    });

    // Gated on the same freshness ceiling the `link` block below applies to this
    // same file. A stale snapshot's controller state describes a radio that may
    // be dead now: a frozen `adaptive_bitrate_enabled: true` beside a frozen
    // recommended rung reads as a live adapting link, which is the
    // healthy-looking dead link an honest surface forbids. Absent and stale both degrade to
    // the config stub rather than to a last-known value.
    let adaptive = build_adaptive_block(
        wfb.adaptive_bitrate_enabled,
        read_stats_if_fresh(&wfb_stats_path()).as_ref(),
    );

    // hopping: the supervisor snapshot, or a config-seeded stub when absent.
    let hopping = match read_state_file(&hop_supervisor_path()) {
        Some(snap) => {
            // Best-effort schema-drift signal (never reject): warn on a
            // producer/reader version mismatch, then read anyway. The writer const
            // lives in the radio crate, so compare against the shared registry.
            let got = snap.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
            if let Some(ours) = ados_protocol::contracts::sidecar_version("hop-supervisor") {
                ados_protocol::sidecar::check_sidecar_version("hop-supervisor", got, ours);
            }
            Value::Object(snap)
        }
        None => json!({
            "enabled": wfb.auto_hop_enabled,
            "band": wfb.band,
            "hop_period_seconds": wfb.hop_period_seconds,
            "history": [],
            "last_hop_at": 0.0,
        }),
    };

    let link = link_snapshot(wfb.channel, &wfb_stats_path());

    json!({
        "radio": radio,
        "encoder": encoder,
        "adaptive": Value::Object(adaptive),
        "hopping": hopping,
        "link": link,
    })
}

/// Beyond this age the `wfb-stats.json` snapshot can no longer describe the link
/// NOW, so the received-side verdict reads `null`: the registry window for
/// `wfb-stats`, the same ceiling `/api/wfb` uses to flip its `state` to
/// `"stale"`.
const LINK_STALE_AFTER_S: f64 = paths::WFB_STATS_MAX_AGE_MS as f64 / 1000.0;

/// The `rf_unverified` verdict for the link block, or `null` when it cannot be
/// sourced honestly.
///
/// The radio owns this boolean (an advancing transmit counter with no confirmed
/// reception inside the proof grace window) and both sidecar writers carry it, so
/// this block forwards the authoritative value instead of leaving the panel to
/// re-derive it from the liveness counters beside it. It is the other half of
/// `channel_locked`, which sits in the same block: locked is true once a verified
/// return signal was heard, `rf_unverified` is true when the transmit counter
/// advances while none has been.
///
/// Null — never a confident `false` — when the key is absent or non-boolean (a
/// sidecar written before the field existed, or a garbled body) or when the
/// snapshot is older than [`LINK_STALE_AFTER_S`], because a reading that old
/// cannot say whether the radio is unverified NOW, and a stale `false` is exactly
/// the healthy-looking dead link this field exists to expose. The status radio
/// block applies the same rule, reaching it through the already-composed
/// `state == "stale"` flip; here the raw sidecar is read directly, so the file
/// mtime is the freshness source.
fn rf_unverified_field(status: &Map<String, Value>, fresh: bool) -> Value {
    if !fresh {
        return Value::Null;
    }
    match status.get("rf_unverified") {
        Some(Value::Bool(b)) => Value::Bool(*b),
        _ => Value::Null,
    }
}

/// The live radio-link liveness block the GCS Video Link panel reads from
/// `config.link.*`. The in-process wfb manager is absent on this native front, so
/// the values come from the `wfb-stats.json` sidecar the radio mirrors; `channel`
/// falls back to the configured value when the sidecar has no value yet. Every
/// field is present (a `null` placeholder when unknown) so the panel never sees a
/// missing key.
///
/// Alongside the liveness counters the block folds in the signals that EXPLAIN why
/// a counter it carries should not be believed — the derived `state`/`link_state`,
/// the muted-PHY + adapter-USB-health flags, and the `link_diag` verdict with its
/// backing RX counters — so a consumer sees the reading AND its cause on one
/// surface instead of re-deriving the verdict from a heuristic.
///
/// The whole snapshot is merged only when it is FRESH (mtime within
/// [`LINK_STALE_AFTER_S`]). A stale snapshot's numbers describe a link that may be
/// dead now, so every counter reads `null` (unknown) rather than a frozen
/// last-known value — a stale advancing `tx_bytes_per_s` or a stale
/// `channel_locked: true` is exactly the healthy-looking dead link operating rule
/// 44 forbids. `channel` is the exception: it carries the sidecar's live
/// `actual_channel` when fresh and otherwise falls back to the configured value
/// (always truthful) so the panel keeps a channel number.
fn link_snapshot(config_channel: i64, stats_path: &Path) -> Value {
    const FIELDS: [&str; 16] = [
        // Liveness counters.
        "tx_bytes_per_s",
        "valid_rx_packets_per_s",
        "video_inbound_bytes_per_s",
        "rx_silent_seconds",
        "channel_locked",
        "acquire_state",
        // Derived link state. The radio writes the same string under both keys
        // (`state` is the legacy name, `link_state` the state-machine name); a
        // consumer keys off whichever it knows. Folded in so the panel reads the
        // verdict beside the counters that back it, not from a separate surface.
        "state",
        "link_state",
        // PHY / adapter-USB health — the signals that explain why an advancing
        // `tx_bytes_per_s` should not be trusted: a muted PHY injects frames yet
        // radiates nothing, and a full-speed (12 Mbps) USB enumeration lets an RTL
        // adapter advance tx_bytes while emitting no usable RF.
        "phy_muted",
        "adapter_usb_speed_mbps",
        "adapter_usb_degraded",
        // Link-diagnostic verdict + its backing RX counters. `link_diag` is the
        // one-glance cause (deaf / mis_keyed / jammed / healthy / searching); the
        // counters separate the failure modes a bare "0 received" hides —
        // `packets_all` captured off-air before decrypt, `decrypt_errors`,
        // `packets_bad`, and valid `session_packets`.
        "link_diag",
        "packets_all",
        "decrypt_errors",
        "packets_bad",
        "session_packets",
    ];
    let mut link = Map::new();
    for f in FIELDS {
        link.insert(f.to_string(), Value::Null);
    }
    link.insert("channel".to_string(), Value::Null);
    // The received-side verdict is always present too, `null` until it can be
    // sourced honestly — it is gated on type and freshness rather than merged
    // verbatim, so it gets its own placeholder outside the loop above.
    link.insert("rf_unverified".to_string(), Value::Null);
    let reading = read_fresh_at::<Map<String, Value>>(
        stats_path,
        Some(secs(LINK_STALE_AFTER_S)),
        SystemTime::now(),
    );
    // A future mtime (a clock that stepped backwards) is stale, never fresh.
    let (status, fresh) = match reading {
        Reading::Fresh(s) => (Some(s), true),
        Reading::Stale(s) => (Some(s), false),
        Reading::Absent | Reading::Malformed(_) => (None, false),
    };
    if let Some(status) = status {
        // Best-effort schema-drift signal (never reject): warn on a producer/reader
        // version mismatch, then read anyway. The writer const lives in the radio
        // crate, so compare against the shared registry.
        let got = status.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
        if let Some(ours) = ados_protocol::contracts::sidecar_version("wfb-stats") {
            ados_protocol::sidecar::check_sidecar_version("wfb-stats", got, ours);
        }
        // Merge the sidecar's counters ONLY when the snapshot is fresh. A stale
        // snapshot describes a link that may be dead now, so every counter stays at
        // its `null` placeholder (unknown) rather than a frozen last-known value —
        // holding a stale advancing `tx_bytes_per_s` or a stale `channel_locked`
        // renders a dead link as if live. `channel` stays null
        // when stale too and falls back to the configured value below.
        if fresh {
            for f in FIELDS {
                if let Some(v) = status.get(f) {
                    if !v.is_null() {
                        link.insert(f.to_string(), v.clone());
                    }
                }
            }
            if let Some(ch) = status.get("actual_channel").filter(|v| !v.is_null()) {
                link.insert("channel".to_string(), ch.clone());
            }
        }
        link.insert(
            "rf_unverified".to_string(),
            rf_unverified_field(&status, fresh),
        );
    }
    // Channel falls back to the configured value so the panel always has a number
    // even before the first stats line lands.
    if link.get("channel").map(Value::is_null).unwrap_or(true) {
        link.insert("channel".to_string(), json!(config_channel));
    }
    Value::Object(link)
}

#[cfg(test)]
mod tests;
