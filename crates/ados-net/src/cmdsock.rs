//! Operator command socket for the ground-station uplink matrix.
//!
//! Joining / forgetting an upstream WiFi network, toggling its autoconnect flag,
//! reconfiguring the AP, the Ethernet IPv4 profile, or the cellular modem are
//! operator on-demand actions the REST layer drives. When the native `ados-net`
//! daemon is the running uplink owner the REST handler has no in-process Python
//! manager to call (and must NOT drive `nmcli` on `wlan0`, `hostapd`, or the
//! Ethernet/modem profiles itself, or it would race the daemon's own managers for
//! the radio + the live link). So it forwards each action to this socket instead;
//! the running service applies it through the SAME live managers the daemon owns,
//! and replies with the manager-truth view.
//!
//! Wire protocol (mirrors the radio command socket): one newline-terminated JSON
//! request, one newline-terminated JSON response per connection, then close.
//!
//! ```text
//! {"op":"wifi_join","ssid":"Net","passphrase":"secret","force":false}
//!     -> {"ok":true,"joined":true,"ip":"...","gateway":"...","error":null}
//! {"op":"wifi_forget","name":"Net"}
//!     -> {"ok":true,"forgot":true,"name":"Net","error":null}
//! {"op":"wifi_leave"}
//!     -> {"ok":true,"left":true,"previous_ssid":"Net"}
//! {"op":"wifi_status"}
//!     -> {"ok":true,"connected":true,"ssid":"Net","ip":"...",...}
//! {"op":"wifi_autoconnect","name":"Net","enabled":true}
//!     -> {"ok":true,"autoconnect":true,"name":"Net","error":null}
//! {"op":"ap_config","ssid":"ADOS-GS-1234","passphrase":"pw","channel":6,"enabled":true}
//!     -> {"ok":true,"enabled":true,"running":true,"ssid":"...","channel":6,...}
//! {"op":"eth_config","mode":"static","ip":"10.0.0.5/24","gateway":"10.0.0.1","dns":["8.8.8.8"]}
//!     -> {"ok":true,"mode":"static","connection_name":"Wired","ip":"...",...}
//! {"op":"eth_config","mode":"dhcp"}
//!     -> {"ok":true,"mode":"dhcp",...}     (a no_ethernet_connection apply -> ok:false)
//! {"op":"modem_config","apn":"internet","cap_gb":5.0,"enabled":true}
//!     -> {"ok":true,"apn":"internet","cap_gb":5.0,"enabled":true}
//! ```
//!
//! A failed apply replies with `ok:false` and the manager's `error`, so the REST
//! layer can surface it. The socket mutates the live managers it shares with the
//! daemon; the AP / WiFi-client ops drive the radio, the Ethernet / modem ops the
//! profiles, and the modem op only PERSISTS the sidecar (the daemon's poll loop
//! reconciles the live session) — the REST layer owns the config-file persistence
//! the AP channel/ssid + the share-uplink flag need.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::managers::{EthernetManager, HostapdManager, ModemManager, WifiClientManager};

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared live managers the command handlers drive. Each is the SAME instance
/// the daemon's run loop owns (or, for the WiFi-client + Ethernet ops, a peer that
/// shares the real system state through the `wlan0` advisory file lock + nmcli),
/// so an op drives the live system, not a parallel copy. Constructed once at
/// service start and shared with every accepted connection.
#[derive(Clone)]
pub struct CmdState {
    /// The WiFi-client manager (owner of the `wlan0` AP/STA advisory lock); drives
    /// `wifi_join` / `wifi_forget` / `wifi_leave` / `wifi_status` /
    /// `wifi_autoconnect`.
    pub wifi: Arc<Mutex<WifiClientManager>>,
    /// The live hostapd AP manager the daemon brought up; drives `ap_config`.
    pub hostapd: Arc<Mutex<HostapdManager>>,
    /// The Ethernet manager (stateless nmcli apply peer); drives `eth_config`.
    pub ethernet: Arc<EthernetManager>,
    /// The live cellular modem manager the daemon owns; drives `modem_config`
    /// (persist only — the daemon's poll loop reconciles the live session).
    pub modem: Arc<ModemManager>,
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    ssid: Option<String>,
    #[serde(default)]
    passphrase: Option<String>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    name: Option<String>,
    // ap_config + wifi_autoconnect + modem_config fields. All optional so a
    // missing field is None and parsing stays tolerant; the per-op parse arm
    // enforces what each op requires.
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    channel: Option<u32>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    gateway: Option<String>,
    #[serde(default)]
    dns: Option<Vec<String>>,
    #[serde(default)]
    apn: Option<String>,
    #[serde(default)]
    cap_gb: Option<f64>,
}

/// Bind the command socket and serve requests until the listener errors. Run as
/// its own task from the service main loop. Removes a stale socket first and
/// chmods it 0660 (root-owned; the api service runs as root on target). Returns
/// only on a bind error; the accept loop never exits on the happy path.
pub async fn serve(state: CmdState, sock_path: &Path) -> std::io::Result<()> {
    // A stale socket from a prior run makes bind() fail with EADDRINUSE.
    let _ = std::fs::remove_file(sock_path);
    if let Some(parent) = sock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(sock_path)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o660));
    }
    tracing::info!(path = %sock_path.display(), "wifi command socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, state).await {
                        tracing::debug!(error = %e, "wifi command conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "wifi command accept failed");
                // Brief backoff so a persistent accept error can't hot-spin.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Read one newline-terminated request, dispatch it, write one newline-
/// terminated response. Matches the radio command socket's framing.
async fn handle_conn(mut stream: UnixStream, state: CmdState) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break; // EOF before newline — dispatch whatever we have.
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') || buf.len() > MAX_REQUEST_BYTES {
            break;
        }
    }
    let line = match buf.iter().position(|&b| b == b'\n') {
        Some(i) => &buf[..i],
        None => &buf[..],
    };
    let resp = dispatch(line, &state).await;
    let mut body = serde_json::to_vec(&resp)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec());
    body.push(b'\n');
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

/// A parsed + field-validated request, ready to apply. Parsing this OUT of the
/// raw bytes is pure (no manager access), so a malformed request is rejected
/// before the service ever locks a manager.
#[derive(Debug, PartialEq)]
enum Command {
    Join {
        ssid: String,
        passphrase: Option<String>,
        force: bool,
    },
    Forget {
        name: String,
    },
    Leave,
    Status,
    /// Toggle the NM autoconnect flag of a saved WiFi profile.
    Autoconnect {
        name: String,
        enabled: bool,
    },
    /// Reconfigure the AP. Every field is optional (the manager applies only the
    /// supplied ones); `enabled` is the start/stop hint.
    ApConfig {
        ssid: Option<String>,
        passphrase: Option<String>,
        channel: Option<u32>,
        enabled: Option<bool>,
    },
    /// Reconfigure the Ethernet IPv4 profile. `static` carries ip + gateway + dns;
    /// `dhcp` carries nothing.
    EthStatic {
        ip: String,
        gateway: String,
        dns: Vec<String>,
    },
    EthDhcp,
    /// Persist the modem config sidecar. The daemon's poll loop reconciles the
    /// live session from the persisted file.
    ModemConfig {
        apn: Option<String>,
        cap_gb: Option<f64>,
        enabled: Option<bool>,
    },
}

/// The outcome of parsing a request line: an apply-ready [`Command`], or a
/// terminal response for a malformed/unknown request (so the caller can reply
/// without touching a manager).
enum Parsed {
    Cmd(Command),
    Reply(Value),
}

/// Parse + field-validate one request line. Pure: no manager access, no I/O,
/// fully unit-testable. A bad-JSON / missing-field / unknown-op request resolves
/// to a terminal [`Parsed::Reply`]; a well-formed request resolves to a command.
fn parse_command(line: &[u8]) -> Parsed {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            return Parsed::Reply(json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}))
        }
    };
    match req.op.as_str() {
        "wifi_join" => match req.ssid {
            Some(ssid) if !ssid.is_empty() => Parsed::Cmd(Command::Join {
                ssid,
                passphrase: req.passphrase.filter(|p| !p.is_empty()),
                force: req.force,
            }),
            _ => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_SSID"})),
        },
        "wifi_forget" => match req.name {
            Some(name) if !name.is_empty() => Parsed::Cmd(Command::Forget { name }),
            _ => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_NAME"})),
        },
        "wifi_leave" => Parsed::Cmd(Command::Leave),
        "wifi_status" => Parsed::Cmd(Command::Status),
        "wifi_autoconnect" => match (req.name, req.enabled) {
            // The manager treats an empty name as `name_required`, so the empty
            // case is forwarded (it is a normal manager result, not a transport
            // error) — but a request that omits `name` entirely is malformed.
            (Some(name), Some(enabled)) => Parsed::Cmd(Command::Autoconnect { name, enabled }),
            (_, None) => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_ENABLED"})),
            (None, _) => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_NAME"})),
        },
        "ap_config" => Parsed::Cmd(Command::ApConfig {
            ssid: req.ssid.filter(|s| !s.is_empty()),
            passphrase: req.passphrase.filter(|p| !p.is_empty()),
            channel: req.channel,
            enabled: req.enabled,
        }),
        "eth_config" => match req.mode.as_deref() {
            Some("static") => Parsed::Cmd(Command::EthStatic {
                ip: req.ip.unwrap_or_default(),
                gateway: req.gateway.unwrap_or_default(),
                dns: req.dns.unwrap_or_default(),
            }),
            // Any non-"static" mode is dhcp, matching the Python `else` branch.
            Some(_) | None => Parsed::Cmd(Command::EthDhcp),
        },
        "modem_config" => Parsed::Cmd(Command::ModemConfig {
            apn: req.apn,
            cap_gb: req.cap_gb,
            enabled: req.enabled,
        }),
        other => Parsed::Reply(json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")})),
    }
}

/// Parse + route one request to the relevant manager. The parse half is pure
/// (covered by the `parse_command` tests); the apply half locks/drives a manager
/// (nmcli / hostapd / sidecar I/O), so it is covered on-rig + by the managers'
/// own tests.
async fn dispatch(line: &[u8], state: &CmdState) -> Value {
    let cmd = match parse_command(line) {
        Parsed::Cmd(c) => c,
        Parsed::Reply(v) => return v,
    };
    apply(cmd, state).await
}

/// Apply a validated command to the live managers. The `ok` flag is derived from
/// the manager's own result so the REST forward client can branch on it: a
/// manager result object replies `ok:true` (its own success field carries the
/// outcome the REST layer inspects); only a non-object manager result yields
/// `ok:false`.
async fn apply(cmd: Command, state: &CmdState) -> Value {
    match cmd {
        Command::Join {
            ssid,
            passphrase,
            force,
        } => {
            let res = state
                .wifi
                .lock()
                .await
                .join(&ssid, passphrase.as_deref(), force)
                .await;
            with_ok(res)
        }
        Command::Forget { name } => {
            let res = state.wifi.lock().await.forget(&name).await;
            with_ok(res)
        }
        Command::Leave => {
            let res = state.wifi.lock().await.leave().await;
            with_ok(res)
        }
        Command::Status => {
            let st: Map<String, Value> = state.wifi.lock().await.status().await;
            let mut v = Value::Object(st);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("ok".to_string(), json!(true));
            }
            v
        }
        Command::Autoconnect { name, enabled } => {
            let res = state
                .wifi
                .lock()
                .await
                .set_autoconnect(&name, enabled)
                .await;
            with_ok(res)
        }
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
        } => {
            let res = state.modem.configure(apn.as_deref(), cap_gb, enabled).await;
            with_ok(res)
        }
    }
}

/// Apply an AP-config change to the live hostapd manager and return the AP view.
///
/// Mirrors the Python AP PUT side-effect order: `apply_ap_config(ssid, passphrase,
/// channel)`, then the `enabled` start/stop hint (start when asked-on and not
/// running, stop when asked-off and running, leave it alone otherwise), then the
/// live `status()` reshaped into the `_ap_view` body (`enabled` mirrors
/// `running`). A failed `apply_ap_config` replies `ok:false` with the apply error
/// so the REST layer surfaces the FastAPI `E_AP_APPLY_FAILED` 500. The config-file
/// persist (`network.hotspot.channel`/`ssid`) is the REST layer's job, not the
/// daemon's, so it is not done here.
async fn apply_ap_config(
    state: &CmdState,
    ssid: Option<String>,
    passphrase: Option<String>,
    channel: Option<u32>,
    enabled: Option<bool>,
) -> Value {
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
/// connected_clients}`; the view adds `enabled` (mirroring `running`) and stamps
/// the transport `ok:true`. Mirrors the Python `_ap_view` live branch.
fn ap_view(status: &Value) -> Value {
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
fn with_ok(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("ok".to_string(), json!(true));
        v
    } else {
        json!({"ok": false, "error": "E_BAD_MANAGER_RESULT"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(line: &[u8]) -> Value {
        match parse_command(line) {
            Parsed::Reply(v) => v,
            Parsed::Cmd(c) => panic!("expected an early reply, got command {c:?}"),
        }
    }

    fn cmd(line: &[u8]) -> Command {
        match parse_command(line) {
            Parsed::Cmd(c) => c,
            Parsed::Reply(v) => panic!("expected a command, got reply {v}"),
        }
    }

    #[test]
    fn bad_json_is_rejected_before_any_manager_access() {
        let v = reply(b"not json");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
    }

    #[test]
    fn unknown_op_is_rejected() {
        let v = reply(br#"{"op":"frob"}"#);
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_UNKNOWN_OP"));
    }

    #[test]
    fn join_requires_a_nonempty_ssid() {
        assert_eq!(reply(br#"{"op":"wifi_join"}"#)["error"], "E_MISSING_SSID");
        assert_eq!(
            reply(br#"{"op":"wifi_join","ssid":""}"#)["error"],
            "E_MISSING_SSID"
        );
        // An open network (no passphrase) parses; an empty passphrase normalises
        // to None so the manager treats it as open, not as a blank key.
        assert_eq!(
            cmd(br#"{"op":"wifi_join","ssid":"Open"}"#),
            Command::Join {
                ssid: "Open".to_string(),
                passphrase: None,
                force: false,
            }
        );
        assert_eq!(
            cmd(br#"{"op":"wifi_join","ssid":"Net","passphrase":"","force":true}"#),
            Command::Join {
                ssid: "Net".to_string(),
                passphrase: None,
                force: true,
            }
        );
        assert_eq!(
            cmd(br#"{"op":"wifi_join","ssid":"Net","passphrase":"pw"}"#),
            Command::Join {
                ssid: "Net".to_string(),
                passphrase: Some("pw".to_string()),
                force: false,
            }
        );
    }

    #[test]
    fn forget_requires_a_nonempty_name() {
        assert_eq!(reply(br#"{"op":"wifi_forget"}"#)["error"], "E_MISSING_NAME");
        assert_eq!(
            reply(br#"{"op":"wifi_forget","name":""}"#)["error"],
            "E_MISSING_NAME"
        );
        assert_eq!(
            cmd(br#"{"op":"wifi_forget","name":"Net"}"#),
            Command::Forget {
                name: "Net".to_string()
            }
        );
    }

    #[test]
    fn leave_and_status_parse_with_no_fields() {
        assert_eq!(cmd(br#"{"op":"wifi_leave"}"#), Command::Leave);
        assert_eq!(cmd(br#"{"op":"wifi_status"}"#), Command::Status);
    }

    // ── wifi_autoconnect parse ───────────────────────────────────────────────

    #[test]
    fn autoconnect_requires_name_and_enabled() {
        // Missing `enabled` is malformed.
        assert_eq!(
            reply(br#"{"op":"wifi_autoconnect","name":"Net"}"#)["error"],
            "E_MISSING_ENABLED"
        );
        // Missing `name` is malformed.
        assert_eq!(
            reply(br#"{"op":"wifi_autoconnect","enabled":true}"#)["error"],
            "E_MISSING_NAME"
        );
        // An empty name is forwarded (the manager replies name_required).
        assert_eq!(
            cmd(br#"{"op":"wifi_autoconnect","name":"","enabled":false}"#),
            Command::Autoconnect {
                name: "".to_string(),
                enabled: false,
            }
        );
        assert_eq!(
            cmd(br#"{"op":"wifi_autoconnect","name":"HomeNet","enabled":true}"#),
            Command::Autoconnect {
                name: "HomeNet".to_string(),
                enabled: true,
            }
        );
    }

    // ── ap_config parse ──────────────────────────────────────────────────────

    #[test]
    fn ap_config_parses_optional_fields_normalising_blanks() {
        // An all-fields request.
        assert_eq!(
            cmd(br#"{"op":"ap_config","ssid":"ADOS-GS-1234","passphrase":"pw","channel":11,"enabled":true}"#),
            Command::ApConfig {
                ssid: Some("ADOS-GS-1234".to_string()),
                passphrase: Some("pw".to_string()),
                channel: Some(11),
                enabled: Some(true),
            }
        );
        // Blank ssid + passphrase normalise to None (the manager leaves them
        // untouched), and the absent fields are None.
        assert_eq!(
            cmd(br#"{"op":"ap_config","ssid":"","passphrase":""}"#),
            Command::ApConfig {
                ssid: None,
                passphrase: None,
                channel: None,
                enabled: None,
            }
        );
    }

    // ── eth_config parse ─────────────────────────────────────────────────────

    #[test]
    fn eth_config_static_carries_ip_gateway_dns() {
        assert_eq!(
            cmd(br#"{"op":"eth_config","mode":"static","ip":"10.0.0.5/24","gateway":"10.0.0.1","dns":["8.8.8.8","1.1.1.1"]}"#),
            Command::EthStatic {
                ip: "10.0.0.5/24".to_string(),
                gateway: "10.0.0.1".to_string(),
                dns: vec!["8.8.8.8".to_string(), "1.1.1.1".to_string()],
            }
        );
        // Missing ip/gateway/dns default to empty (the REST layer rejects the
        // missing-fields case before forwarding, but the parse stays tolerant).
        assert_eq!(
            cmd(br#"{"op":"eth_config","mode":"static"}"#),
            Command::EthStatic {
                ip: "".to_string(),
                gateway: "".to_string(),
                dns: Vec::new(),
            }
        );
    }

    #[test]
    fn eth_config_dhcp_and_any_other_mode_is_dhcp() {
        assert_eq!(
            cmd(br#"{"op":"eth_config","mode":"dhcp"}"#),
            Command::EthDhcp
        );
        // No mode → dhcp (matches the Python `else`).
        assert_eq!(cmd(br#"{"op":"eth_config"}"#), Command::EthDhcp);
        // An unrecognised mode → dhcp too.
        assert_eq!(
            cmd(br#"{"op":"eth_config","mode":"weird"}"#),
            Command::EthDhcp
        );
    }

    // ── modem_config parse ───────────────────────────────────────────────────

    #[test]
    fn modem_config_parses_optional_fields() {
        assert_eq!(
            cmd(br#"{"op":"modem_config","apn":"internet","cap_gb":5.0,"enabled":true}"#),
            Command::ModemConfig {
                apn: Some("internet".to_string()),
                cap_gb: Some(5.0),
                enabled: Some(true),
            }
        );
        assert_eq!(
            cmd(br#"{"op":"modem_config"}"#),
            Command::ModemConfig {
                apn: None,
                cap_gb: None,
                enabled: None,
            }
        );
    }

    // ── view shapers ─────────────────────────────────────────────────────────

    #[test]
    fn ap_view_reshapes_status_into_the_view_body() {
        let status = json!({
            "running": true,
            "ssid": "ADOS-GS-1234",
            "channel": 6,
            "interface": "wlan0",
            "gateway": "192.168.4.1",
            "connected_clients": ["aa:bb:cc:dd:ee:ff"],
        });
        assert_eq!(
            ap_view(&status),
            json!({
                "ok": true,
                "enabled": true,
                "running": true,
                "ssid": "ADOS-GS-1234",
                "channel": 6,
                "interface": "wlan0",
                "gateway": "192.168.4.1",
                "connected_clients": ["aa:bb:cc:dd:ee:ff"],
            })
        );
        // A not-running status reports enabled=false and a null gateway, the empty
        // client list defaulting when absent.
        let down = json!({
            "running": false,
            "ssid": "ADOS-GS-1234",
            "channel": 6,
            "interface": "wlan0",
            "gateway": Value::Null,
            "connected_clients": [],
        });
        let v = ap_view(&down);
        assert_eq!(v["enabled"], json!(false));
        assert_eq!(v["running"], json!(false));
        assert_eq!(v["gateway"], Value::Null);
        assert_eq!(v["connected_clients"], json!([]));
    }

    #[test]
    fn with_ok_marks_a_processed_result_ok_and_preserves_fields() {
        // A processed command is ok:true regardless of the join outcome — the
        // `joined` field carries success, which the REST layer inspects.
        let joined = with_ok(json!({"joined": true, "ip": "1.2.3.4"}));
        assert_eq!(joined["ok"], true);
        assert_eq!(joined["ip"], "1.2.3.4");
        let failed = with_ok(json!({"joined": false, "error": "wlan0_busy_ap_active"}));
        assert_eq!(failed["ok"], true);
        assert_eq!(failed["error"], "wlan0_busy_ap_active");
        // A non-object result is a hard transport error.
        assert_eq!(with_ok(json!("oops"))["ok"], false);
    }

    #[test]
    fn an_empty_line_is_a_bad_request_not_a_panic() {
        let v = reply(b"");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
    }
}
