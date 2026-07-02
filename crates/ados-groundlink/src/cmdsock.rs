//! Operator command socket for the ground-station data-plane.
//!
//! Role transitions and WFB pair-key install / unpair are operator on-demand
//! actions the REST layer drives. When the native front owns the LAN port it has
//! no in-process Python pair/role manager to call, so it forwards each action to
//! this socket; the running `ados-groundlink` service applies it (it owns the
//! receive plane, masks/unmasks the role units, and restarts its own
//! `ados-wfb-rx` unit to pick up a fresh key).
//!
//! Wire protocol (mirrors the radio + Wi-Fi command sockets): one
//! newline-terminated JSON request, one newline-terminated JSON reply per
//! connection, then close.
//!
//! ```text
//! {"op":"set_role","role":"relay","reason":"rest"}
//!     -> {"ok":true,"role":"relay","previous":"direct","units_started":[...],
//!         "units_stopped":[...],"ts_ms":1234,"noop":false}
//! {"op":"set_gateway_preference","mode":"pinned","pinned_mac":"aa:bb:..."}
//!     -> {"ok":true,"mode":"pinned","pinned_mac":"aa:bb:...","persisted":true}
//!     -> {"ok":false,"error":"E_BATCTL_UNAVAILABLE"}   (batctl missing)
//! {"op":"pair_keypair","blob_b64":"...","peer_device_id":null}
//!     -> {"ok":true,"paired":true,"paired_with_device_id":null,
//!         "paired_at":"...","fingerprint":"...","role":"gs"}
//!     -> {"ok":false,"error":"E_INVALID_KEY_BLOB"}     (bad blob)
//! {"op":"unpair"}
//!     -> {"ok":true,"paired":false,"role":"gs"}
//! ```
//!
//! `ok:false` carries the apply-time error code so the REST layer can map it to a
//! 4xx/5xx. A parse / encode failure yields a transport `ok:false`. The role + the
//! gateway preference are stateless file+systemctl operations, so the socket
//! holds no manager instances; it dispatches each op directly.

use std::path::Path;

use ados_protocol::ipc::{bind_command_socket, serve_rpc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mesh::role_apply;
use crate::pair_state;

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    pinned_mac: Option<String>,
    #[serde(default)]
    blob_b64: Option<String>,
    #[serde(default)]
    peer_device_id: Option<String>,
}

/// Bind the command socket and serve requests until the listener errors. Run as
/// its own task from the service main loop. The shared helper removes a stale
/// socket first and chmods it 0660 (root-owned; the api service runs as root on
/// target). Each connection is one newline-terminated JSON request → one
/// newline-terminated JSON response, then close (the radio + Wi-Fi command
/// sockets' framing). Returns only on a bind error.
pub async fn serve(sock_path: &Path) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "groundlink command socket listening");

    serve_rpc(listener, MAX_REQUEST_BYTES, |req: Vec<u8>| async move {
        let resp = dispatch(&req).await;
        serde_json::to_vec(&resp).unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
    })
    .await;
    Ok(())
}

/// A parsed + field-validated command, ready to apply. Parsing this OUT of the
/// raw bytes is pure (no I/O), so a malformed request is rejected before any
/// side effect.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    SetRole {
        role: String,
        reason: String,
    },
    SetGatewayPreference {
        mode: String,
        pinned_mac: Option<String>,
    },
    PairKeypair {
        blob_b64: String,
        peer_device_id: Option<String>,
    },
    Unpair,
}

/// The outcome of parsing a request line: an apply-ready [`Command`], or a
/// terminal reply for a malformed/unknown request.
enum Parsed {
    Cmd(Command),
    Reply(Value),
}

/// Parse + field-validate one request line. Pure: no I/O, fully unit-testable. A
/// bad-JSON / missing-field / unknown-op request resolves to a terminal reply.
fn parse_command(line: &[u8]) -> Parsed {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            return Parsed::Reply(json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}))
        }
    };
    match req.op.as_str() {
        "set_role" => match req.role {
            Some(role) if !role.is_empty() => Parsed::Cmd(Command::SetRole {
                role,
                reason: req.reason.unwrap_or_else(|| "rest".to_string()),
            }),
            _ => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_ROLE"})),
        },
        "set_gateway_preference" => match req.mode {
            Some(mode) if matches!(mode.as_str(), "auto" | "pinned" | "off") => {
                Parsed::Cmd(Command::SetGatewayPreference {
                    mode,
                    pinned_mac: req.pinned_mac.filter(|m| !m.is_empty()),
                })
            }
            _ => Parsed::Reply(json!({"ok": false, "error": "E_INVALID_MODE"})),
        },
        "pair_keypair" => match req.blob_b64 {
            Some(blob_b64) if !blob_b64.is_empty() => Parsed::Cmd(Command::PairKeypair {
                blob_b64,
                peer_device_id: req.peer_device_id.filter(|p| !p.is_empty()),
            }),
            _ => Parsed::Reply(json!({"ok": false, "error": "E_BLOB_REQUIRED"})),
        },
        "unpair" => Parsed::Cmd(Command::Unpair),
        other => Parsed::Reply(json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")})),
    }
}

/// Parse + route one request. The parse half is pure (covered by the
/// `parse_command` tests); the apply half does file + systemctl work, covered
/// on-rig + by the apply-path unit tests.
async fn dispatch(line: &[u8]) -> Value {
    match parse_command(line) {
        Parsed::Cmd(c) => apply(c).await,
        Parsed::Reply(v) => v,
    }
}

/// Apply a validated command and return the reply object (with the transport
/// `ok` flag stamped). The reply body matches the field shape the REST route
/// projects, so the front strips the `ok` flag and returns the rest verbatim.
async fn apply(cmd: Command) -> Value {
    match cmd {
        Command::SetRole { role, reason } => match role_apply::apply_role(&role, &reason).await {
            Ok(res) => json!({
                "ok": true,
                "role": res.role,
                "previous": res.previous,
                "units_started": res.units_started,
                "units_stopped": res.units_stopped,
                "ts_ms": res.ts_ms,
                "noop": res.noop,
            }),
            // The role gate (capability / paired) is enforced in the route before
            // the forward; the only failure here is an unknown role, which the
            // route also pre-validates — so this is a belt-and-suspenders 400.
            Err(bad) => {
                json!({"ok": false, "error": "E_INVALID_ROLE", "message": format!("unknown role: {bad}")})
            }
        },
        Command::SetGatewayPreference { mode, pinned_mac } => {
            apply_gateway_preference(&mode, pinned_mac.as_deref()).await
        }
        Command::PairKeypair {
            blob_b64,
            peer_device_id,
        } => match pair_state::apply_keypair_gs(&blob_b64, peer_device_id.as_deref()).await {
            Ok(reply) => {
                let mut v = reply;
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("ok".to_string(), json!(true));
                }
                v
            }
            Err(pair_state::PairError::BadBase64(msg)) => {
                json!({"ok": false, "error": "E_BLOB_BASE64", "message": msg})
            }
            Err(pair_state::PairError::BadBlob(msg)) => {
                json!({"ok": false, "error": "E_INVALID_KEY_BLOB", "message": msg})
            }
            Err(pair_state::PairError::Io(msg)) => {
                json!({"ok": false, "error": "E_PAIR_FAILED", "message": msg})
            }
        },
        Command::Unpair => match pair_state::unpair_gs().await {
            Ok(reply) => {
                let mut v = reply;
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("ok".to_string(), json!(true));
                }
                v
            }
            Err(msg) => json!({"ok": false, "error": "E_UNPAIR_FAILED", "message": msg}),
        },
    }
}

/// Persist the gateway preference + apply the batman gateway mode. Mirrors the
/// FastAPI `put_gateway_preference` body: write `/etc/ados/mesh/gateway.json`
/// (atomic, sorted keys) then `batctl gw_mode off|client` (+ `gw_sel <mac>` when
/// pinned). The persist runs first; a persist failure still applies but surfaces
/// `persist_error`. A missing `batctl` (the spawn fails) maps to
/// `E_BATCTL_UNAVAILABLE`.
async fn apply_gateway_preference(mode: &str, pinned_mac: Option<&str>) -> Value {
    use std::time::Duration;

    use crate::mesh::batctl;
    use crate::paths::MESH_GATEWAY_JSON;

    // Persist first. A write failure is surfaced but does not abort the apply
    // (some kernels have /etc read-only briefly on upgrade).
    let persist_error = write_gateway_json(Path::new(MESH_GATEWAY_JSON), mode, pinned_mac).err();

    // Apply via batctl. The Python catches `FileNotFoundError` from the spawn and
    // raises 503 E_BATCTL_UNAVAILABLE; the batctl wrapper returns rc 127 ("not
    // found") on a spawn failure, which is how we detect the same condition.
    let (rc, _out, _err) = if mode == "off" {
        batctl::run("batctl", &["gw_mode", "off"], Duration::from_secs(5)).await
    } else {
        let r = batctl::run("batctl", &["gw_mode", "client"], Duration::from_secs(5)).await;
        if r.0 == 127 {
            r
        } else if mode == "pinned" {
            if let Some(mac) = pinned_mac {
                let _ = batctl::run("batctl", &["gw_sel", mac], Duration::from_secs(5)).await;
            }
            r
        } else {
            r
        }
    };
    if rc == 127 {
        return json!({"ok": false, "error": "E_BATCTL_UNAVAILABLE"});
    }

    let mut resp = serde_json::Map::new();
    resp.insert("ok".to_string(), json!(true));
    resp.insert("mode".to_string(), json!(mode));
    resp.insert(
        "pinned_mac".to_string(),
        pinned_mac.map(Value::from).unwrap_or(Value::Null),
    );
    resp.insert("persisted".to_string(), json!(persist_error.is_none()));
    if let Some(err) = persist_error {
        resp.insert("persist_error".to_string(), json!(err));
    }
    Value::Object(resp)
}

/// Write `gateway.json` atomically with sorted keys, matching the FastAPI
/// `json.dumps({"mode","pinned_mac"}, sort_keys=True)` + tmp+rename. Returns the
/// OS error string on a write fault so the caller can surface `persist_error`.
///
/// The Python `json.dumps(..., sort_keys=True)` uses the default `", "` / `": "`
/// separators, so the on-disk bytes carry a space after each `:` and `,`. The
/// body is built by hand to reproduce that spacing exactly (serde_json's compact
/// form drops the spaces); `mode` precedes `pinned_mac` per `sort_keys=True`.
fn write_gateway_json(path: &Path, mode: &str, pinned_mac: Option<&str>) -> Result<(), String> {
    let body = format!(
        "{{\"mode\": {}, \"pinned_mac\": {}}}",
        json!(mode),
        match pinned_mac {
            Some(m) => json!(m),
            None => Value::Null,
        }
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
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
    fn bad_json_is_rejected_before_any_side_effect() {
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
    fn set_role_requires_a_role() {
        assert_eq!(reply(br#"{"op":"set_role"}"#)["error"], "E_MISSING_ROLE");
        assert_eq!(
            cmd(br#"{"op":"set_role","role":"relay"}"#),
            Command::SetRole {
                role: "relay".to_string(),
                reason: "rest".to_string(),
            }
        );
        // An explicit reason is carried through.
        assert_eq!(
            cmd(br#"{"op":"set_role","role":"direct","reason":"factory_reset"}"#),
            Command::SetRole {
                role: "direct".to_string(),
                reason: "factory_reset".to_string(),
            }
        );
    }

    #[test]
    fn gateway_preference_validates_the_mode() {
        assert_eq!(
            reply(br#"{"op":"set_gateway_preference"}"#)["error"],
            "E_INVALID_MODE"
        );
        assert_eq!(
            reply(br#"{"op":"set_gateway_preference","mode":"bogus"}"#)["error"],
            "E_INVALID_MODE"
        );
        assert_eq!(
            cmd(br#"{"op":"set_gateway_preference","mode":"off"}"#),
            Command::SetGatewayPreference {
                mode: "off".to_string(),
                pinned_mac: None,
            }
        );
        // An empty pinned_mac normalises to None.
        assert_eq!(
            cmd(br#"{"op":"set_gateway_preference","mode":"pinned","pinned_mac":""}"#),
            Command::SetGatewayPreference {
                mode: "pinned".to_string(),
                pinned_mac: None,
            }
        );
        assert_eq!(
            cmd(br#"{"op":"set_gateway_preference","mode":"pinned","pinned_mac":"aa:bb"}"#),
            Command::SetGatewayPreference {
                mode: "pinned".to_string(),
                pinned_mac: Some("aa:bb".to_string()),
            }
        );
    }

    #[test]
    fn pair_keypair_requires_a_blob() {
        assert_eq!(
            reply(br#"{"op":"pair_keypair"}"#)["error"],
            "E_BLOB_REQUIRED"
        );
        assert_eq!(
            reply(br#"{"op":"pair_keypair","blob_b64":""}"#)["error"],
            "E_BLOB_REQUIRED"
        );
        assert_eq!(
            cmd(br#"{"op":"pair_keypair","blob_b64":"QUJD"}"#),
            Command::PairKeypair {
                blob_b64: "QUJD".to_string(),
                peer_device_id: None,
            }
        );
        assert_eq!(
            cmd(br#"{"op":"pair_keypair","blob_b64":"QUJD","peer_device_id":"dev-1"}"#),
            Command::PairKeypair {
                blob_b64: "QUJD".to_string(),
                peer_device_id: Some("dev-1".to_string()),
            }
        );
    }

    #[test]
    fn unpair_parses_with_no_fields() {
        assert_eq!(cmd(br#"{"op":"unpair"}"#), Command::Unpair);
    }

    #[test]
    fn an_empty_line_is_a_bad_request_not_a_panic() {
        let v = reply(b"");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
    }

    #[test]
    fn gateway_json_writes_sorted_keys_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh/gateway.json");
        write_gateway_json(&path, "pinned", Some("aa:bb:cc:dd:ee:ff")).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        // sort_keys=True with the default json.dumps separators → mode before
        // pinned_mac, with a space after each colon and comma.
        assert_eq!(
            body,
            r#"{"mode": "pinned", "pinned_mac": "aa:bb:cc:dd:ee:ff"}"#
        );
        // off mode persists a null pinned_mac.
        write_gateway_json(&path, "off", None).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, r#"{"mode": "off", "pinned_mac": null}"#);
    }
}
