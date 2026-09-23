//! The config request/response envelope carried inside the reassembled L3
//! body.
//!
//! The request is a small JSON op **restricted to `/api/config`** — never an
//! arbitrary path — so the channel can only read or write agent config, never
//! become a general command proxy over the radio:
//!
//! ```text
//!   {"op":"get","ticket":"<relay ticket>"}                → GET  /api/config
//!   {"op":"put","key":"<dot.path>","value":"<s>","ticket":"…"}
//!                                                         → PUT  /api/config {key,value}
//! ```
//!
//! `ticket` is the per-pair relay ticket the ground station mints for the
//! drone it addresses (see `ados_protocol::relay_ticket`); the terminator
//! refuses a request without one it can verify.
//!
//! The response body is the config surface's own JSON (verbatim on success),
//! or a `{"error":"E_…","detail":"…"}` envelope on a rejection. The chunk
//! header's `is_error` flag marks which; the caller need not sniff the body.

use serde::Deserialize;
use serde_json::{json, Value};

/// The largest response body the channel will chunk back over the radio. The
/// bearer is low-rate (128-byte chunks over the aux lane's FEC-protected
/// datagrams), so a large op — a whole-model `GET /api/config` is several KB —
/// is refused with an honest "too large, use a LAN" error rather than silently
/// truncated or flooding a lane it shares with MAVLink telemetry. A single-key
/// `PUT` result is small and always fits.
pub const MAX_CONFIG_RESPONSE_BYTES: usize = 4 * 1024;

/// A parsed, validated config operation, restricted to `/api/config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigOp {
    /// `GET /api/config` — read the (redacted) config.
    Get,
    /// `PUT /api/config` — write one dot-path key. Gated behind
    /// `radio.tunnel.command_enabled`.
    Put { key: String, value: String },
}

/// A parsed request: the op and the relay ticket presented with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelRequest {
    pub op: ConfigOp,
    pub ticket: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRequest {
    op: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    ticket: Option<String>,
}

/// Parse a request body into a [`TunnelRequest`]. `Err` carries a
/// ready-to-send error envelope (JSON bytes) so the caller relays it verbatim
/// with the `is_error` flag set.
pub fn parse_request(body: &[u8]) -> Result<TunnelRequest, Vec<u8>> {
    let req: RawRequest =
        serde_json::from_slice(body).map_err(|e| error_body("E_BAD_REQUEST", &e.to_string()))?;
    let op = match req.op.as_str() {
        "get" => ConfigOp::Get,
        "put" => {
            let Some(key) = req.key.filter(|k| !k.trim().is_empty()) else {
                return Err(error_body("E_BAD_REQUEST", "put requires a non-empty key"));
            };
            let Some(raw_value) = req.value else {
                return Err(error_body("E_BAD_REQUEST", "put requires a value"));
            };
            // The config PUT body is `{key, value:str}`; coerce a JSON scalar
            // to its string form (a bool/number is common), and take a string
            // verbatim. Reject an object/array — /api/config casts to a leaf.
            let value = match raw_value {
                Value::String(s) => s,
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Null => return Err(error_body("E_BAD_REQUEST", "value cannot be null")),
                _ => {
                    return Err(error_body(
                        "E_BAD_REQUEST",
                        "value must be a string, number, or boolean",
                    ))
                }
            };
            ConfigOp::Put { key, value }
        }
        other => return Err(error_body("E_UNKNOWN_OP", other)),
    };
    Ok(TunnelRequest {
        op,
        ticket: req.ticket.filter(|t| !t.is_empty()),
    })
}

/// Build an error envelope body (JSON bytes) for a response frame.
#[must_use]
pub fn error_body(code: &str, detail: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "error": code, "detail": detail }))
        .unwrap_or_else(|_| br#"{"error":"E_ENCODE"}"#.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_code(body: &[u8]) -> String {
        serde_json::from_slice::<Value>(body).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn parses_get_and_put() {
        assert_eq!(parse_request(br#"{"op":"get"}"#).unwrap().op, ConfigOp::Get);
        assert_eq!(
            parse_request(br#"{"op":"put","key":"radio.tunnel.enabled","value":"true"}"#)
                .unwrap()
                .op,
            ConfigOp::Put {
                key: "radio.tunnel.enabled".to_string(),
                value: "true".to_string()
            }
        );
        // A bool/number value coerces to its string form (config casts to leaf).
        assert_eq!(
            parse_request(br#"{"op":"put","key":"k","value":true}"#)
                .unwrap()
                .op,
            ConfigOp::Put {
                key: "k".to_string(),
                value: "true".to_string()
            }
        );
        assert_eq!(
            parse_request(br#"{"op":"put","key":"k","value":150}"#)
                .unwrap()
                .op,
            ConfigOp::Put {
                key: "k".to_string(),
                value: "150".to_string()
            }
        );
        // The ticket rides beside the op.
        let with_ticket = parse_request(br#"{"op":"get","ticket":"v1|t"}"#).unwrap();
        assert_eq!(with_ticket.ticket.as_deref(), Some("v1|t"));
    }

    #[test]
    fn rejects_malformed_unknown_and_incomplete() {
        assert_eq!(
            err_code(&parse_request(b"not json").unwrap_err()),
            "E_BAD_REQUEST"
        );
        assert_eq!(
            err_code(&parse_request(br#"{"op":"reboot"}"#).unwrap_err()),
            "E_UNKNOWN_OP"
        );
        assert_eq!(
            err_code(&parse_request(br#"{"op":"put","value":"x"}"#).unwrap_err()),
            "E_BAD_REQUEST"
        );
        assert_eq!(
            err_code(&parse_request(br#"{"op":"put","key":"k"}"#).unwrap_err()),
            "E_BAD_REQUEST"
        );
        assert_eq!(
            err_code(&parse_request(br#"{"op":"put","key":"k","value":{"a":1}}"#).unwrap_err()),
            "E_BAD_REQUEST"
        );
    }
}
