//! WFB-ng radio write routes: channel change + runtime TX-power.
//!
//! Two operator knobs the GCS radio panel writes:
//!
//! - **`POST /api/wfb/channel`** — change the WFB-ng channel. On this native front
//!   the transmit plane runs in a sibling `ados-radio` process, so there is no
//!   in-process manager to call; the channel change is forwarded as a coordinated
//!   hop to the radio's operator command socket (`/run/ados/radio-cmd.sock`). The
//!   radio validates the hop and replies at once, BEFORE it announces the hop,
//!   waits for the ground station's ack and commits. So an accepted reply means
//!   the hop was initiated, not that the link is on the new channel: the route
//!   answers `202` with the target channel, and the operating channel is read
//!   from the status surfaces.
//! - **`PUT /api/wfb/tx-power`** — set the runtime TX power (dBm). Forwarded to the
//!   radio's other command socket (`/run/ados/wfb-cmd.sock`), the same `set_tx_power`
//!   op the data-plane knobs use. The accepted value is persisted to
//!   `video.wfb.tx_power_dbm` in `/etc/ados/config.yaml` so the operator's
//!   preference survives a service restart.
//!
//! ## Why the socket is the only write path on this front
//!
//! The transmit plane runs in the `ados-radio` process and this front holds NO
//! in-process manager. So the radio command socket is the only write seam it can
//! drive, and an unreachable socket is the no-link condition: the front returns
//! the no-manager `503 "WFB-ng service not running"`. The front never panics on
//! an absent socket; it degrades to that 503.
//!
//! ## Guard order + envelopes
//!
//! Channel:
//! 1. The channel must be a standard WFB channel → `400 {"detail": "Invalid
//!    channel <n>. Valid channels: [..]"}`.
//! 2. The hop is forwarded to the radio command socket. A reply with `ok: true`
//!    is `202 {"status": "initiated", "target_channel": <echoed>, "frequency_mhz":
//!    <derived>}`; a reply with `ok: false` is `409 {"detail": {"error":
//!    "hop_refused", "message": <reason>}}`.
//! 3. An unreachable socket is the no-manager `503 {"detail": "WFB-ng service not
//!    running"}`; a socket that does not reply within `RADIO_CMD_TIMEOUT` is
//!    `504`, and an unparseable reply `502`.
//!
//! TX-power:
//! 1. Below 1 dBm → `400 {"detail": {"error": "below_floor", "min": 1}}`.
//! 2. Above the configured ceiling (`video.wfb.tx_power_max_dbm`, default 15) →
//!    `400 {"detail": {"error": "above_ceiling", "max": <ceiling>}}`.
//! 3. The value is forwarded to the radio command socket. A reply with `ok: true`
//!    yields the effective dBm; a reply with `ok: false` is `500 {"detail":
//!    {"error": "apply_failed", "message": <reason>}}`. An unreachable socket is
//!    the no-manager `503 {"detail": "WFB-ng service not running"}`, a silent one
//!    `504` and an unparseable reply `502`; none of those persist (only an
//!    accepted apply persists).
//! 4. On accept the value is persisted to `video.wfb.tx_power_dbm` through the
//!    shared config store (every other key preserved; a document that cannot be
//!    read or parsed is never written over) and the route returns `200
//!    {"requested_dbm", "effective_dbm", "tx_power_max_dbm", "persisted"}`, plus
//!    `persist_error` when the value was applied but will not survive a restart.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config_store::{section_path, update_config, ConfigWriteError};
use crate::ipc::cmd::{self, CmdFailure};
use crate::routes::detail;

// ---------------------------------------------------------------------------
// Standard WFB-ng channels (frequency lookup + the invalid-channel guard).
// ---------------------------------------------------------------------------

/// The valid-channel list rendered as the error message uses it: a list repr
/// `[36, 40, ..., 165]`, the numbers joined with `, ` inside square brackets.
///
/// Reads `ados_protocol::wfb_status::STANDARD_CHANNELS`, the one table. This
/// module carried a private copy of the same nine pairs; a third copy in the
/// radio block omitted 40 and 44 and rendered a null frequency for a link on
/// either, which is what a duplicated table buys.
fn valid_channels_repr() -> String {
    let inner = ados_protocol::wfb_status::STANDARD_CHANNELS
        .iter()
        .map(|c| c.channel_number.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

// ---------------------------------------------------------------------------
// Runtime-dir seam: the radio operator command sockets.
// ---------------------------------------------------------------------------

/// The native radio's coordinated-hop command socket (`/run/ados/radio-cmd.sock`),
/// the same socket `ados radio hop` drives. The channel route forwards a
/// `{"op":"hop","channel":N}` request here.
fn radio_cmd_sock() -> PathBuf {
    ados_protocol::paths::run_path("radio-cmd")
}

/// The native radio's data-plane command socket (`/run/ados/wfb-cmd.sock`). The
/// tx-power route forwards a `{"op":"set_tx_power","tx_power_dbm":N}` request
/// here, and the video-config write its FEC / MCS / preset / link-tier ops.
pub(crate) fn wfb_cmd_sock() -> PathBuf {
    ados_protocol::paths::run_path("wfb-cmd")
}

/// How long a radio command-socket round trip may take. Both sockets reply
/// once validation or the apply completes (a hop replies before its announce),
/// so a healthy radio answers well inside this.
const RADIO_CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The route response for a radio command-socket exchange that produced no reply.
fn radio_failure_response(failure: CmdFailure) -> Response {
    match failure {
        CmdFailure::Unreachable => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "WFB-ng service not running",
        ),
        CmdFailure::Timeout => detail(
            StatusCode::GATEWAY_TIMEOUT,
            "the radio command socket did not reply in time",
        ),
        CmdFailure::BadReply => detail(
            StatusCode::BAD_GATEWAY,
            "the radio command socket returned an unreadable reply",
        ),
    }
}

/// One exchange with a radio command socket (both radio sockets speak the shared
/// one-line JSON framing), bounded by [`RADIO_CMD_TIMEOUT`].
async fn radio_cmd_roundtrip(socket: &Path, request: &Value) -> Result<Value, CmdFailure> {
    cmd::roundtrip(socket, request, RADIO_CMD_TIMEOUT).await
}

// ---------------------------------------------------------------------------
// POST /api/wfb/channel — change the WFB channel.
// ---------------------------------------------------------------------------

/// The `POST /api/wfb/channel` request body: a single required channel number.
#[derive(Debug, Deserialize)]
pub struct ChannelRequest {
    pub channel: i64,
}

/// `POST /api/wfb/channel` → request a coordinated WFB-ng channel hop.
///
/// Validates the channel against the standard set (`400` on an unknown channel),
/// then forwards a coordinated hop to the radio command socket. A reply with
/// `ok: true` is `202 {"status":"initiated","target_channel":<echoed>,
/// "frequency_mhz":<derived>}`; a reply with `ok: false` is the `409
/// hop_refused` body; an unreachable socket is the no-manager `503`.
pub async fn set_wfb_channel(Json(req): Json<ChannelRequest>) -> Response {
    set_wfb_channel_at(&radio_cmd_sock(), req.channel).await
}

/// The channel-write logic against an explicit radio command socket path. The
/// public handler resolves the path from the runtime dir; this takes it directly
/// so a test can point it at a temp socket without mutating process-global env.
async fn set_wfb_channel_at(socket: &Path, channel: i64) -> Response {
    // 1. The channel must be a standard WFB channel. An unknown channel is the
    //    400 with the valid-channel list rendered as a Python-list repr.
    let ch = match ados_protocol::wfb_status::get_channel(channel) {
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
    match radio_cmd_roundtrip(socket, &request).await {
        Ok(reply) => channel_reply_response(&reply, channel, ch.frequency_mhz),
        Err(failure) => radio_failure_response(failure),
    }
}

/// Map a radio command-socket reply to the channel route's response. `ok: true` is
/// the initiated body (the echoed target channel preferred, the frequency derived
/// from it); anything else is the `409 hop_refused` body with the reply's
/// `error` text (or the default `"hop refused"`). Factored out so the
/// reply→response mapping is testable without the socket.
fn channel_reply_response(reply: &Value, requested_channel: i64, frequency_mhz: i64) -> Response {
    if reply.get("ok") == Some(&Value::Bool(true)) {
        // The radio validated the hop and has started it; it commits only after
        // the ground station acks the announce, so nothing here is the operating
        // channel yet.
        let target = reply
            .get("channel")
            .and_then(Value::as_i64)
            .unwrap_or(requested_channel);
        let frequency = ados_protocol::wfb_status::get_channel(target)
            .map_or(frequency_mhz, |c| c.frequency_mhz);
        return (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "initiated",
                "target_channel": target,
                "frequency_mhz": frequency,
            })),
        )
            .into_response();
    }

    // ok != true: the radio refused the hop. The 409 carries an
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

/// The `PUT /api/wfb/tx-power` request body: a single required TX power in dBm.
#[derive(Debug, Deserialize)]
pub struct TxPowerRequest {
    pub tx_power_dbm: i64,
}

/// The floor the route refuses below: a request under 1 dBm is a 400.
const TX_POWER_FLOOR: i64 = 1;

/// The default ceiling when `video.wfb.tx_power_max_dbm` is absent.
const TX_POWER_DEFAULT_CEILING: i64 = 15;

/// `PUT /api/wfb/tx-power` → set the WFB-ng TX power at runtime.
///
/// Refuses below 1 dBm and above the configured ceiling (`400` each), forwards the
/// value to the radio command socket, persists the accepted value to config, and
/// returns `{requested_dbm, effective_dbm, tx_power_max_dbm}`. A reply with
/// `ok: false` is the `500 apply_failed`; an unreachable socket is the
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
    //    so an unreachable socket is the no-manager 503; neither it nor a silent or
    //    garbled socket persists; only an accepted apply persists.
    let request = json!({"op": "set_tx_power", "tx_power_dbm": requested});
    let effective: Value = match radio_cmd_roundtrip(socket, &request).await {
        Ok(reply) => match tx_power_effective_from_reply(&reply) {
            Ok(eff) => eff,
            Err(TxPowerRefusal::Refused(message)) => {
                // ok: false → the 500 apply_failed body.
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": {"error": "apply_failed", "message": message}})),
                )
                    .into_response();
            }
            // Neither ok:true nor ok:false: the apply is unconfirmed, so nothing
            // is persisted.
            Err(TxPowerRefusal::Unreadable) => return radio_failure_response(CmdFailure::BadReply),
        },
        Err(failure) => return radio_failure_response(failure),
    };

    // 4. Persist the accepted value so it survives a restart, regardless of the
    //    driver's effective value. The radio already took the value, so a persist
    //    failure does not turn the apply into an error; the body says whether the
    //    setting will survive a restart (`persisted`) and, when not, why.
    let persist = persist_tx_power(config_path, requested);
    if let Err(e) = &persist {
        tracing::error!(
            path = %config_path.display(),
            requested_dbm = requested,
            error = %e,
            "tx power applied to the radio but not persisted; it will revert on restart"
        );
    }

    let mut body = json!({
        "requested_dbm": requested,
        "effective_dbm": effective,
        "tx_power_max_dbm": ceiling,
        "persisted": persist.is_ok(),
    });
    if let Err(e) = persist {
        body["persist_error"] = json!(e.to_string());
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// Why a `set_tx_power` reply does not confirm the apply.
#[derive(Debug, PartialEq)]
enum TxPowerRefusal {
    /// `ok: false`, with the radio's error text.
    Refused(String),
    /// Neither `ok: true` nor `ok: false`: not a confirmation of anything.
    Unreadable,
}

/// The effective dBm from a `set_tx_power` reply. Only `ok: true` confirms the
/// apply; `ok: false` is a refusal (the 500 apply_failed body, carrying the reply's
/// `error` text) and any other shape confirms nothing. On success `effective_dbm`
/// is a number or `null` (the driver rejected every ramp step), so the success
/// body reports `null` when the field is absent / non-numeric.
fn tx_power_effective_from_reply(reply: &Value) -> Result<Value, TxPowerRefusal> {
    match reply.get("ok") {
        Some(Value::Bool(true)) => {}
        Some(Value::Bool(false)) => {
            let message = reply
                .get("error")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown radio command error");
            return Err(TxPowerRefusal::Refused(message.to_string()));
        }
        _ => return Err(TxPowerRefusal::Unreadable),
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
/// same resolution the sibling read routes use and the Python `CONFIG_YAML`.
pub(crate) fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The configured TX-power ceiling: `video.wfb.tx_power_max_dbm` from the agent
/// config at `path`, or [`TX_POWER_DEFAULT_CEILING`] (15) when the field is absent
/// / non-numeric / the config is unreadable.
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
/// config field may be either; a float truncates). `None` for a non-number.
fn norway_to_i64(v: &serde_norway::Value) -> Option<i64> {
    match v {
        serde_norway::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Merge `tx_power_dbm` into the `video.wfb` block of the on-disk config at
/// `path` so the operator's tuning survives a service restart, preserving every
/// other config key. Goes through the shared config store, so a document that
/// cannot be read or parsed is left untouched and the error says why.
fn persist_tx_power(path: &Path, dbm: i64) -> Result<(), ConfigWriteError> {
    use serde_norway::Value as Yaml;
    update_config(path, |root| {
        section_path(root, &["video", "wfb"]).insert(
            Yaml::String("tx_power_dbm".to_string()),
            Yaml::Number(dbm.into()),
        );
        Ok(())
    })
    .map(|_| ())
}

#[cfg(test)]
mod tests;
