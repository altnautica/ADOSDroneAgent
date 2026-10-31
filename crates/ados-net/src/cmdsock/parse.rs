//! Request decoding and field validation for the command socket.

use super::*;

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

/// A parsed + field-validated request, ready to apply. Parsing this OUT of the
/// raw bytes is pure (no manager access), so a malformed request is rejected
/// before the service ever locks a manager.
#[derive(Debug, PartialEq)]
pub(super) enum Command {
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
    /// Scan for nearby networks on the station radio.
    Scan,
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
    /// Apply (or remove) the share-uplink NAT on the active uplink now.
    ShareUplink {
        enabled: bool,
    },
}

/// The outcome of parsing a request line: an apply-ready [`Command`], or a
/// terminal response for a malformed/unknown request (so the caller can reply
/// without touching a manager).
pub(super) enum Parsed {
    Cmd(Command),
    Reply(Value),
}

/// Parse + field-validate one request line. Pure: no manager access, no I/O,
/// fully unit-testable. A bad-JSON / missing-field / unknown-op request resolves
/// to a terminal [`Parsed::Reply`]; a well-formed request resolves to a command.
pub(super) fn parse_command(line: &[u8]) -> Parsed {
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
        "wifi_scan" => Parsed::Cmd(Command::Scan),
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
        "share_uplink" => match req.enabled {
            Some(enabled) => Parsed::Cmd(Command::ShareUplink { enabled }),
            None => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_ENABLED"})),
        },
        other => Parsed::Reply(json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")})),
    }
}
