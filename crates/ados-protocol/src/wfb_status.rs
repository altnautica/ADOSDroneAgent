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
//! is [`regulatory_domain`], which forks `iw reg get` behind a short TTL cache —
//! the domain changes only when something explicitly sets it, and a per-request
//! (or per-heartbeat-tick) fork is a cost with no reading behind it.

use std::path::Path;
use std::process::Command;

use serde_json::{json, Map, Value};

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

/// The `video.wfb` config slice the WFB status base block seeds from. Each field
/// is optional so an absent section reads the unset default: `channel` → `0`, the
/// rest → JSON `null`.
#[derive(Debug, Clone, Default)]
pub struct WfbStatusConfig {
    pub channel: i64,
    pub tx_power_dbm: Value,
    pub tx_power_max_dbm: Value,
    pub topology: Value,
    pub mcs_index: Value,
}

impl WfbStatusConfig {
    /// Load the `video.wfb` slice from a config path, defaulting every field when
    /// the file / section is absent or unparseable. Takes the path rather than
    /// resolving it, because the two callers resolve it differently (the front
    /// from its pairing paths, the cloud relay from `ADOS_CONFIG`).
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
        WfbStatusConfig {
            channel: wfb.get("channel").and_then(json_to_i64).unwrap_or(0),
            tx_power_dbm: wfb.get("tx_power_dbm").cloned().unwrap_or(Value::Null),
            tx_power_max_dbm: wfb.get("tx_power_max_dbm").cloned().unwrap_or(Value::Null),
            topology: wfb.get("topology").cloned().unwrap_or(Value::Null),
            mcs_index: wfb.get("mcs_index").cloned().unwrap_or(Value::Null),
        }
    }
}

// ---------------------------------------------------------------------------
// Regulatory domain: one `iw reg get` reading, held for a bounded window.
// ---------------------------------------------------------------------------

/// How long one `iw reg get` reading is reused.
///
/// The regulatory domain changes only when something explicitly sets it — never
/// on its own — while the GCS radio panel polls the status route about once a
/// second and the cloud heartbeat ticks every few seconds. Every one of those
/// used to fork `iw` twice (once to seed the base block, once to re-assert the
/// domain over the payload). Reusing a reading for this window collapses a poll
/// to at most one fork, and a real domain change still surfaces inside it. The
/// value stays the freshest thing in the body by a wide margin: the link figures
/// beside it come off a sidecar the route only calls stale after ten seconds.
const REG_DOMAIN_TTL: std::time::Duration = std::time::Duration::from_secs(3);

/// One string reading held for a bounded window.
///
/// What it serves is always a reading this process genuinely took; the window
/// bounds only how often the reading is refreshed, so the age of an answer is a
/// stated number rather than an unknown. Nothing is ever synthesised when the
/// underlying read fails — that failure has its own value (`"unknown"`) and is
/// cached like any other, so a wedged `iw` is not retried once per request either.
struct TimedCache {
    inner: std::sync::Mutex<Option<(std::time::Instant, String)>>,
}

impl TimedCache {
    const fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(None),
        }
    }

    /// The held reading while it is younger than `ttl`, else a fresh one.
    fn get_or_read(&self, ttl: std::time::Duration, read: impl FnOnce() -> String) -> String {
        self.get_or_read_at(std::time::Instant::now(), ttl, read)
    }

    /// The clock-injectable core of [`TimedCache::get_or_read`], so the reuse
    /// window is a unit under test instead of something a test has to sleep out.
    fn get_or_read_at(
        &self,
        now: std::time::Instant,
        ttl: std::time::Duration,
        read: impl FnOnce() -> String,
    ) -> String {
        // A poisoned lock means an earlier holder panicked mid-update. Fall
        // through to a fresh read rather than propagate the panic: the routes
        // behind this are required to answer.
        if let Ok(held) = self.inner.lock() {
            if let Some((at, value)) = held.as_ref() {
                if now.duration_since(*at) < ttl {
                    return value.clone();
                }
            }
        }
        // Deliberately outside the lock: the read forks a process, and holding
        // the mutex across it would queue every concurrent status request behind
        // one `iw` invocation.
        let fresh = read();
        if let Ok(mut held) = self.inner.lock() {
            *held = Some((now, fresh.clone()));
        }
        fresh
    }
}

/// The process-wide reading every status caller shares.
static REG_DOMAIN_CACHE: TimedCache = TimedCache::new();

/// The live regulatory domain, re-read at most once per [`REG_DOMAIN_TTL`].
pub fn regulatory_domain() -> String {
    REG_DOMAIN_CACHE.get_or_read(REG_DOMAIN_TTL, read_regulatory_domain)
}

/// Best-effort `iw reg get` first-line parse, returning the two-letter country
/// code, `"global"`, or `"unknown"` on any failure.
fn read_regulatory_domain() -> String {
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
// The status derivation.
// ---------------------------------------------------------------------------

/// Beyond this age (microseconds) a stored status event is treated as stale,
/// mirroring the sidecar path's `mtime > 10 s` flip.
pub const WFB_STALE_AGE_US: i64 = 10_000_000;

/// The staleness ceiling in seconds, the same number [`WFB_STALE_AGE_US`] carries.
/// A snapshot older than this renders a dead link as if it were live, so every
/// transport gates on it (operating rule 44).
pub const WFB_STALE_AGE_S: f64 = 10.0;

/// The config-seeded zero-default WFB status block both read paths merge over.
/// `regulatory_domain` is the live (TTL-cached) `iw reg get` value.
pub fn wfb_base_block(cfg: &WfbStatusConfig) -> Map<String, Value> {
    let mut block = Map::new();
    block.insert("state".to_string(), json!("disabled"));
    block.insert("interface".to_string(), json!(""));
    block.insert("channel".to_string(), json!(cfg.channel));
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
    block.insert("tx_power_dbm".to_string(), cfg.tx_power_dbm.clone());
    block.insert("tx_power_max_dbm".to_string(), cfg.tx_power_max_dbm.clone());
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

/// Re-derive `frequency_mhz` / `bandwidth_mhz` from the channel and add the
/// `bitrate_mbps` shim, on top of a base+payload merge.
pub fn finalize_wfb_status(mut merged: Map<String, Value>) -> Value {
    let channel = merged.get("channel").and_then(json_to_i64).unwrap_or(0);
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
// The radio block.
// ---------------------------------------------------------------------------

/// The RSSI sentinel the link-quality monitor seeds before the first real sample.
/// Treated as "no reading yet" so the radio block reports `null` for it.
pub const RSSI_SENTINEL: f64 = -100.0;

/// The `rf_unverified` verdict forwarded from the radio sidecar, or `null` when it
/// cannot be sourced honestly.
///
/// The radio service owns this boolean (an advancing transmit counter with no
/// confirmed reception inside the proof grace window); every other surface only
/// forwards it, so a consumer never has to re-derive it from a heuristic.
///
/// It reads `null` — never a confident `false` — when the key is absent (a
/// receive-side view, or a sidecar written before the field existed) or when the
/// snapshot went stale, because a reading older than the staleness ceiling cannot
/// say whether the radio is unverified NOW, and a stale `false` is exactly the
/// healthy-looking dead link this field exists to expose.
fn rf_unverified_field(status: &Map<String, Value>) -> Value {
    if status.get("state").and_then(Value::as_str) == Some("stale") {
        return Value::Null;
    }
    bool_or_null(status, "rf_unverified")
}

/// Forward a boolean verdict from the status view verbatim, or `null` when the
/// view carries no boolean reading for it.
///
/// The absence of a verdict must never collapse to a confident `false`: the GCS
/// resolves these fields three-state (degraded / ok / unknown), so a fabricated
/// `false` for `adapter_usb_degraded` renders a green "USB link OK" for an adapter
/// nothing ever enumerated, a fabricated `false` for `adapter_injection_ok`
/// renders a red no-injection claim a pre-scan rig never measured, and a
/// fabricated `false` for `phy_muted` asserts a healthy TX PHY on a view (the
/// receive side) that has no TX PHY to read.
fn bool_or_null(map: &Map<String, Value>, key: &str) -> Value {
    match map.get(key) {
        Some(Value::Bool(b)) => Value::Bool(*b),
        _ => Value::Null,
    }
}

/// Shape the forward-compatible `radio` heartbeat block from a WFB status view.
///
/// `wfb_status` is the status body (or `None` when no view is available). The GCS
/// keys off the presence of the block, not the values; an absent view yields the
/// [`radio_absent_block`] skeleton.
///
/// The emitted key set is exactly `ados_cloud::heartbeat::RadioBlock`'s 40 fields,
/// order for order, and `ados-cloud`'s `radio_block_from_sidecar` round-trip test
/// pins that: it deserializes this output into the struct and asserts the
/// re-serialization is byte-identical, so a field added here without a matching
/// struct field fails there rather than silently vanishing off the cloud wire.
pub fn build_radio_block(wfb_status: Option<&Map<String, Value>>) -> Value {
    let Some(status) = wfb_status else {
        return radio_absent_block();
    };

    let iface = status.get("interface").and_then(non_empty_string);
    let driver = iface
        .as_deref()
        .and_then(detect_radio_driver_name)
        .map(Value::from)
        .unwrap_or(Value::Null);
    let iface_value = iface.clone().map(Value::from).unwrap_or(Value::Null);

    let channel = status
        .get("channel")
        .filter(|v| !is_falsey(v))
        .cloned()
        .unwrap_or(Value::Null);
    let freq_mhz = channel
        .as_i64()
        .and_then(|c| get_channel(c).map(|ch| ch.frequency_mhz))
        .map(Value::from)
        .unwrap_or(Value::Null);

    let rssi = match status.get("rssi_dbm").and_then(Value::as_f64) {
        Some(v) if v == RSSI_SENTINEL => Value::Null,
        _ => status.get("rssi_dbm").cloned().unwrap_or(Value::Null),
    };
    let bitrate = status
        .get("bitrate_kbps")
        .filter(|v| !is_falsey(v))
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "state": status.get("state").cloned().unwrap_or(Value::Null),
        "iface": iface_value,
        "driver": driver,
        "channel": channel,
        "freq_mhz": freq_mhz,
        "bandwidth_mhz": 20,
        "tx_power_dbm": get_or_null(status, "tx_power_dbm"),
        "tx_power_max_dbm": get_or_null(status, "tx_power_max_dbm"),
        "topology": get_or_null(status, "topology"),
        "rssi_dbm": rssi,
        "snr_db": get_or_null(status, "snr_db"),
        "noise_dbm": get_or_null(status, "noise_dbm"),
        "bitrate_kbps": bitrate,
        "fec_recovered": get_or_null(status, "fec_recovered"),
        "fec_lost": get_or_null(status, "fec_failed"),
        "packets_lost": get_or_null(status, "packets_lost"),
        "loss_percent": get_or_null(status, "loss_percent"),
        "mcs_index": get_or_null(status, "mcs_index"),
        "rx_silent_seconds": get_or_null(status, "rx_silent_seconds"),
        "paired": json_truthy(status.get("paired").unwrap_or(&Value::Null)),
        "paired_with_device_id": get_or_null(status, "paired_with_device_id"),
        "paired_at": get_or_null(status, "paired_at"),
        "public_key_fingerprint": get_or_null(status, "public_key_fingerprint"),
        "auto_pair_enabled": get_or_null(status, "auto_pair_enabled"),
        "tx_video_stalled": get_or_null(status, "tx_video_stalled"),
        "tx_video_stall_kills": get_or_null(status, "tx_video_stall_kills"),
        "tx_video_recvq_bytes": get_or_null(status, "tx_video_recvq_bytes"),
        "acquire_state": get_or_null(status, "acquire_state"),
        "channel_locked": get_or_null(status, "channel_locked"),
        // The two halves of the received-side proof: `channel_locked` is true once
        // a verified return signal was heard, `rf_unverified` is true when the
        // transmit counter advances while none has been. Forwarded from the
        // radio's own verdict so a consumer reads the authoritative value instead
        // of re-deriving it.
        "rf_unverified": rf_unverified_field(status),
        "reacquire_kills": get_or_null(status, "reacquire_kills"),
        "valid_rx_packets_per_s": get_or_null(status, "valid_rx_packets_per_s"),
        "adapter_chipset": get_or_null(status, "adapter_chipset"),
        // Adapter + PHY verdicts forward verbatim as booleans, or `null` when the
        // view has no reading — never a fabricated false, which the GCS's
        // three-state resolvers would render as a measured green/red claim about
        // hardware this view never examined.
        "adapter_injection_ok": bool_or_null(status, "adapter_injection_ok"),
        "adapter_usb_speed_mbps": get_or_null(status, "adapter_usb_speed_mbps"),
        "adapter_usb_degraded": bool_or_null(status, "adapter_usb_degraded"),
        "phy_muted": bool_or_null(status, "phy_muted"),
        "tx_zombie_kills": get_or_null(status, "tx_zombie_kills"),
        "tx_bytes_per_s": get_or_null(status, "tx_bytes_per_s"),
        "restart_count": get_or_null(status, "restart_count"),
    })
}

/// The "no radio" skeleton every transport carries when there is no WFB status
/// view. Every metric is `null` and `paired` is `false`; the adapter / PHY
/// verdicts are `null` too — with no radio view there is nothing to have measured
/// them, and a `false` would claim a healthy USB link / unmuted PHY that was never
/// examined.
///
/// Three keys of the populated block are deliberately ABSENT rather than null
/// (`tx_zombie_kills`, `tx_bytes_per_s`, `restart_count`): they are the
/// `skip_serializing_if` fields on the cloud `RadioBlock`, and the frozen
/// heartbeat fixture pins the 37-key absent form exactly.
pub fn radio_absent_block() -> Value {
    json!({
        "state": "absent",
        "iface": null,
        "driver": null,
        "channel": null,
        "freq_mhz": null,
        "bandwidth_mhz": null,
        "tx_power_dbm": null,
        "tx_power_max_dbm": null,
        "topology": null,
        "rssi_dbm": null,
        "snr_db": null,
        "noise_dbm": null,
        "bitrate_kbps": null,
        "fec_recovered": null,
        "fec_lost": null,
        "packets_lost": null,
        "loss_percent": null,
        "mcs_index": null,
        "rx_silent_seconds": null,
        "paired": false,
        "paired_with_device_id": null,
        "paired_at": null,
        "public_key_fingerprint": null,
        "auto_pair_enabled": null,
        "tx_video_stalled": null,
        "tx_video_stall_kills": null,
        "tx_video_recvq_bytes": null,
        "acquire_state": null,
        "channel_locked": null,
        // Null, not false: with no radio view there is no verdict to report, and a
        // false here would claim a transmit path was proven.
        "rf_unverified": null,
        "reacquire_kills": null,
        "valid_rx_packets_per_s": null,
        "adapter_chipset": null,
        "adapter_injection_ok": null,
        "adapter_usb_speed_mbps": null,
        "adapter_usb_degraded": null,
        "phy_muted": null,
    })
}

/// Best-effort kernel driver name for the WFB monitor interface, read from
/// `/sys/class/net/<iface>/device/uevent`'s `DRIVER=` line. `None` for an empty
/// iface or an unreadable file.
fn detect_radio_driver_name(interface: &str) -> Option<String> {
    if interface.is_empty() {
        return None;
    }
    let path = Path::new("/sys/class/net")
        .join(interface)
        .join("device")
        .join("uevent");
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("DRIVER=") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
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
mod tests {
    use super::*;

    #[test]
    fn the_channel_table_covers_both_sub_bands_including_forty_and_forty_four() {
        // 40 and 44 are the regression: the radio block carried a second table
        // that omitted them, so a link on either rendered a null `freqMhz` in the
        // GCS beside a status body reporting the right frequency.
        assert_eq!(get_channel(36).unwrap().frequency_mhz, 5180);
        assert_eq!(get_channel(40).unwrap().frequency_mhz, 5200);
        assert_eq!(get_channel(44).unwrap().frequency_mhz, 5220);
        assert_eq!(get_channel(48).unwrap().frequency_mhz, 5240);
        assert_eq!(get_channel(165).unwrap().frequency_mhz, 5825);
        assert!(get_channel(1).is_none());
        for ch in STANDARD_CHANNELS {
            assert_eq!(ch.bandwidth_mhz, 20);
        }
    }

    #[test]
    fn the_radio_block_resolves_a_channel_40_frequency() {
        let mut status = Map::new();
        status.insert("channel".to_string(), json!(40));
        let block = build_radio_block(Some(&status));
        assert_eq!(block["freq_mhz"], json!(5200));
        let mut status44 = Map::new();
        status44.insert("channel".to_string(), json!(44));
        assert_eq!(build_radio_block(Some(&status44))["freq_mhz"], json!(5220));
    }

    #[test]
    fn the_absent_block_omits_exactly_the_three_skip_serializing_keys() {
        let absent = radio_absent_block();
        let populated = build_radio_block(Some(&Map::new()));
        let a = absent.as_object().unwrap();
        let p = populated.as_object().unwrap();
        assert_eq!(p.len(), 40, "the populated block is the 40-field set");
        assert_eq!(a.len(), 37, "the absent skeleton is the 37-field set");
        // `serde_json::Map` is a BTreeMap here, so compare the delta as a sorted
        // set rather than in declaration order.
        let mut only_populated: Vec<&str> = p
            .keys()
            .filter(|k| !a.contains_key(*k))
            .map(|k| k.as_str())
            .collect();
        only_populated.sort_unstable();
        assert_eq!(
            only_populated,
            vec!["restart_count", "tx_bytes_per_s", "tx_zombie_kills"]
        );
        assert!(
            a.keys().all(|k| p.contains_key(k)),
            "the absent skeleton must not carry a key the populated block lacks"
        );
    }

    #[test]
    fn the_verdict_fields_are_null_rather_than_a_fabricated_false() {
        let block = build_radio_block(Some(&Map::new()));
        for key in [
            "adapter_injection_ok",
            "adapter_usb_degraded",
            "phy_muted",
            "rf_unverified",
        ] {
            assert_eq!(block[key], Value::Null, "{key} must not fabricate a false");
        }
        // A real reading forwards verbatim, in both polarities.
        let mut status = Map::new();
        status.insert("adapter_injection_ok".to_string(), json!(false));
        status.insert("phy_muted".to_string(), json!(true));
        let block = build_radio_block(Some(&status));
        assert_eq!(block["adapter_injection_ok"], json!(false));
        assert_eq!(block["phy_muted"], json!(true));
    }

    #[test]
    fn a_stale_view_reports_no_rf_verdict() {
        let mut status = Map::new();
        status.insert("state".to_string(), json!("stale"));
        status.insert("rf_unverified".to_string(), json!(false));
        // A stale `false` is exactly the healthy-looking dead link this field
        // exists to expose.
        assert_eq!(
            build_radio_block(Some(&status))["rf_unverified"],
            Value::Null
        );
    }

    #[test]
    fn the_rssi_sentinel_reads_as_no_reading() {
        let mut status = Map::new();
        status.insert("rssi_dbm".to_string(), json!(RSSI_SENTINEL));
        assert_eq!(build_radio_block(Some(&status))["rssi_dbm"], Value::Null);
        status.insert("rssi_dbm".to_string(), json!(-61.5));
        assert_eq!(build_radio_block(Some(&status))["rssi_dbm"], json!(-61.5));
    }

    #[test]
    fn an_absent_stats_file_degrades_to_the_bare_base() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = WfbStatusConfig {
            channel: 149,
            ..Default::default()
        };
        let body = build_status_from_stats_file_at(&cfg, &dir.path().join("wfb-stats.json"));
        let obj = body.as_object().unwrap();
        assert_eq!(obj["state"], json!("disabled"));
        assert_eq!(obj["channel"], json!(149));
        // The bare base carries no finalize legs.
        assert!(!obj.contains_key("bitrate_mbps"));
        assert_eq!(obj["frequency_mhz"], json!(0));
    }

    #[test]
    fn a_fresh_stats_file_merges_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-stats.json");
        std::fs::write(
            &path,
            r#"{"state":"active","interface":"wlan1","channel":40,"bitrate_kbps":4057,"rssi_dbm":-40.0}"#,
        )
        .unwrap();
        let body = build_status_from_stats_file_at(&WfbStatusConfig::default(), &path);
        let obj = body.as_object().unwrap();
        assert_eq!(obj["state"], json!("active"));
        // Channel 40 resolves through the one table.
        assert_eq!(obj["frequency_mhz"], json!(5200));
        assert_eq!(obj["bandwidth_mhz"], json!(20));
        assert_eq!(obj["bitrate_mbps"], json!(4.057));
    }

    #[test]
    fn a_stale_stats_file_flips_state_and_drops_the_rf_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-stats.json");
        std::fs::write(
            &path,
            r#"{"state":"active","rf_unverified":false,"channel":149}"#,
        )
        .unwrap();
        // Back-date the file past the ceiling. `File::set_modified` keeps this
        // dependency-free — the crate carries no filetime dev-dependency.
        let old = std::time::SystemTime::now()
            - std::time::Duration::from_secs(WFB_STALE_AGE_S as u64 + 5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let body = build_status_from_stats_file_at(&WfbStatusConfig::default(), &path);
        let obj = body.as_object().unwrap().clone();
        assert_eq!(obj["state"], json!("stale"));
        assert_eq!(build_radio_block(Some(&obj))["rf_unverified"], Value::Null);
    }

    #[test]
    fn a_stored_event_older_than_the_ceiling_reads_stale() {
        let mut detail = Map::new();
        detail.insert("state".to_string(), json!("active"));
        let fresh = derive_wfb_status(&detail, now_unix_micros(), &WfbStatusConfig::default());
        assert_eq!(fresh["state"], json!("active"));

        let old_ts = now_unix_micros() - WFB_STALE_AGE_US - 1;
        let stale = derive_wfb_status(&detail, old_ts, &WfbStatusConfig::default());
        assert_eq!(stale["state"], json!("stale"));

        // A zero timestamp means "no event age known" and must not flip.
        let unknown = derive_wfb_status(&detail, 0, &WfbStatusConfig::default());
        assert_eq!(unknown["state"], json!("active"));
    }

    #[test]
    fn the_config_slice_defaults_when_the_file_or_section_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.yaml");
        assert_eq!(WfbStatusConfig::load(&missing).channel, 0);

        let cfg_path = dir.path().join("config.yaml");
        std::fs::write(&cfg_path, "agent:\n  name: x\n").unwrap();
        assert_eq!(WfbStatusConfig::load(&cfg_path).channel, 0);

        std::fs::write(
            &cfg_path,
            "video:\n  wfb:\n    channel: 157\n    tx_power_dbm: 10\n",
        )
        .unwrap();
        let cfg = WfbStatusConfig::load(&cfg_path);
        assert_eq!(cfg.channel, 157);
        assert_eq!(cfg.tx_power_dbm, json!(10));
        assert_eq!(cfg.topology, Value::Null);
    }

    #[test]
    fn a_regulatory_reading_is_reused_inside_its_window_and_re_read_after_it() {
        // The radio panel polls this route about once a second and each poll
        // used to fork `iw` twice. The reading is reused for the window, so a
        // poll costs at most one fork; past the window it is taken again.
        let cache = TimedCache::new();
        let reads = std::cell::Cell::new(0u32);
        let read = || {
            reads.set(reads.get() + 1);
            "US".to_string()
        };

        let t0 = std::time::Instant::now();
        assert_eq!(cache.get_or_read_at(t0, REG_DOMAIN_TTL, read), "US");
        assert_eq!(reads.get(), 1);

        // A second poll one second later, and the two calls a single request
        // makes, all ride the one reading.
        let t1 = t0 + std::time::Duration::from_secs(1);
        assert_eq!(cache.get_or_read_at(t1, REG_DOMAIN_TTL, read), "US");
        assert_eq!(cache.get_or_read_at(t1, REG_DOMAIN_TTL, read), "US");
        assert_eq!(reads.get(), 1, "the window was not honoured");

        // Past the window the domain is read again, so a real change reaches
        // the body rather than being pinned to whatever was true at boot.
        let t2 = t0 + REG_DOMAIN_TTL;
        assert_eq!(cache.get_or_read_at(t2, REG_DOMAIN_TTL, read), "US");
        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn a_cached_reading_is_never_a_value_nothing_read() {
        // The cache must not outlive its usefulness by inventing continuity: a
        // domain that genuinely changed is served as soon as the window is out,
        // and a failed read caches as the honest "unknown" rather than holding
        // the last good answer forever.
        let cache = TimedCache::new();
        let t0 = std::time::Instant::now();
        assert_eq!(
            cache.get_or_read_at(t0, REG_DOMAIN_TTL, || "US".to_string()),
            "US"
        );
        assert_eq!(
            cache.get_or_read_at(t0 + REG_DOMAIN_TTL, REG_DOMAIN_TTL, || "IN".to_string()),
            "IN"
        );
        assert_eq!(
            cache.get_or_read_at(t0 + REG_DOMAIN_TTL * 2, REG_DOMAIN_TTL, || "unknown"
                .to_string()),
            "unknown"
        );
    }
}
