//! Profile-agnostic Wi-Fi-client read routes.
//!
//! Joining an upstream Wi-Fi network only needs a `wlan` interface, not a
//! particular operator profile, so a drone and a ground station both expose
//! these reads. The matching writes (join / leave / forget) live in
//! `network_write`; the saved-profile autoconnect toggle stays proxied.
//!
//! Two of the three Python reads (`network.py`) are served here:
//!
//! - **`GET /api/v1/network/client/status`** — the live station connection
//!   state `{connected, ssid, bssid, signal, ip, gateway, security}`, served
//!   natively where the `ados-net` uplink daemon runs (a ground station): the
//!   front reads the station state off that daemon's Wi-Fi command socket's
//!   `wifi_status` op and reshapes the reply to the seven-key body. An
//!   unreachable socket is a `503 E_WIFI_STATUS_UNAVAILABLE`: the station state
//!   is unknown, and `connected:false` would claim a fact nobody measured. On a
//!   drone the route is not registered here and the residual's packaged Wi-Fi
//!   manager answers it.
//! - **`GET /api/v1/network/client/configured`** — the saved NetworkManager
//!   Wi-Fi profiles `{connections:[{name, type, device, autoconnect}, …]}`.
//!   A read-only `nmcli -t -f NAME,TYPE,DEVICE,AUTOCONNECT connection show`
//!   (the same read-only `nmcli connection show` seam the ground-station
//!   ethernet view uses), filtered to the wireless connection types, matching
//!   the Python `configured_connections()` field-for-field. An absent / failing
//!   `nmcli` degrades to the empty list, the same body the Python route returns
//!   when the listing fails.
//!
//! `GET /api/v1/network/client/scan` is NOT served here: the Python `scan()`
//! runs `nmcli device wifi list --rescan yes`, which TRIGGERS a fresh active
//! scan (a side effect), and there is no scan op on the daemon command socket.
//! That route stays proxied.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::routes::gs_network::{json_truthy, nmcli_connections, wifi_status};

// ---------------------------------------------------------------------------
// GET /api/v1/network/client/status — live station connection state.
// ---------------------------------------------------------------------------

/// `GET /api/v1/network/client/status` → the live Wi-Fi station connection
/// state, off the `ados-net` daemon's Wi-Fi command socket's `wifi_status` op,
/// reshaped to the seven-key body. An unreachable socket / a `wifi_status`
/// failure is a `503`: the station state is unknown.
pub async fn get_client_status() -> Response {
    match client_status_view(wifi_status().await) {
        Some(view) => Json(view).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail": {"error": {
                "code": "E_WIFI_STATUS_UNAVAILABLE",
                "message": "Wi-Fi manager unreachable; station state unknown",
            }}})),
        )
            .into_response(),
    }
}

/// Reshape a `wifi_status` reply into the seven-key client-status body, or
/// `None` when there is no reply. Split out so the shape is unit tested without
/// the socket IO. The reply carries the `{connected, ssid, bssid, signal, ip,
/// gateway, security}` keys plus an `ok` flag the view drops.
fn client_status_view(reply: Option<serde_json::Map<String, Value>>) -> Option<Value> {
    let reply = reply?;
    let field = |key: &str| reply.get(key).cloned().unwrap_or(Value::Null);
    Some(json!({
        "connected": reply.get("connected").map(json_truthy).unwrap_or(false),
        "ssid": field("ssid"),
        "bssid": field("bssid"),
        "signal": field("signal"),
        "ip": field("ip"),
        "gateway": field("gateway"),
        "security": field("security"),
    }))
}

// ---------------------------------------------------------------------------
// GET /api/v1/network/client/configured — saved NetworkManager Wi-Fi profiles.
// ---------------------------------------------------------------------------

/// `GET /api/v1/network/client/configured` → the saved NetworkManager Wi-Fi
/// profiles. Profile-agnostic, so there is no profile gate.
///
/// Reads the saved connection list with a read-only
/// `nmcli -t -f NAME,TYPE,DEVICE,AUTOCONNECT connection show` (the same
/// read-only `nmcli connection show` seam the ground-station ethernet view
/// reuses) and reshapes the wireless rows to the Python
/// `configured_connections()` body. An absent / failing `nmcli` yields the empty
/// list, the same body the Python route returns when the listing fails.
pub async fn get_client_configured() -> Response {
    let rows = nmcli_connections(&["NAME", "TYPE", "DEVICE", "AUTOCONNECT"], false).await;
    Json(json!({"connections": configured_connections_from(&rows)})).into_response()
}

/// Reshape parsed `nmcli connection show` terse rows into the saved-profile list,
/// mirroring the Python `configured_connections()`: keep only rows whose TYPE
/// contains `wireless`; for each, emit `{name, type, device, autoconnect}` with a
/// blank device coerced to null and the autoconnect flag derived from a
/// case-insensitive `yes`. Split out so the filter + the field mapping are unit
/// tested without the `nmcli` IO.
fn configured_connections_from(rows: &[Vec<String>]) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for row in rows {
        let name = row.first().map(String::as_str).unwrap_or("");
        let ctype = row.get(1).map(String::as_str).unwrap_or("");
        let device = row.get(2).map(String::as_str).unwrap_or("");
        let autoconnect = row.get(3).map(String::as_str).unwrap_or("");
        if !ctype.contains("wireless") {
            continue;
        }
        out.push(json!({
            "name": name,
            "type": ctype,
            "device": if device.is_empty() { Value::Null } else { Value::String(device.to_string()) },
            "autoconnect": autoconnect.trim().eq_ignore_ascii_case("yes"),
        }));
    }
    Value::Array(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_status_is_unknown_when_the_socket_is_down() {
        // No wifi_status reply (the daemon socket unreachable): the state is
        // unknown, never a measured "not connected".
        assert_eq!(client_status_view(None), None);
    }

    #[test]
    fn client_status_reshapes_a_status_reply() {
        // A wifi_status reply carries the daemon status() shape plus an `ok`
        // flag; the view drops `ok` and keeps the seven contract keys verbatim.
        let reply: serde_json::Map<String, Value> = serde_json::from_value(json!({
            "ok": true,
            "connected": true,
            "ssid": "BenchNet",
            "bssid": "AA:BB:CC:DD:EE:FF",
            "signal": 72,
            "ip": "192.168.7.42",
            "gateway": "192.168.7.1",
            "security": "WPA2",
        }))
        .unwrap();
        let v = client_status_view(Some(reply)).unwrap();
        let want = json!({
            "connected": true,
            "ssid": "BenchNet",
            "bssid": "AA:BB:CC:DD:EE:FF",
            "signal": 72,
            "ip": "192.168.7.42",
            "gateway": "192.168.7.1",
            "security": "WPA2",
        });
        assert_eq!(v, want);
        // `ok` is never echoed back to the client.
        assert!(v.as_object().unwrap().get("ok").is_none());
    }

    #[test]
    fn client_status_disconnected_reply_keeps_the_full_shape() {
        // A connected-false reply (the daemon reporting no active station) still
        // carries every key; the nulls flow through unchanged.
        let reply: serde_json::Map<String, Value> = serde_json::from_value(json!({
            "ok": true,
            "connected": false,
            "ssid": null,
            "bssid": null,
            "signal": null,
            "ip": null,
            "gateway": null,
            "security": null,
        }))
        .unwrap();
        let v = client_status_view(Some(reply)).unwrap();
        assert_eq!(v["connected"], json!(false));
        assert_eq!(v["ssid"], Value::Null);
        assert_eq!(v["security"], Value::Null);
    }

    #[test]
    fn configured_keeps_only_wireless_rows_and_maps_the_fields() {
        // The terse rows are NAME,TYPE,DEVICE,AUTOCONNECT. Only the
        // *-wireless types survive; the ethernet/loopback rows drop. A blank
        // device coerces to null; the autoconnect flag is a case-insensitive
        // `yes`.
        let rows = vec![
            vec![
                "BenchNet".to_string(),
                "802-11-wireless".to_string(),
                "wlan0".to_string(),
                "yes".to_string(),
            ],
            vec![
                "Wired connection 1".to_string(),
                "802-3-ethernet".to_string(),
                "eth0".to_string(),
                "yes".to_string(),
            ],
            vec![
                "SavedHotspot".to_string(),
                "802-11-wireless".to_string(),
                String::new(),
                "no".to_string(),
            ],
        ];
        let v = configured_connections_from(&rows);
        let want = json!([
            {
                "name": "BenchNet",
                "type": "802-11-wireless",
                "device": "wlan0",
                "autoconnect": true,
            },
            {
                "name": "SavedHotspot",
                "type": "802-11-wireless",
                "device": null,
                "autoconnect": false,
            },
        ]);
        assert_eq!(v, want);
    }

    #[test]
    fn configured_empty_rows_yield_the_empty_list() {
        // No rows (an absent / failing nmcli) → the empty list, matching the
        // Python route's degrade.
        assert_eq!(configured_connections_from(&[]), json!([]));
    }

    #[test]
    fn configured_autoconnect_is_case_insensitive_and_trimmed() {
        let rows = vec![vec![
            "N".to_string(),
            "802-11-wireless".to_string(),
            "wlan0".to_string(),
            " YES ".to_string(),
        ]];
        let v = configured_connections_from(&rows);
        assert_eq!(v[0]["autoconnect"], json!(true));
    }
}
