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
//! {"op":"wifi_scan"}
//!     -> {"ok":true,"networks":[{"ssid":"Net","bssid":"..","signal":-48,"frequency_mhz":2437,"channel":6,"security":"wpa2"}]}
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
//! {"op":"share_uplink","enabled":true}
//!     -> {"ok":true,"applied":true,"backend":"nftables","apply_error":null}
//! ```
//!
//! A failed apply replies with `ok:false` and the manager's `error`, so the REST
//! layer can surface it. The socket mutates the live managers it shares with the
//! daemon; the AP / WiFi-client ops drive the radio, the Ethernet / modem ops the
//! profiles, the share-uplink op the NAT firewall on the active uplink, and the
//! modem op only PERSISTS the sidecar (the daemon's poll loop reconciles the live
//! session) — the REST layer owns the config-file persistence the AP
//! channel/ssid + the share-uplink flag need.

use std::path::Path;
use std::sync::Arc;

use ados_protocol::ipc::{bind_command_socket, serve_rpc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::firewall::ShareUplinkFirewall;
use crate::managers::{EthernetManager, HostapdManager, ModemManager, WifiClientManager};
use crate::router::UplinkRouter;

mod apply;
mod parse;
#[cfg(test)]
use apply::{ap_view, with_ok};
use apply::{apply, apply_wifi};
use parse::{parse_command, Command, Parsed};

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
    /// The share-uplink firewall the daemon's uplink-switch consumer also
    /// drives; drives `share_uplink`. Its applies are serialized internally.
    pub firewall: Arc<ShareUplinkFirewall>,
    /// The live uplink router, for the active iface `share_uplink` applies on.
    pub router: Arc<UplinkRouter>,
}

/// Bind the command socket and serve requests until the listener errors. Run as
/// its own task from the service main loop. The shared helper removes a stale
/// socket first and chmods it 0660 (root-owned; the api service runs as root on
/// target). Each connection is one newline-terminated JSON request → one
/// newline-terminated JSON response, then close (the radio command socket's
/// framing). Returns only on a bind error.
pub async fn serve(state: CmdState, sock_path: &Path) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "wifi command socket listening");

    serve_rpc(listener, MAX_REQUEST_BYTES, move |req: Vec<u8>| {
        let state = state.clone();
        async move {
            let resp = dispatch(&req, &state).await;
            serde_json::to_vec(&resp)
                .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
        }
    })
    .await;
    Ok(())
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

/// Serve ONLY the Wi-Fi-client ops on `sock_path`, for a node that runs no
/// uplink daemon (a drone). The REST layer talks to the same socket with the
/// same wire protocol on every profile; here the one Wi-Fi-client manager behind
/// it is owned by the caller's process. The AP, Ethernet and modem ops have no
/// manager on such a node and reply `ok:false` with `E_UNSUPPORTED_OP`.
pub async fn serve_wifi_client(
    wifi: Arc<Mutex<WifiClientManager>>,
    sock_path: &Path,
) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "wifi client command socket listening");
    serve_rpc(listener, MAX_REQUEST_BYTES, move |req: Vec<u8>| {
        let wifi = Arc::clone(&wifi);
        async move {
            let resp = dispatch_wifi_client(&req, &wifi).await;
            serde_json::to_vec(&resp)
                .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
        }
    })
    .await;
    Ok(())
}

/// Parse + route one request against a lone Wi-Fi-client manager.
async fn dispatch_wifi_client(line: &[u8], wifi: &Mutex<WifiClientManager>) -> Value {
    match parse_command(line) {
        Parsed::Reply(v) => v,
        Parsed::Cmd(cmd) => match apply_wifi(cmd, wifi).await {
            Ok(v) => v,
            Err(_) => {
                json!({"ok": false, "error": "E_UNSUPPORTED_OP: no uplink daemon on this node"})
            }
        },
    }
}

#[cfg(test)]
mod tests;
