//! The Ethernet-profile and cellular-modem write routes.

use super::*;

/// The `PUT .../network/ethernet` request body. A required `mode` (`dhcp` | `static`)
/// plus the static fields. There is no schema pre-validation, so a malformed `ip` (IPv4
/// with a CIDR suffix), `gateway` (IPv4) or `dns` (each IPv4) reaches the daemon's
/// `nmcli` apply and surfaces as `E_ETHERNET_APPLY_FAILED` — the same posture the
/// priority route documents.
#[derive(Debug, Deserialize)]
pub struct EthernetConfigUpdate {
    pub mode: String,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub gateway: Option<String>,
    #[serde(default)]
    pub dns: Option<Vec<String>>,
}

/// `PUT .../network/ethernet` → the live `config()` view.
///
/// Gates on the ground-station profile (404 on a drone). For `mode=static`,
/// rejects a missing ip / gateway with the FastAPI 400
/// `E_ETHERNET_STATIC_MISSING_FIELDS` (the in-handler guard) before forwarding.
/// Forwards an `eth_config` op to the `ados-net` command socket (the daemon
/// applies it through its live ethernet manager and replies with the `config()`
/// view, or an apply-failed payload), then returns the view. An apply that returns
/// `applied:false` maps to the FastAPI 500 — `E_ETHERNET_NO_CONNECTION` when the
/// manager reports `no_ethernet_connection`, else `E_ETHERNET_APPLY_FAILED`,
/// carrying the manager's `hint`. An unreachable socket → 503.
pub async fn put_network_ethernet(
    State(_state): State<AppState>,
    Json(update): Json<EthernetConfigUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    let is_static = update.mode == "static";
    if is_static {
        // The FastAPI in-handler guard: static requires ip + gateway.
        let ip_present = update.ip.as_deref().is_some_and(|s| !s.is_empty());
        let gw_present = update.gateway.as_deref().is_some_and(|s| !s.is_empty());
        if !ip_present || !gw_present {
            return error_body(
                StatusCode::BAD_REQUEST,
                "E_ETHERNET_STATIC_MISSING_FIELDS",
                "ip and gateway are required when mode=static",
            );
        }
    }

    let request = if is_static {
        json!({
            "op": "eth_config",
            "mode": "static",
            "ip": update.ip,
            "gateway": update.gateway,
            "dns": update.dns.clone().unwrap_or_default(),
        })
    } else {
        json!({"op": "eth_config", "mode": "dhcp"})
    };

    // The unreachable / server-error codes differ between the static + dhcp apply
    // failures (the FastAPI route wraps each manager call in its own except), so
    // the socket-transport failure code follows the requested mode.
    let transport_code = if is_static {
        "E_ETHERNET_STATIC_FAILED"
    } else {
        "E_ETHERNET_DHCP_FAILED"
    };

    let reply = match net_cmd(&request).await {
        NetCmd::Reply(r) => r,
        NetCmd::Error(msg) => {
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, transport_code, &msg)
        }
        NetCmd::Unavailable | NetCmd::TimedOut => return socket_unavailable(transport_code),
    };

    // A processed-but-failed apply (`applied:false`) is the FastAPI
    // `result.get("ok") is False` arm: 500 with E_ETHERNET_NO_CONNECTION when the
    // manager reports a missing connection, else E_ETHERNET_APPLY_FAILED, carrying
    // the manager's `error` text + `hint`.
    if reply.get("applied") == Some(&Value::Bool(false)) {
        let error = reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("ethernet_apply_failed");
        let code = if error == "no_ethernet_connection" {
            "E_ETHERNET_NO_CONNECTION"
        } else {
            "E_ETHERNET_APPLY_FAILED"
        };
        let hint = reply.get("hint").cloned().unwrap_or(Value::Null);
        return error_body_with_hint(StatusCode::INTERNAL_SERVER_ERROR, code, error, &hint);
    }

    // Success: the manager's `config()` view.
    Json(Value::Object(reply)).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/network/modem — update the cellular modem config.
// ---------------------------------------------------------------------------

/// The `PUT .../network/modem` request body. The GET view reports the cap as `cap_mb`,
/// so a client that round-trips the view sends `cap_mb` back. `cap_gb` wins when both
/// are present; otherwise `cap_mb` is converted to `cap_gb` before it reaches the
/// manager (which persists in GB).
#[derive(Debug, Deserialize)]
pub struct ModemConfigUpdate {
    #[serde(default)]
    pub apn: Option<String>,
    #[serde(default)]
    pub cap_gb: Option<f64>,
    #[serde(default)]
    pub cap_mb: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `PUT .../network/modem` → the `_modem_view` body.
///
/// Gates on the ground-station profile (404 on a drone). Converts `cap_mb` →
/// `cap_gb` when only the former is supplied, forwards a `modem_config` op to the
/// `ados-net` command socket (the daemon persists the config sidecar through its
/// live modem manager; its poll loop reconciles the live session), then returns
/// the modem view — the SAME `_modem_view()` body the GET route serves over the
/// freshly-persisted config (config file + the store's `net.modem_usage` overlay),
/// exactly as the FastAPI modem PUT returns `_modem_view()` after `configure()`. A
/// failed configure maps to the FastAPI 500 `E_MODEM_CONFIGURE_FAILED`; an
/// unreachable socket → 503.
pub async fn put_network_modem(
    State(state): State<AppState>,
    Json(update): Json<ModemConfigUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    // The APN is spliced into an AT command line on the modem's fallback path,
    // so only the APN charset is ever forwarded.
    if let Some(apn) = update.apn.as_deref() {
        if let Err(msg) = ados_net::managers::validate_apn(apn) {
            return error_body(StatusCode::BAD_REQUEST, "E_INVALID_APN", &msg);
        }
    }

    // cap_gb wins; otherwise convert cap_mb → cap_gb (mirrors the Python
    // `update.cap_mb / 1024.0`).
    let cap_gb = update
        .cap_gb
        .or_else(|| update.cap_mb.map(|mb| mb as f64 / 1024.0));

    let mut request = Map::new();
    request.insert("op".to_string(), json!("modem_config"));
    request.insert("apn".to_string(), json!(update.apn));
    request.insert("cap_gb".to_string(), json!(cap_gb));
    request.insert("enabled".to_string(), json!(update.enabled));

    match net_cmd(&Value::Object(request)).await {
        NetCmd::Reply(_) => {}
        NetCmd::Error(msg) => {
            return error_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_MODEM_CONFIGURE_FAILED",
                &msg,
            )
        }
        NetCmd::Unavailable | NetCmd::TimedOut => {
            return socket_unavailable("E_MODEM_CONFIGURE_FAILED")
        }
    }

    // The configure persisted the sidecar; the response is the modem view over the
    // freshly-persisted config (the same helper the GET route uses).
    Json(crate::routes::gs_network::modem_view(&state).await).into_response()
}
