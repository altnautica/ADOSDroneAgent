//! WFB-ng radio write routes: channel change + runtime TX-power.
//!
//! Two operator knobs the GCS radio panel writes:
//!
//! - **`POST /api/wfb/channel`** — change the WFB-ng channel. On this native front
//!   the transmit plane runs in a sibling `ados-radio` process, so there is no
//!   in-process manager to call; the channel change is forwarded as a coordinated
//!   hop to the radio's operator command socket (`/run/ados/radio-cmd.sock`). The
//!   radio announces the hop, waits for the ground station's ack, then commits, so
//!   the link does not go one-sided. A reachable socket's reply is authoritative.
//! - **`PUT /api/wfb/tx-power`** — set the runtime TX power (dBm). Forwarded to the
//!   radio's other command socket (`/run/ados/wfb-cmd.sock`), the same `set_tx_power`
//!   op the data-plane knobs use. The accepted value is persisted to
//!   `video.wfb.tx_power_dbm` in `/etc/ados/config.yaml` so the operator's
//!   preference survives a service restart.
//!
//! ## Why the socket is the only write path on this front
//!
//! The FastAPI route branches on whether the native radio is the running transmit
//! plane: native → forward to the command socket; otherwise reach for an
//! in-process Python WFB manager. This front sits in front of the standalone API
//! process and holds NO in-process manager. So the radio command socket is the
//! only write seam it can drive, and an unreachable socket is the no-link
//! condition: the front returns the FastAPI route's no-manager `503 "WFB-ng
//! service not running"`, which is exactly the terminal posture the FastAPI route
//! takes when the socket is unreachable AND no manager is present (the
//! native-radio box's only state). The front never panics on an absent socket; it
//! degrades to that 503.
//!
//! ## Guard order + envelopes (matched to the FastAPI routes)
//!
//! Channel:
//! 1. The channel must be a standard WFB channel → `400 {"detail": "Invalid
//!    channel <n>. Valid channels: [..]"}`, byte-identical to the FastAPI text.
//! 2. The hop is forwarded to the radio command socket. A reply with `ok: true`
//!    is `200 {"status": "ok", "channel": <echoed>, "frequency_mhz": <derived>}`;
//!    a reply with `ok: false` is `409 {"detail": {"error": "hop_refused",
//!    "message": <reason>}}`.
//! 3. An unreachable socket is the no-manager `503 {"detail": "WFB-ng service not
//!    running"}`.
//!
//! TX-power:
//! 1. Below 1 dBm → `400 {"detail": {"error": "below_floor", "min": 1}}`.
//! 2. Above the configured ceiling (`video.wfb.tx_power_max_dbm`, default 15) →
//!    `400 {"detail": {"error": "above_ceiling", "max": <ceiling>}}`.
//! 3. The value is forwarded to the radio command socket. A reply with `ok: true`
//!    yields the effective dBm; a reply with `ok: false` is `500 {"detail":
//!    {"error": "apply_failed", "message": <reason>}}`. An unreachable socket is
//!    the no-manager `503 {"detail": "WFB-ng service not running"}` (no persist on
//!    that path, matching the FastAPI route, which only persists after a
//!    non-raising apply).
//! 4. On accept the value is persisted to `video.wfb.tx_power_dbm` (atomic
//!    tmp+rename, every other config key preserved) and the route returns `200
//!    {"requested_dbm", "effective_dbm", "tx_power_max_dbm"}`.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::routes::detail;

// ---------------------------------------------------------------------------
// Standard WFB-ng channels (frequency lookup + the invalid-channel guard).
// ---------------------------------------------------------------------------

/// A WFB-ng channel: the channel number + its 5 GHz centre frequency. Mirrors the
/// Python `WfbChannel` fields the channel route reads.
#[derive(Clone, Copy)]
struct WfbChannel {
    channel_number: i64,
    frequency_mhz: i64,
}

/// The standard 5 GHz channels usable with WFB-ng on the RTL8812 family: the
/// U-NII-1 sub-band (36/40/44/48) and the U-NII-3 sub-band (149/153/157/161/165).
/// Mirrors the Python `STANDARD_CHANNELS` list exactly; the order is the order the
/// invalid-channel error lists the valid set in.
const STANDARD_CHANNELS: [WfbChannel; 9] = [
    WfbChannel { channel_number: 36, frequency_mhz: 5180 },
    WfbChannel { channel_number: 40, frequency_mhz: 5200 },
    WfbChannel { channel_number: 44, frequency_mhz: 5220 },
    WfbChannel { channel_number: 48, frequency_mhz: 5240 },
    WfbChannel { channel_number: 149, frequency_mhz: 5745 },
    WfbChannel { channel_number: 153, frequency_mhz: 5765 },
    WfbChannel { channel_number: 157, frequency_mhz: 5785 },
    WfbChannel { channel_number: 161, frequency_mhz: 5805 },
    WfbChannel { channel_number: 165, frequency_mhz: 5825 },
];

/// Look up a channel by number, or `None` for an unknown number. Mirrors the
/// Python `get_channel`.
fn get_channel(channel_number: i64) -> Option<WfbChannel> {
    STANDARD_CHANNELS
        .iter()
        .find(|c| c.channel_number == channel_number)
        .copied()
}

/// The valid-channel list rendered as the FastAPI error message uses it: a
/// Python-list repr `[36, 40, ..., 165]`. The numbers are joined with `, ` inside
/// square brackets, byte-identical to `str([c.channel_number for c in
/// STANDARD_CHANNELS])`.
fn valid_channels_repr() -> String {
    let inner = STANDARD_CHANNELS
        .iter()
        .map(|c| c.channel_number.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

// ---------------------------------------------------------------------------
// Runtime-dir seam: the radio operator command sockets.
// ---------------------------------------------------------------------------

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The native radio's coordinated-hop command socket (`/run/ados/radio-cmd.sock`),
/// the same socket `ados radio hop` drives. The channel route forwards a
/// `{"op":"hop","channel":N}` request here.
fn radio_cmd_sock() -> PathBuf {
    run_dir().join("radio-cmd.sock")
}

/// The native radio's data-plane command socket (`/run/ados/wfb-cmd.sock`). The
/// tx-power route forwards a `{"op":"set_tx_power","tx_power_dbm":N}` request here.
fn wfb_cmd_sock() -> PathBuf {
    run_dir().join("wfb-cmd.sock")
}

/// Send one newline-terminated JSON request to a radio command socket and read one
/// newline-terminated JSON reply. `None` on an unreachable socket, a read error, a
/// closed connection before a reply, or an unparseable reply, so the caller
/// degrades to the no-manager posture. Mirrors the framing both radio command
/// sockets use (one newline-terminated JSON each way, then the server closes).
async fn radio_cmd_roundtrip(socket: &Path, request: &Value) -> Option<Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A command reply is a few hundred bytes; bound the read to guard a runaway.
    const MAX_REPLY_BYTES: usize = 64 * 1024;

    let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let line = format!("{}\n", serde_json::to_string(request).ok()?);
    stream.write_all(line.as_bytes()).await.ok()?;
    stream.flush().await.ok()?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break; // EOF: the server replies once then closes.
        }
        if raw.len() + n > MAX_REPLY_BYTES {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        // The reply is one newline-terminated line; stop at the first newline.
        if raw.contains(&b'\n') {
            break;
        }
    }
    let text = String::from_utf8(raw).ok()?;
    let reply_line = text.lines().next()?;
    serde_json::from_str(reply_line).ok()
}

// ---------------------------------------------------------------------------
// POST /api/wfb/channel — change the WFB channel.
// ---------------------------------------------------------------------------

/// The `POST /api/wfb/channel` request body. Mirrors the FastAPI `ChannelRequest`:
/// a single required channel number.
#[derive(Debug, Deserialize)]
pub struct ChannelRequest {
    pub channel: i64,
}

/// `POST /api/wfb/channel` → set the WFB-ng channel.
///
/// Validates the channel against the standard set (`400` on an unknown channel),
/// then forwards a coordinated hop to the radio command socket. A reply with
/// `ok: true` is `200 {"status":"ok","channel":<echoed>,"frequency_mhz":<derived>}`;
/// a reply with `ok: false` is the FastAPI `409 hop_refused` body; an unreachable
/// socket is the FastAPI no-manager `503`. Never panics on an absent socket.
pub async fn set_wfb_channel(Json(req): Json<ChannelRequest>) -> Response {
    set_wfb_channel_at(&radio_cmd_sock(), req.channel).await
}

/// The channel-write logic against an explicit radio command socket path. The
/// public handler resolves the path from the runtime dir; this takes it directly
/// so a test can point it at a temp socket without mutating process-global env.
async fn set_wfb_channel_at(socket: &Path, channel: i64) -> Response {
    // 1. The channel must be a standard WFB channel. An unknown channel is the
    //    FastAPI 400 with the valid-channel list rendered as a Python-list repr.
    let ch = match get_channel(channel) {
        Some(ch) => ch,
        None => {
            return detail(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid channel {}. Valid channels: {}",
                    channel,
                    valid_channels_repr()
                ),
            );
        }
    };

    // 2. Forward the coordinated hop to the radio command socket. The front has no
    //    in-process manager, so a reachable reply is authoritative and an
    //    unreachable socket is the no-manager 503.
    let request = json!({"op": "hop", "channel": channel});
    let reply = match radio_cmd_roundtrip(socket, &request).await {
        Some(reply) => reply,
        None => {
            // Unreachable socket: the FastAPI route would fall through to the
            // in-process manager, which on this front is absent → the no-manager
            // 503, the same terminal posture as the FastAPI `wfb is None` branch.
            return detail(StatusCode::SERVICE_UNAVAILABLE, "WFB-ng service not running");
        }
    };

    channel_reply_response(&reply, channel, ch.frequency_mhz)
}

/// Map a radio command-socket reply to the channel route's response. `ok: true` is
/// the success body (the echoed channel preferred, the frequency derived from the
/// validated channel); anything else is the FastAPI `409 hop_refused` body with
/// the reply's `error` text (or the default `"hop refused"`). Factored out so the
/// reply→response mapping is testable without the socket.
fn channel_reply_response(reply: &Value, requested_channel: i64, frequency_mhz: i64) -> Response {
    if reply.get("ok") == Some(&Value::Bool(true)) {
        // Prefer the channel the radio echoed back; fall back to the request.
        let echoed = reply
            .get("channel")
            .and_then(Value::as_i64)
            .unwrap_or(requested_channel);
        return (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "channel": echoed,
                "frequency_mhz": frequency_mhz,
            })),
        )
            .into_response();
    }

    // ok != true: the radio refused the hop. The FastAPI route's 409 carries an
    // object detail with the reply's error text, defaulting to "hop refused".
    let message = reply
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("hop refused");
    (
        StatusCode::CONFLICT,
        Json(json!({"detail": {"error": "hop_refused", "message": message}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/wfb/tx-power — set the runtime TX power.
// ---------------------------------------------------------------------------

/// The `PUT /api/wfb/tx-power` request body. Mirrors the FastAPI `TxPowerRequest`:
/// a single required TX power in dBm.
#[derive(Debug, Deserialize)]
pub struct TxPowerRequest {
    pub tx_power_dbm: i64,
}

/// The floor the FastAPI route refuses below: a request under 1 dBm is a 400.
const TX_POWER_FLOOR: i64 = 1;

/// The default ceiling when `video.wfb.tx_power_max_dbm` is absent, matching the
/// FastAPI `int(getattr(wfb_cfg, "tx_power_max_dbm", 15))` default.
const TX_POWER_DEFAULT_CEILING: i64 = 15;

/// `PUT /api/wfb/tx-power` → set the WFB-ng TX power at runtime.
///
/// Refuses below 1 dBm and above the configured ceiling (`400` each), forwards the
/// value to the radio command socket, persists the accepted value to config, and
/// returns `{requested_dbm, effective_dbm, tx_power_max_dbm}`. A reply with
/// `ok: false` is the FastAPI `500 apply_failed`; an unreachable socket is the
/// no-manager `503` (no persist). Never panics on an absent socket.
pub async fn set_wfb_tx_power(Json(req): Json<TxPowerRequest>) -> Response {
    set_wfb_tx_power_at(&wfb_cmd_sock(), &config_yaml_path(), req.tx_power_dbm).await
}

/// The tx-power-write logic against an explicit radio command socket + config
/// path. The public handler resolves both from the runtime/etc dirs; this takes
/// them directly so a test can point them at temp paths without mutating env.
async fn set_wfb_tx_power_at(socket: &Path, config_path: &Path, requested: i64) -> Response {
    let ceiling = configured_tx_power_ceiling(config_path);

    // 1. Below the floor → 400 below_floor.
    if requested < TX_POWER_FLOOR {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": {"error": "below_floor", "min": TX_POWER_FLOOR}})),
        )
            .into_response();
    }
    // 2. Above the configured ceiling → 400 above_ceiling.
    if requested > ceiling {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": {"error": "above_ceiling", "max": ceiling}})),
        )
            .into_response();
    }

    // 3. Forward to the radio command socket. The front has no in-process manager,
    //    so an unreachable socket is the no-manager 503 (no persist on that path,
    //    matching the FastAPI route, which only persists after a non-raising apply).
    let request = json!({"op": "set_tx_power", "tx_power_dbm": requested});
    let effective: Value = match radio_cmd_roundtrip(socket, &request).await {
        Some(reply) => match tx_power_effective_from_reply(&reply) {
            Ok(eff) => eff,
            Err(message) => {
                // ok: false → the FastAPI `RadioCmdError` 500 apply_failed body.
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": {"error": "apply_failed", "message": message}})),
                )
                    .into_response();
            }
        },
        None => {
            // Unreachable socket: the FastAPI route falls back to the in-process
            // manager, which on this front is absent → the no-manager 503.
            return detail(StatusCode::SERVICE_UNAVAILABLE, "WFB-ng service not running");
        }
    };

    // 4. Persist the accepted value so it survives a restart, regardless of the
    //    driver's effective value, then return the success body. A persist failure
    //    does not change the response, matching the FastAPI route (which ignores
    //    the `_persist_tx_power` boolean result).
    let _ = persist_tx_power(config_path, requested);

    (
        StatusCode::OK,
        Json(json!({
            "requested_dbm": requested,
            "effective_dbm": effective,
            "tx_power_max_dbm": ceiling,
        })),
    )
        .into_response()
}

/// The effective dBm from a `set_tx_power` reply, mirroring the Python
/// `cmd_client.set_tx_power`: `ok: false` raises (mapped to the 500 apply_failed
/// body here, carrying the reply's `error` text); otherwise `effective_dbm` is a
/// number or `null` (the driver rejected every ramp step), so the success body
/// reports `null` when the field is absent / non-numeric. Returns `Err(message)`
/// for the `ok: false` case, `Ok(value)` for the success case.
fn tx_power_effective_from_reply(reply: &Value) -> Result<Value, String> {
    if reply.get("ok") == Some(&Value::Bool(false)) {
        // The Python client raises RadioCmdError(resp.get("error") or "unknown
        // radio command error"); the route maps that to the 500 apply_failed body.
        let message = reply
            .get("error")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown radio command error");
        return Err(message.to_string());
    }
    // effective_dbm is `int(eff)` when numeric, else None — the success body
    // carries the integer or JSON null.
    let effective = match reply.get("effective_dbm").and_then(Value::as_f64) {
        Some(eff) => json!(eff as i64),
        None => Value::Null,
    };
    Ok(effective)
}

/// The agent config path (`ADOS_CONFIG`, default `/etc/ados/config.yaml`), the
/// same resolution the sibling read routes use. Mirrors the Python `CONFIG_YAML`.
fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The configured TX-power ceiling: `video.wfb.tx_power_max_dbm` from the agent
/// config at `path`, or [`TX_POWER_DEFAULT_CEILING`] (15) when the field is absent
/// / non-numeric / the config is unreadable. Mirrors the FastAPI `int(getattr(
/// wfb_cfg, "tx_power_max_dbm", 15))`.
fn configured_tx_power_ceiling(path: &Path) -> i64 {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return TX_POWER_DEFAULT_CEILING,
    };
    let value: serde_norway::Value = match serde_norway::from_str(&text) {
        Ok(v) => v,
        Err(_) => return TX_POWER_DEFAULT_CEILING,
    };
    value
        .get("video")
        .and_then(|v| v.get("wfb"))
        .and_then(|v| v.get("tx_power_max_dbm"))
        .and_then(norway_to_i64)
        .unwrap_or(TX_POWER_DEFAULT_CEILING)
}

/// Coerce a serde_norway scalar to `i64`, accepting an integer or a float (the
/// config field may be either). `None` for a non-number. Mirrors the Python
/// `int(...)` over a numeric config value.
fn norway_to_i64(v: &serde_norway::Value) -> Option<i64> {
    match v {
        serde_norway::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Atomically merge `tx_power_dbm` into the `video.wfb` block of the on-disk
/// config at `path` so the operator's tuning survives a service restart,
/// preserving every other config key. Reads the full config as a YAML value,
/// navigates/creates `video.wfb`, sets the field, and writes via a tmp sibling +
/// rename. Returns `true` on success, `false` on any read/parse/write fault (the
/// route ignores the result, matching the FastAPI `_persist_tx_power` boolean it
/// never inspects). Mirrors the Python `_persist_wfb_fields({"tx_power_dbm": ...})`.
fn persist_tx_power(path: &Path, dbm: i64) -> bool {
    use serde_norway::Value as Yaml;

    // Load the existing config (an absent / non-mapping file starts from an empty
    // mapping, matching the Python `data: dict = {}` seed).
    let mut data: Yaml = match std::fs::read_to_string(path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(serde_norway::Mapping::new()),
        },
        Err(_) => Yaml::Mapping(serde_norway::Mapping::new()),
    };

    // Navigate/create `video.wfb` and set `tx_power_dbm`, preserving every other
    // key (and the mapping's insertion order, like the Python sort_keys=False).
    {
        let root = match data.as_mapping_mut() {
            Some(m) => m,
            None => return false,
        };
        let video = root
            .entry(Yaml::String("video".to_string()))
            .or_insert_with(|| Yaml::Mapping(serde_norway::Mapping::new()));
        if !video.is_mapping() {
            *video = Yaml::Mapping(serde_norway::Mapping::new());
        }
        let video_map = match video.as_mapping_mut() {
            Some(m) => m,
            None => return false,
        };
        let wfb = video_map
            .entry(Yaml::String("wfb".to_string()))
            .or_insert_with(|| Yaml::Mapping(serde_norway::Mapping::new()));
        if !wfb.is_mapping() {
            *wfb = Yaml::Mapping(serde_norway::Mapping::new());
        }
        let wfb_map = match wfb.as_mapping_mut() {
            Some(m) => m,
            None => return false,
        };
        wfb_map.insert(
            Yaml::String("tx_power_dbm".to_string()),
            Yaml::Number(dbm.into()),
        );
    }

    let body = match serde_norway::to_string(&data) {
        Ok(b) => b,
        Err(_) => return false,
    };
    write_atomic(path, body.as_bytes())
}

/// Write `bytes` to `path` atomically: ensure the parent dir, write a `.tmp`
/// sibling, then rename over the target. Mirrors the Python tmp-write +
/// `os.replace` idiom. Returns `false` on any I/O fault.
fn write_atomic(path: &Path, bytes: &[u8]) -> bool {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    let tmp = {
        let mut ext = path.extension().map(|e| e.to_os_string()).unwrap_or_default();
        ext.push(".tmp");
        path.with_extension(ext)
    };
    if std::fs::write(&tmp, bytes).is_err() {
        return false;
    }
    std::fs::rename(&tmp, path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── channel lookup + the invalid-channel guard ────────────────────────────

    #[test]
    fn get_channel_resolves_the_standard_set() {
        assert_eq!(get_channel(149).unwrap().frequency_mhz, 5745);
        assert_eq!(get_channel(36).unwrap().frequency_mhz, 5180);
        assert_eq!(get_channel(165).unwrap().frequency_mhz, 5825);
        // A non-standard channel is unknown.
        assert!(get_channel(7).is_none());
        assert!(get_channel(100).is_none());
    }

    #[test]
    fn valid_channels_repr_matches_the_python_list() {
        // The exact `str([c.channel_number for c in STANDARD_CHANNELS])` text.
        assert_eq!(
            valid_channels_repr(),
            "[36, 40, 44, 48, 149, 153, 157, 161, 165]"
        );
    }

    #[tokio::test]
    async fn an_invalid_channel_is_a_400_with_the_valid_list() {
        // The channel guard fires before any socket touch, so an absent socket
        // path under the tempdir is irrelevant.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("radio-cmd.sock");
        let resp = set_wfb_channel_at(&sock, 7).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_detail_string(resp).await,
            "Invalid channel 7. Valid channels: [36, 40, 44, 48, 149, 153, 157, 161, 165]"
        );
    }

    #[tokio::test]
    async fn a_valid_channel_with_no_radio_socket_is_a_503() {
        // An absent radio-cmd.sock → the roundtrip is None → the no-manager 503,
        // the FastAPI `wfb is None` terminal posture.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("radio-cmd.sock");
        let resp = set_wfb_channel_at(&sock, 149).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_detail_string(resp).await, "WFB-ng service not running");
    }

    // ── channel reply mapping: ok / refused ───────────────────────────────────

    #[tokio::test]
    async fn an_ok_reply_is_the_success_body_with_the_echoed_channel() {
        // The radio echoes back the channel it committed; the success body prefers
        // it, and the frequency is derived from the validated channel.
        let reply = json!({"ok": true, "channel": 153});
        let resp = channel_reply_response(&reply, 149, 5745);
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"status": "ok", "channel": 153, "frequency_mhz": 5745})
        );
    }

    #[tokio::test]
    async fn an_ok_reply_without_an_echoed_channel_falls_back_to_the_request() {
        let reply = json!({"ok": true});
        let resp = channel_reply_response(&reply, 149, 5745);
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"status": "ok", "channel": 149, "frequency_mhz": 5745})
        );
    }

    #[tokio::test]
    async fn a_refused_reply_is_a_409_with_the_error_object() {
        let reply = json!({"ok": false, "error": "peer did not ack"});
        let resp = channel_reply_response(&reply, 149, 5745);
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": "hop_refused", "message": "peer did not ack"}})
        );
    }

    #[tokio::test]
    async fn a_refused_reply_without_an_error_uses_the_default_message() {
        // ok absent / not true and no error text → the default "hop refused".
        let reply = json!({"ok": false});
        let resp = channel_reply_response(&reply, 149, 5745);
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": "hop_refused", "message": "hop refused"}})
        );
    }

    /// A full POST against a live radio command socket that acks the hop: the route
    /// forwards `{"op":"hop","channel":N}` and returns the success body with the
    /// echoed channel.
    #[tokio::test]
    async fn full_channel_post_against_a_live_socket_acks() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("radio-cmd.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // The server reads the one-line request and replies with an ok hop.
        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = conn.read(&mut buf).await.unwrap();
            let req: Value = serde_json::from_str(
                std::str::from_utf8(&buf[..n]).unwrap().trim(),
            )
            .unwrap();
            conn.write_all(b"{\"ok\": true, \"channel\": 149}\n")
                .await
                .unwrap();
            req
        });

        let resp = set_wfb_channel_at(&sock, 149).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = server.await.unwrap();
        assert_eq!(req, json!({"op": "hop", "channel": 149}));
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"status": "ok", "channel": 149, "frequency_mhz": 5745})
        );
    }

    // ── tx-power: the floor / ceiling guards ──────────────────────────────────

    #[tokio::test]
    async fn tx_power_below_the_floor_is_a_400_below_floor() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wfb-cmd.sock");
        let cfg = dir.path().join("config.yaml");
        let resp = set_wfb_tx_power_at(&sock, &cfg, 0).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body, json!({"detail": {"error": "below_floor", "min": 1}}));
    }

    #[tokio::test]
    async fn tx_power_above_the_default_ceiling_is_a_400_above_ceiling() {
        // With no config the ceiling defaults to 15; 20 is above it.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wfb-cmd.sock");
        let cfg = dir.path().join("config.yaml");
        let resp = set_wfb_tx_power_at(&sock, &cfg, 20).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body, json!({"detail": {"error": "above_ceiling", "max": 15}}));
    }

    #[test]
    fn configured_ceiling_reads_the_config_field() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "video:\n  wfb:\n    tx_power_max_dbm: 22\n").unwrap();
        assert_eq!(configured_tx_power_ceiling(&cfg), 22);
    }

    #[test]
    fn configured_ceiling_defaults_to_fifteen_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "video:\n  wfb:\n    channel: 149\n").unwrap();
        assert_eq!(configured_tx_power_ceiling(&cfg), 15);
        // An absent file also reads the default.
        assert_eq!(configured_tx_power_ceiling(&dir.path().join("absent.yaml")), 15);
    }

    // ── tx-power: the effective-dbm reply mapping ─────────────────────────────

    #[test]
    fn effective_reads_a_numeric_reply() {
        let reply = json!({"ok": true, "effective_dbm": 10});
        assert_eq!(tx_power_effective_from_reply(&reply), Ok(json!(10)));
        // A float effective is coerced to an integer (Python `int(eff)`).
        let reply = json!({"ok": true, "effective_dbm": 12.0});
        assert_eq!(tx_power_effective_from_reply(&reply), Ok(json!(12)));
    }

    #[test]
    fn effective_is_null_when_every_ramp_step_was_rejected() {
        // The driver rejected every step → effective_dbm absent/null → the success
        // body reports null (Python `None`).
        let reply = json!({"ok": true, "effective_dbm": Value::Null});
        assert_eq!(tx_power_effective_from_reply(&reply), Ok(Value::Null));
        let reply = json!({"ok": true});
        assert_eq!(tx_power_effective_from_reply(&reply), Ok(Value::Null));
    }

    #[test]
    fn an_apply_failure_reply_is_an_error_with_the_radio_message() {
        let reply = json!({"ok": false, "error": "txpower set failed"});
        assert_eq!(
            tx_power_effective_from_reply(&reply),
            Err("txpower set failed".to_string())
        );
        // ok: false with no error text uses the Python default message.
        let reply = json!({"ok": false});
        assert_eq!(
            tx_power_effective_from_reply(&reply),
            Err("unknown radio command error".to_string())
        );
    }

    #[tokio::test]
    async fn tx_power_with_no_radio_socket_is_a_503() {
        // An accepted value (within the floor/ceiling) but an unreachable command
        // socket is the no-manager 503; no persist happens on that path.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wfb-cmd.sock");
        let cfg = dir.path().join("config.yaml");
        let resp = set_wfb_tx_power_at(&sock, &cfg, 10).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_detail_string(resp).await, "WFB-ng service not running");
        // No persist on the 503 path: the config file was never created.
        assert!(!cfg.exists());
    }

    /// A full PUT against a live wfb command socket that accepts the value: the
    /// route forwards `{"op":"set_tx_power","tx_power_dbm":N}`, persists the value
    /// to config, and returns the success body with the effective dBm + ceiling.
    #[tokio::test]
    async fn full_tx_power_put_against_a_live_socket_acks_and_persists() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // Seed the config with a non-default ceiling + an unrelated key to prove the
        // persist preserves the rest of the file.
        std::fs::write(
            &cfg,
            "agent:\n  name: my-drone\nvideo:\n  wfb:\n    channel: 149\n    tx_power_max_dbm: 18\n",
        )
        .unwrap();

        let sock = dir.path().join("wfb-cmd.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = conn.read(&mut buf).await.unwrap();
            let req: Value = serde_json::from_str(
                std::str::from_utf8(&buf[..n]).unwrap().trim(),
            )
            .unwrap();
            conn.write_all(b"{\"ok\": true, \"effective_dbm\": 10}\n")
                .await
                .unwrap();
            req
        });

        let resp = set_wfb_tx_power_at(&sock, &cfg, 10).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = server.await.unwrap();
        assert_eq!(req, json!({"op": "set_tx_power", "tx_power_dbm": 10}));
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"requested_dbm": 10, "effective_dbm": 10, "tx_power_max_dbm": 18})
        );

        // The persist wrote tx_power_dbm into video.wfb and kept the rest.
        let written = std::fs::read_to_string(&cfg).unwrap();
        let parsed: serde_norway::Value = serde_norway::from_str(&written).unwrap();
        let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert_eq!(wfb.get("tx_power_dbm").and_then(norway_to_i64), Some(10));
        assert_eq!(wfb.get("channel").and_then(norway_to_i64), Some(149));
        assert_eq!(wfb.get("tx_power_max_dbm").and_then(norway_to_i64), Some(18));
        // The unrelated agent.name survived.
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("my-drone")
        );
    }

    // ── persist: preserves the rest of the file + creates missing sections ─────

    #[test]
    fn persist_creates_the_video_wfb_section_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();
        assert!(persist_tx_power(&cfg, 7));

        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert_eq!(wfb.get("tx_power_dbm").and_then(norway_to_i64), Some(7));
        // The pre-existing agent section is untouched.
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("my-drone")
        );
    }

    #[test]
    fn persist_creates_the_file_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        assert!(persist_tx_power(&cfg, 9));
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("video")
                .and_then(|v| v.get("wfb"))
                .and_then(|w| w.get("tx_power_dbm"))
                .and_then(norway_to_i64),
            Some(9)
        );
    }

    // ── shared body helpers ───────────────────────────────────────────────────

    /// Read the `{"detail": "<string>"}` body off a response.
    async fn body_detail_string(resp: Response) -> String {
        body_json(resp).await["detail"].as_str().unwrap().to_string()
    }

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
