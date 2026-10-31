//! Applying validated commands to the live managers.

use super::*;

/// Apply a Wi-Fi-client command, or hand a non-Wi-Fi command back unapplied.
pub(super) async fn apply_wifi(
    cmd: Command,
    wifi: &Mutex<WifiClientManager>,
) -> Result<Value, Command> {
    Ok(match cmd {
        Command::Join {
            ssid,
            passphrase,
            force,
        } => with_ok(
            wifi.lock()
                .await
                .join(&ssid, passphrase.as_deref(), force)
                .await,
        ),
        Command::Forget { name } => with_ok(wifi.lock().await.forget(&name).await),
        Command::Leave => with_ok(wifi.lock().await.leave().await),
        Command::Status => {
            let mut st: Map<String, Value> = wifi.lock().await.status().await;
            st.insert("ok".to_string(), json!(true));
            Value::Object(st)
        }
        Command::Autoconnect { name, enabled } => {
            with_ok(wifi.lock().await.set_autoconnect(&name, enabled).await)
        }
        // The manager stays locked for the scan so it cannot overlap a join.
        Command::Scan => match wifi.lock().await.scan().await {
            Ok(networks) => json!({"ok": true, "networks": networks}),
            Err(error) => json!({"ok": false, "error": error}),
        },
        other => return Err(other),
    })
}

/// Apply a validated command to the live managers. The `ok` flag is derived from
/// the manager's own result so the REST forward client can branch on it: a
/// manager result object replies `ok:true` (its own success field carries the
/// outcome the REST layer inspects); only a non-object manager result yields
/// `ok:false`.
pub(super) async fn apply(cmd: Command, state: &CmdState) -> Value {
    let cmd = match apply_wifi(cmd, &state.wifi).await {
        Ok(v) => return v,
        Err(other) => other,
    };
    match cmd {
        Command::ApConfig {
            ssid,
            passphrase,
            channel,
            enabled,
        } => apply_ap_config(state, ssid, passphrase, channel, enabled).await,
        Command::EthStatic { ip, gateway, dns } => {
            let res = state.ethernet.configure_static(&ip, &gateway, &dns).await;
            apply_eth_result(state, res).await
        }
        Command::EthDhcp => {
            let res = state.ethernet.configure_dhcp().await;
            apply_eth_result(state, res).await
        }
        Command::ModemConfig {
            apn,
            cap_gb,
            enabled,
        } => match state.modem.configure(apn.as_deref(), cap_gb, enabled).await {
            Ok(res) => with_ok(res),
            Err(error) => json!({"ok": false, "error": format!("E_INVALID_APN: {error}")}),
        },
        Command::ShareUplink { enabled } => {
            let iface = state.router.active_iface().await;
            with_ok(
                state
                    .firewall
                    .set_share_uplink(enabled, iface.as_deref())
                    .await,
            )
        }
        Command::Join { .. }
        | Command::Forget { .. }
        | Command::Leave
        | Command::Status
        | Command::Scan
        | Command::Autoconnect { .. } => unreachable!("Wi-Fi-client ops are applied above"),
    }
}

/// Apply an AP-config change to the live hostapd manager and return the AP view.
///
/// Side-effect order: `apply_ap_config(ssid, passphrase,
/// channel)`, then the `enabled` start/stop hint (start when asked-on and not
/// running, stop when asked-off and running, leave it alone otherwise), then the
/// live `status()` reshaped into the `_ap_view` body (`enabled` mirrors
/// `running`). A failed `apply_ap_config` replies `ok:false` with the apply error
/// so the REST layer surfaces the `E_AP_APPLY_FAILED` 500. The config-file
/// persist (`network.hotspot.channel`/`ssid`) is the REST layer's job, not the
/// daemon's, so it is not done here.
async fn apply_ap_config(
    state: &CmdState,
    ssid: Option<String>,
    passphrase: Option<String>,
    channel: Option<u32>,
    enabled: Option<bool>,
) -> Value {
    if let Err(error) =
        crate::managers::validate_ap_settings(ssid.as_deref(), passphrase.as_deref(), channel)
    {
        return json!({"ok": false, "error": format!("E_AP_INVALID_CONFIG: {error}")});
    }
    let mut mgr = state.hostapd.lock().await;
    let ok = mgr
        .apply_ap_config(ssid.as_deref(), passphrase.as_deref(), channel)
        .await;
    if !ok {
        return json!({"ok": false, "error": "E_AP_APPLY_FAILED"});
    }
    // The `enabled` hint: start/stop only on a real transition, swallowing any
    // unit error, matching the Python `try/except: pass` around the start/stop.
    if let Some(want) = enabled {
        let running = mgr
            .status()
            .await
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if want && !running {
            let _ = mgr.start().await;
        } else if !want && running {
            mgr.stop().await;
        }
    }
    ap_view(&mgr.status().await)
}

/// Reshape the live hostapd `status()` into the `_ap_view` body: the manager's
/// `status()` already carries `{running, ssid, channel, interface, gateway,
/// connected_clients}`; the view adds `enabled` (mirroring `running`) and stamps the
/// transport `ok:true`.
pub(super) fn ap_view(status: &Value) -> Value {
    let running = status
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    json!({
        "ok": true,
        "enabled": running,
        "running": running,
        "ssid": status.get("ssid").cloned().unwrap_or(Value::Null),
        "channel": status.get("channel").cloned().unwrap_or(Value::Null),
        "interface": status.get("interface").cloned().unwrap_or(Value::Null),
        "gateway": status.get("gateway").cloned().unwrap_or(Value::Null),
        "connected_clients": status
            .get("connected_clients")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
    })
}

/// Branch an Ethernet apply result, mirroring the Python ethernet PUT tail: a
/// result with `ok:false` is the apply-failed case (the REST layer maps it to the
/// FastAPI 500 with `E_ETHERNET_NO_CONNECTION` vs `E_ETHERNET_APPLY_FAILED` keyed
/// on the `error` string, carrying the `hint`); otherwise the route returns the
/// manager's `config()` view. The transport `ok` is stamped onto the carried
/// payload either way so the REST layer inspects the manager's `error`/`hint`
/// (apply-failed) or the config view (success).
async fn apply_eth_result(state: &CmdState, result: Value) -> Value {
    let failed = matches!(result.get("ok"), Some(Value::Bool(false)));
    if failed {
        // Carry the manager's error + hint to the REST layer as an apply failure.
        // The reply stays ok:true at the transport layer (the apply ran; the
        // manager reports its own failure in `error`), matching the WiFi ops where
        // a processed-but-failed result is still a transport success.
        let error = result
            .get("error")
            .cloned()
            .unwrap_or_else(|| Value::String("ethernet_apply_failed".to_string()));
        let hint = result.get("hint").cloned().unwrap_or(Value::Null);
        return json!({"ok": true, "applied": false, "error": error, "hint": hint});
    }
    // Success: return the live config() view the Python route returns.
    let mut cfg = state.ethernet.config().await;
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert("ok".to_string(), json!(true));
    }
    cfg
}

/// Stamp `ok:true` onto a processed manager result object. A processed command
/// always replies `ok:true` — its own success field (`joined`/`forgot`/`left`/
/// `autoconnect`) carries whether the action succeeded, which the REST layer
/// inspects (a failed join is a normal result with a 409/error path, NOT a
/// transport error). Only a parse/encode failure or a non-object manager result
/// yields `ok:false`.
pub(super) fn with_ok(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("ok".to_string(), json!(true));
        v
    } else {
        json!({"ok": false, "error": "E_BAD_MANAGER_RESULT"})
    }
}
