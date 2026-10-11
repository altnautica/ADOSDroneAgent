// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Altnautica — ADOS Drone Agent
//! The one WFB link-status derivation, and the radio block built from it.
//!
//! ## Why this lives here
//!
//! Three copies of this derivation existed. `ados-control`'s `/api/wfb` route and
//! its `/api/status/full` route each carried a private `derive_wfb_status` +
//! `build_status_from_stats_file` + base block + finalize legs, near-identical and
//! free to drift; the cloud heartbeat carried no derivation at all and shipped an
//! `absent` radio block forever, because writing a fourth was the only way to get
//! one. A same-name fork is the acute form of the reader/producer defect, and it
//! has already cost this codebase a shipped regression, so the derivation
//! is implemented once, here, and consumed by every transport.
//!
//! `ados-protocol` is the right home: it already owns the frozen wire seams both
//! `ados-control` and `ados-cloud` depend on, and neither can depend on the other.
//!
//! ## Shape
//!
//! - [`WfbStatusConfig`] is the `video.wfb` slice the base block seeds from.
//! - [`wfb_base_block`] is the config-seeded zero default both read paths merge
//!   the producer's payload over.
//! - [`derive_wfb_status`] maps a stored `link.wfb_status` event body onto it;
//!   [`build_status_from_stats_file_at`] does the same from the `wfb-stats.json`
//!   sidecar. Both end in [`finalize_wfb_status`].
//! - [`build_radio_block`] projects that status onto the 40-field heartbeat radio
//!   block, and [`radio_absent_block`] is its honest no-radio skeleton.
//!
//! Every file read is path-injectable so a caller's test drives a tempdir without
//! touching the process-global run dir. The one impure edge that is not injected
//! is [`regulatory_domain`], and it is read by a background task rather than on
//! the caller's path: every caller here is on the tokio reactor (an axum route
//! handler, the cloud heartbeat tick), and `iw reg get` on a wedged RTL driver
//! does not return, so forking it inline parked a reactor worker indefinitely.

use std::path::Path;

use serde_json::{json, Map, Value};

mod radio_block;
mod reg_domain;

pub use radio_block::{build_radio_block, radio_absent_block, RSSI_SENTINEL};
#[cfg(test)]
use reg_domain::*;
pub use reg_domain::{refresh_regulatory_domain, regulatory_domain};

// ---------------------------------------------------------------------------
// Channels.
// ---------------------------------------------------------------------------

/// One standard WFB-ng channel: the number, its centre frequency, and its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WfbChannel {
    pub channel_number: i64,
    pub frequency_mhz: i64,
    pub bandwidth_mhz: i64,
}

/// The standard 5 GHz channels usable with WFB-ng on the RTL8812 family: the
/// U-NII-1 sub-band (36/40/44/48) and the U-NII-3 sub-band (149/153/157/161/165),
/// each 20 MHz wide.
///
/// This is the ONE table. The radio block used to carry a second, narrower copy
/// that omitted 40 and 44 — so a link on either channel rendered a null
/// `freq_mhz` in the GCS while the status body beside it reported the correct
/// `frequency_mhz`. The narrower copy was justified in a comment as matching a
/// Python map that no longer exists.
pub const STANDARD_CHANNELS: [WfbChannel; 9] = [
    WfbChannel {
        channel_number: 36,
        frequency_mhz: 5180,
        bandwidth_mhz: 20,
    },
    WfbChannel {
        channel_number: 40,
        frequency_mhz: 5200,
        bandwidth_mhz: 20,
    },
    WfbChannel {
        channel_number: 44,
        frequency_mhz: 5220,
        bandwidth_mhz: 20,
    },
    WfbChannel {
        channel_number: 48,
        frequency_mhz: 5240,
        bandwidth_mhz: 20,
    },
    WfbChannel {
        channel_number: 149,
        frequency_mhz: 5745,
        bandwidth_mhz: 20,
    },
    WfbChannel {
        channel_number: 153,
        frequency_mhz: 5765,
        bandwidth_mhz: 20,
    },
    WfbChannel {
        channel_number: 157,
        frequency_mhz: 5785,
        bandwidth_mhz: 20,
    },
    WfbChannel {
        channel_number: 161,
        frequency_mhz: 5805,
        bandwidth_mhz: 20,
    },
    WfbChannel {
        channel_number: 165,
        frequency_mhz: 5825,
        bandwidth_mhz: 20,
    },
];

/// Look up a channel by number, or `None` for an unknown number.
pub fn get_channel(channel_number: i64) -> Option<WfbChannel> {
    STANDARD_CHANNELS
        .iter()
        .find(|c| c.channel_number == channel_number)
        .copied()
}

// ---------------------------------------------------------------------------
// Config seam.
// ---------------------------------------------------------------------------

/// The `video.wfb` config slice the WFB status base block seeds from. These are
/// configured values, not measurements: an absent file, section or key reads
/// the schema default the radio itself runs at (channel 149, 5/15 dBm,
/// `host_vbus`, MCS 1), so the base block never reports a channel or power the
/// radio is not configured for.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WfbStatusConfig {
    pub channel: i64,
    pub tx_power_dbm: Value,
    pub tx_power_max_dbm: Value,
    pub topology: Value,
    pub mcs_index: Value,
}

impl Default for WfbStatusConfig {
    fn default() -> Self {
        WfbStatusConfig {
            channel: 149,
            tx_power_dbm: json!(5),
            tx_power_max_dbm: json!(15),
            topology: json!("host_vbus"),
            mcs_index: json!(1),
        }
    }
}

impl WfbStatusConfig {
    /// Load the `video.wfb` slice from a config path, defaulting every field when
    /// the file / section / key is absent or unparseable. Takes the path rather
    /// than resolving it, because the two callers resolve it differently (the
    /// front from its pairing paths, the cloud relay from `ADOS_CONFIG`).
    pub fn load(config_path: &Path) -> Self {
        let text = match std::fs::read_to_string(config_path) {
            Ok(t) => t,
            Err(_) => return WfbStatusConfig::default(),
        };
        let root: Value = match serde_norway::from_str(&text) {
            Ok(v) => v,
            Err(_) => return WfbStatusConfig::default(),
        };
        let wfb = root
            .get("video")
            .filter(|v| v.is_object())
            .and_then(|v| v.get("wfb"))
            .filter(|v| v.is_object());
        let Some(wfb) = wfb else {
            return WfbStatusConfig::default();
        };
        let d = WfbStatusConfig::default();
        WfbStatusConfig {
            channel: wfb
                .get("channel")
                .and_then(json_to_i64)
                .unwrap_or(d.channel),
            tx_power_dbm: wfb.get("tx_power_dbm").cloned().unwrap_or(d.tx_power_dbm),
            tx_power_max_dbm: wfb
                .get("tx_power_max_dbm")
                .cloned()
                .unwrap_or(d.tx_power_max_dbm),
            topology: wfb.get("topology").cloned().unwrap_or(d.topology),
            mcs_index: wfb.get("mcs_index").cloned().unwrap_or(d.mcs_index),
        }
    }
}

// ---------------------------------------------------------------------------
// The status derivation.
// ---------------------------------------------------------------------------

/// Beyond this age (microseconds) a stored status event is treated as stale,
/// mirroring the sidecar path's `mtime > 10 s` flip.
pub const WFB_STALE_AGE_US: i64 = 10_000_000;

/// The staleness ceiling in seconds, the same number [`WFB_STALE_AGE_US`] carries.
/// A snapshot older than this renders a dead link as if it were live, so every
/// transport gates on it.
pub const WFB_STALE_AGE_S: f64 = 10.0;

/// The config-seeded zero-default WFB status block both read paths merge over.
/// `regulatory_domain` is the live (TTL-cached) `iw reg get` value.
pub fn wfb_base_block(cfg: &WfbStatusConfig) -> Map<String, Value> {
    let mut block = Map::new();
    block.insert("state".to_string(), json!("disabled"));
    block.insert("interface".to_string(), json!(""));
    // `channel` is the configured home channel; `actual_channel` is the live
    // interface channel, null until a producer reports one.
    block.insert("channel".to_string(), json!(cfg.channel));
    block.insert("actual_channel".to_string(), Value::Null);
    block.insert("frequency_mhz".to_string(), json!(0));
    block.insert("bandwidth_mhz".to_string(), json!(0));
    block.insert(
        "adapter".to_string(),
        json!({"driver": "", "chipset": "", "supports_monitor": false}),
    );
    block.insert("adapter_chipset".to_string(), Value::Null);
    // Null, not false: this base is what `build_radio_block` receives when no
    // sidecar / stored status exists — i.e. no adapter scan ever produced a
    // verdict. A false here would be a real boolean `bool_or_null` forwards
    // verbatim, resurrecting the fabricated measured no-injection claim the radio
    // block's null contract exists to prevent. A real sidecar always carries the
    // key, so a genuine verdict overwrites this on merge.
    block.insert("adapter_injection_ok".to_string(), Value::Null);
    // Null, not a plausible reading. This base is what a node with NO radio
    // serves: no adapter, no producer, nothing ever sampled. A `-100.0` RSSI
    // beside a `-95.0` noise floor and a `0.0` SNR is a complete, credible weak
    // link, and it reaches the GCS signal meter, the durable link series and the
    // operator as a measurement. The sentinel-to-null fold in
    // `build_radio_block` only covered `rssi_dbm`, so the noise floor and SNR
    // rode the heartbeat verbatim.
    block.insert("rssi_dbm".to_string(), Value::Null);
    block.insert("noise_dbm".to_string(), Value::Null);
    block.insert("snr_db".to_string(), Value::Null);
    block.insert("packets_received".to_string(), json!(0));
    block.insert("packets_lost".to_string(), json!(0));
    block.insert("loss_percent".to_string(), json!(0.0));
    block.insert("fec_recovered".to_string(), json!(0));
    block.insert("fec_failed".to_string(), json!(0));
    block.insert("bitrate_kbps".to_string(), json!(0));
    block.insert("rx_silent_seconds".to_string(), Value::Null);
    block.insert("restart_count".to_string(), json!(0));
    block.insert("samples".to_string(), json!(0));
    block.insert("tx_power_dbm".to_string(), cfg.tx_power_dbm.clone());
    block.insert("tx_power_max_dbm".to_string(), cfg.tx_power_max_dbm.clone());
    block.insert("tx_power_readback_dbm".to_string(), Value::Null);
    block.insert("topology".to_string(), cfg.topology.clone());
    block.insert("mcs_index".to_string(), cfg.mcs_index.clone());
    block.insert("regulatory_domain".to_string(), json!(regulatory_domain()));
    block
}

/// Map a stored `link.wfb_status` event body onto the status shape: the
/// config-seeded base, the body merged over it, an event-age staleness flip, then
/// the shared finalize legs.
///
/// The base `regulatory_domain` (the live reading) stays put: the stored body
/// carries `reg_domain`, a different key, so the merge never overwrites it.
pub fn derive_wfb_status(detail: &Map<String, Value>, ts_us: i64, cfg: &WfbStatusConfig) -> Value {
    let mut merged = wfb_base_block(cfg);
    for (k, v) in detail {
        merged.insert(k.clone(), v.clone());
    }
    let now_us = now_unix_micros();
    if ts_us > 0 && now_us - ts_us > WFB_STALE_AGE_US {
        merged.insert("state".to_string(), json!("stale"));
    }
    finalize_wfb_status(merged)
}

/// Compose a status body from a `wfb-stats.json` sidecar at `path`: merge the file
/// payload over the config-seeded base, flip `state` to `"stale"` when the file is
/// older than [`WFB_STALE_AGE_S`], re-assert the live regulatory domain, and
/// finalize.
///
/// An absent / unreadable / unparseable / non-object body degrades to the bare
/// base block (no finalize legs) — the shape a caller with no producer must see.
pub fn build_status_from_stats_file_at(cfg: &WfbStatusConfig, path: &Path) -> Value {
    let base = wfb_base_block(cfg);

    // The mtime drives the staleness flip; compute the file age in seconds.
    let age_s = match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime.elapsed().map(|d| d.as_secs_f64()).unwrap_or(0.0),
        Err(_) => return Value::Object(base),
    };

    let payload = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Value::Object(base),
            Err(_) => return Value::Object(base),
        },
        Err(_) => return Value::Object(base),
    };

    // Best-effort schema-drift signal (never reject): warn when the sidecar was
    // written by an agent with a different schema version, then read anyway. The
    // writer const lives in the radio crate, so compare against the registry.
    let got = payload.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = crate::contracts::sidecar_version("wfb-stats") {
        crate::sidecar::check_sidecar_version("wfb-stats", got, ours);
    }

    let mut merged = base;
    for (k, v) in payload {
        merged.insert(k, v);
    }
    if age_s > WFB_STALE_AGE_S {
        merged.insert("state".to_string(), json!("stale"));
    }
    // Re-assert the live regulatory domain over the payload (the file's body must
    // not overwrite it).
    merged.insert("regulatory_domain".to_string(), json!(regulatory_domain()));
    finalize_wfb_status(merged)
}

/// Re-derive `frequency_mhz` / `bandwidth_mhz` from the live channel and add the
/// `bitrate_mbps` shim, on top of a base+payload merge.
pub fn finalize_wfb_status(mut merged: Map<String, Value>) -> Value {
    let channel = merged
        .get("actual_channel")
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

// ---------------------------------------------------------------------------
// JSON helpers shared by the derivation and its callers.
// ---------------------------------------------------------------------------

/// Look up `key` and return its value, or JSON `null` when absent.
pub fn get_or_null(map: &Map<String, Value>, key: &str) -> Value {
    map.get(key).cloned().unwrap_or(Value::Null)
}

/// A non-empty owned string for a JSON string value, or `None` for a non-string /
/// empty string.
pub fn non_empty_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Python `bool(x)` over a JSON value: `null`/`false`/`0`/`0.0`/`""`/`[]`/`{}` are
/// falsey, everything else truthy.
pub fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The `x or None` falsiness used for `channel` / `bitrate_kbps` (a `0` / `0.0` /
/// `null` reads as no value → `null`).
pub fn is_falsey(v: &Value) -> bool {
    !json_truthy(v)
}

/// Coerce a JSON number to `i64`, accepting an integer or a float. `None` for a
/// non-number.
pub fn json_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Round to three decimal places.
pub fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// The current wall-clock time in microseconds since the Unix epoch.
pub fn now_unix_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
