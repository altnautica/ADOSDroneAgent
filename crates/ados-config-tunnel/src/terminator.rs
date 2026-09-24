//! The drone-side terminator: turn a reassembled config request into a call
//! against the local `/api/config` surface and chunk the reply back.
//!
//! [`handle_request`] is the pure request→response logic (testable with a mock
//! [`ConfigClient`]); [`run_terminator`] is the loop that ties it to the
//! bearer transport, the reassembler, and the honest counters. The terminator
//! restricts every call to `/api/config` exactly (via [`ConfigOp`]), so the
//! channel can only read/write agent config — never a general command proxy.
//!
//! The WFB radio key is shared by a whole fleet, so it cannot tell this
//! drone's ground station from any other key holder. Every request must
//! therefore carry a relay ticket minted from the per-pair secret this drone
//! was given at pairing and naming this drone ([`TunnelAuth`]); a drone with
//! no secret refuses everything. A write is further limited to
//! [`WRITABLE_KEY_PREFIXES`]: the radio and video tuning the channel exists
//! for. Credentials, cloud and MAVLink routing, and the tunnel's own gates are
//! never writable over the radio.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use ados_protocol::mavlink::{build_tunnel_v2, tunnel_payload, tunnel_payload_type, MavHeader};
use ados_protocol::relay_ticket::{load_secret_at, RelayTicketError, RelayTicketIssuer};
use ados_protocol::tunnel_config::{
    chunk_message, CompletedMessage, PushOutcome, Reassembler, CONFIG_TUNNEL_PAYLOAD_TYPE,
};

use crate::config_client::{ConfigClient, ConfigResponse, Unreachable};
use crate::message::{error_body, parse_request, ConfigOp, MAX_CONFIG_RESPONSE_BYTES};
use crate::stats::Counters;
use crate::transport::TunnelTransport;
use crate::{MAX_BODY_BYTES, MAX_CHUNKS, REASSEMBLY_TIMEOUT, SWEEP_INTERVAL};

/// The config keys a tunnel write may touch, by dot-path prefix.
pub const WRITABLE_KEY_PREFIXES: &[&str] = &["radio.", "video."];

/// Prefixes refused even under an allowed one: the channel's own gates.
const UNWRITABLE_KEY_PREFIXES: &[&str] = &["radio.tunnel."];

/// Whether a tunnel write may touch `key`.
#[must_use]
pub fn key_is_writable(key: &str) -> bool {
    let key = key.trim();
    WRITABLE_KEY_PREFIXES.iter().any(|p| key.starts_with(p))
        && !UNWRITABLE_KEY_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// How a request's relay ticket is checked: against the per-pair secret on
/// file (re-read per request, so a secret delivered after start applies) and
/// this drone's own device id.
#[derive(Debug, Clone)]
pub struct TunnelAuth {
    pub secret_path: PathBuf,
    pub own_device_id: String,
}

impl TunnelAuth {
    fn check(&self, ticket: Option<&str>, now: i64) -> Result<(), RelayTicketError> {
        let secret = load_secret_at(&self.secret_path).ok_or(RelayTicketError::NoSecret)?;
        RelayTicketIssuer::from_secret(secret.as_bytes()).verify(
            ticket.unwrap_or(""),
            &self.own_device_id,
            now,
        )
    }
}

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A prepared response: the body bytes and whether they are an error envelope
/// (mirrored into the chunk header's `is_error` flag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandledResponse {
    pub body: Vec<u8>,
    pub is_error: bool,
}

/// Turn a request body into a response body. Every request must carry a relay
/// ticket `auth` verifies. Reads (`GET`) are then served whenever the channel
/// is enabled; writes (`PUT`) additionally require `command_enabled` and a key
/// under [`WRITABLE_KEY_PREFIXES`], and are otherwise refused honestly. The
/// config surface's own JSON is relayed verbatim on success; an unreachable
/// surface, a non-2xx status, or an over-cap body each become an honest error
/// envelope — never a fabricated value.
pub async fn handle_request(
    body: &[u8],
    command_enabled: bool,
    auth: &TunnelAuth,
    now: i64,
    client: &dyn ConfigClient,
) -> HandledResponse {
    let request = match parse_request(body) {
        Ok(r) => r,
        Err(err) => {
            return HandledResponse {
                body: err,
                is_error: true,
            }
        }
    };
    if let Err(e) = auth.check(request.ticket.as_deref(), now) {
        tracing::warn!(error = %e, "config tunnel request unauthorized");
        return HandledResponse {
            body: error_body("E_UNAUTHORIZED", &e.to_string()),
            is_error: true,
        };
    }
    match request.op {
        ConfigOp::Get { key } => relay_get(client.get().await, key.as_deref()),
        ConfigOp::Put { key, value } => {
            if !command_enabled {
                return HandledResponse {
                    body: error_body(
                        "E_WRITE_DISABLED",
                        "config writes over the radio are gated off; \
                         set radio.tunnel.command_enabled after a safety review",
                    ),
                    is_error: true,
                };
            }
            if !key_is_writable(&key) {
                return HandledResponse {
                    body: error_body(
                        "E_KEY_NOT_WRITABLE",
                        "only radio.* and video.* keys (not radio.tunnel.*) are writable \
                         over the radio",
                    ),
                    is_error: true,
                };
            }
            relay(client.put(&key, &value).await)
        }
    }
}

/// Relay a config read, narrowed to the dot-path `key` subtree when one is
/// given. The narrowing runs before the radio-link size cap, so a subtree read
/// fits where the whole config does not; an unknown key is an honest
/// `E_KEY_NOT_FOUND`, never an empty value.
fn relay_get(result: Result<ConfigResponse, Unreachable>, key: Option<&str>) -> HandledResponse {
    let Some(key) = key else {
        return relay(result);
    };
    match result {
        Ok(resp) if (200..300).contains(&resp.status) => {
            let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&resp.body) else {
                return HandledResponse {
                    body: error_body("E_CONFIG_STATUS", "config surface returned non-JSON"),
                    is_error: true,
                };
            };
            match key.split('.').try_fold(&doc, |v, part| v.get(part)) {
                Some(sub) => relay(Ok(ConfigResponse {
                    status: resp.status,
                    body: serde_json::to_vec(sub).unwrap_or_default(),
                })),
                None => HandledResponse {
                    body: error_body("E_KEY_NOT_FOUND", key),
                    is_error: true,
                },
            }
        }
        other => relay(other),
    }
}

/// Map a config-surface call result to a response body + error flag.
fn relay(result: Result<ConfigResponse, Unreachable>) -> HandledResponse {
    match result {
        Err(Unreachable(msg)) => HandledResponse {
            body: error_body("E_CONFIG_UNAVAILABLE", &msg),
            is_error: true,
        },
        Ok(resp) if (200..300).contains(&resp.status) => {
            if resp.body.len() > MAX_CONFIG_RESPONSE_BYTES {
                HandledResponse {
                    body: error_body(
                        "E_RESPONSE_TOO_LARGE",
                        &format!(
                            "{} bytes exceeds the {MAX_CONFIG_RESPONSE_BYTES}-byte radio-link \
                             limit; read this over a LAN",
                            resp.body.len()
                        ),
                    ),
                    is_error: true,
                }
            } else {
                HandledResponse {
                    body: resp.body,
                    is_error: false,
                }
            }
        }
        // A completed non-2xx (a 422 validation reject, or a 404/501 on a
        // headless Python-free node) — relay the upstream detail honestly.
        Ok(resp) => {
            let detail: String = String::from_utf8_lossy(&resp.body)
                .chars()
                .take(200)
                .collect();
            HandledResponse {
                body: error_body(
                    "E_CONFIG_STATUS",
                    &format!("upstream {} : {}", resp.status, detail),
                ),
                is_error: true,
            }
        }
    }
}

/// The drone-side loop: receive TUNNEL frames off the bearer, reassemble a
/// request, call `/api/config`, and chunk the reply back onto the downlink.
/// Runs until shutdown (returns `false`) or a config reload (returns `true`).
///
/// It only acts on REQUEST messages (`is_response == false`); a response frame
/// on the shared bearer is not for the drone and is ignored. Every accepted
/// frame, handled request, response frame, rejection, and reassembly timeout
/// bumps the shared counters so the sidecar can report the channel's true
/// state.
pub async fn run_terminator(
    transport: Arc<dyn TunnelTransport>,
    command_enabled: bool,
    auth: TunnelAuth,
    client: Arc<dyn ConfigClient>,
    counters: Arc<Counters>,
    mut shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
) -> bool {
    let mut re = Reassembler::new(MAX_CHUNKS, MAX_BODY_BYTES);
    let mut seq: u8 = 0;
    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return false; }
            }
            _ = reload.notified() => return true,
            _ = sweep.tick() => {
                let dropped = re.sweep(Instant::now().into_std(), REASSEMBLY_TIMEOUT);
                if dropped > 0 {
                    counters.add_timeouts(dropped as u64);
                }
            }
            frame = transport.recv_frame() => {
                let Ok(frame) = frame else { continue };
                if tunnel_payload_type(&frame) != Some(CONFIG_TUNNEL_PAYLOAD_TYPE) {
                    continue;
                }
                let Some(payload) = tunnel_payload(&frame) else { continue };
                counters.mark_rx();
                match re.push(&payload, Instant::now().into_std()) {
                    PushOutcome::Complete(msg) if !msg.is_response => {
                        handle_and_reply(
                            &transport, command_enabled, &auth, &client, &counters, &mut seq, msg,
                        )
                        .await;
                    }
                    PushOutcome::Complete(_) => {} // a response is not ours
                    PushOutcome::Rejected(reason) => {
                        counters.mark_rejected();
                        tracing::warn!(reason, "config tunnel chunk rejected");
                    }
                    PushOutcome::Incomplete | PushOutcome::Ignored => {}
                }
            }
        }
    }
}

async fn handle_and_reply(
    transport: &Arc<dyn TunnelTransport>,
    command_enabled: bool,
    auth: &TunnelAuth,
    client: &Arc<dyn ConfigClient>,
    counters: &Arc<Counters>,
    seq: &mut u8,
    request: CompletedMessage,
) {
    counters.mark_request();
    let handled = handle_request(
        &request.body,
        command_enabled,
        auth,
        now_unix_secs(),
        client.as_ref(),
    )
    .await;
    let frames = match chunk_message(
        request.request_id,
        true,
        handled.is_error,
        &handled.body,
        MAX_CHUNKS,
    ) {
        Ok(frames) => frames,
        Err(e) => {
            // The response body outgrew the chunk budget after the caps above;
            // fall back to a tiny honest error so the caller is not left hanging.
            tracing::warn!(error = %e, "config tunnel response too large to chunk");
            let small = error_body("E_RESPONSE_TOO_LARGE", "response exceeded the chunk budget");
            chunk_message(request.request_id, true, true, &small, MAX_CHUNKS).unwrap_or_default()
        }
    };
    for payload in frames {
        *seq = seq.wrapping_add(1);
        let header = MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: *seq,
        };
        match build_tunnel_v2(header, CONFIG_TUNNEL_PAYLOAD_TYPE, 0, 0, &payload) {
            Ok(frame) => {
                if transport.send_frame(&frame).await.is_ok() {
                    counters.mark_tx();
                }
            }
            Err(e) => tracing::warn!(error = %e, "config tunnel response frame build failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::Value;

    struct MockClient {
        get: Result<ConfigResponse, Unreachable>,
        put: Result<ConfigResponse, Unreachable>,
    }

    #[async_trait]
    impl ConfigClient for MockClient {
        async fn get(&self) -> Result<ConfigResponse, Unreachable> {
            self.get.clone()
        }
        async fn put(&self, _k: &str, _v: &str) -> Result<ConfigResponse, Unreachable> {
            self.put.clone()
        }
    }

    fn ok(status: u16, body: &[u8]) -> Result<ConfigResponse, Unreachable> {
        Ok(ConfigResponse {
            status,
            body: body.to_vec(),
        })
    }

    fn err_code(body: &[u8]) -> String {
        serde_json::from_slice::<Value>(body).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string()
    }

    const NOW: i64 = 1_700_000_000;
    const DRONE: &str = "ados-drone-01";
    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    /// A drone holding `SECRET`, and a ticket its ground station minted for it.
    fn paired() -> (tempfile::TempDir, TunnelAuth, String) {
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("relay-peer-secret");
        std::fs::write(&secret_path, SECRET).unwrap();
        let ticket = RelayTicketIssuer::from_secret(SECRET.as_bytes()).mint_at(DRONE, 30, NOW);
        let auth = TunnelAuth {
            secret_path,
            own_device_id: DRONE.to_string(),
        };
        (dir, auth, ticket)
    }

    /// `op` (a JSON object without a ticket) with `ticket` added.
    fn with_ticket(op: &str, ticket: &str) -> Vec<u8> {
        let mut v: Value = serde_json::from_str(op).unwrap();
        v["ticket"] = Value::String(ticket.to_string());
        serde_json::to_vec(&v).unwrap()
    }

    #[tokio::test]
    async fn get_relays_the_config_body_verbatim() {
        let (_d, auth, ticket) = paired();
        let client = MockClient {
            get: ok(200, br#"{"radio":{"tunnel":{"enabled":true}}}"#),
            put: ok(200, b"{}"),
        };
        let req = with_ticket(r#"{"op":"get"}"#, &ticket);
        let out = handle_request(&req, false, &auth, NOW, &client).await;
        assert!(!out.is_error);
        assert_eq!(out.body, br#"{"radio":{"tunnel":{"enabled":true}}}"#);
    }

    #[tokio::test]
    async fn a_request_without_a_ticket_for_this_drone_is_refused() {
        // Anyone holding the shared fleet radio key can inject a frame; only
        // this drone's own ground station holds the per-pair secret.
        let (_d, auth, _ticket) = paired();
        let client = MockClient {
            get: ok(200, b"{}"),
            put: ok(200, b"{}"),
        };
        let bare = handle_request(br#"{"op":"get"}"#, true, &auth, NOW, &client).await;
        assert_eq!(err_code(&bare.body), "E_UNAUTHORIZED");

        let forged = RelayTicketIssuer::from_secret(b"ffffffffffffffffffffffffffffffff")
            .mint_at(DRONE, 30, NOW);
        let req = with_ticket(r#"{"op":"put","key":"video.bitrate","value":"4"}"#, &forged);
        let out = handle_request(&req, true, &auth, NOW, &client).await;
        assert_eq!(err_code(&out.body), "E_UNAUTHORIZED");

        let for_other =
            RelayTicketIssuer::from_secret(SECRET.as_bytes()).mint_at("ados-drone-02", 30, NOW);
        let req = with_ticket(r#"{"op":"get"}"#, &for_other);
        let out = handle_request(&req, true, &auth, NOW, &client).await;
        assert_eq!(err_code(&out.body), "E_UNAUTHORIZED");

        // A drone with no secret on file refuses even a well-formed ticket.
        let (_d2, mut unprovisioned, ticket) = paired();
        unprovisioned.secret_path = _d2.path().join("absent");
        let req = with_ticket(r#"{"op":"get"}"#, &ticket);
        let out = handle_request(&req, true, &unprovisioned, NOW, &client).await;
        assert_eq!(err_code(&out.body), "E_UNAUTHORIZED");
    }

    #[tokio::test]
    async fn put_is_refused_until_command_enabled() {
        let (_d, auth, ticket) = paired();
        let client = MockClient {
            get: ok(200, b"{}"),
            put: ok(200, br#"{"status":"ok","persisted":true}"#),
        };
        let req = with_ticket(
            r#"{"op":"put","key":"video.wfb.tx_power_dbm","value":"10"}"#,
            &ticket,
        );
        let refused = handle_request(&req, false, &auth, NOW, &client).await;
        assert!(refused.is_error);
        assert_eq!(err_code(&refused.body), "E_WRITE_DISABLED");
        // With command_enabled, the write goes through and the result relays.
        let ok = handle_request(&req, true, &auth, NOW, &client).await;
        assert!(!ok.is_error);
        assert_eq!(ok.body, br#"{"status":"ok","persisted":true}"#);
    }

    #[tokio::test]
    async fn credentials_and_link_keys_are_never_writable_over_the_radio() {
        let (_d, auth, ticket) = paired();
        let client = MockClient {
            get: ok(200, b"{}"),
            put: ok(200, br#"{"status":"ok"}"#),
        };
        for key in [
            "security.api.api_key",
            "server.cloud.url",
            "mavlink.endpoints",
            "radio.tunnel.command_enabled",
            "agent.name",
        ] {
            let op = format!(r#"{{"op":"put","key":"{key}","value":"x"}}"#);
            let out = handle_request(&with_ticket(&op, &ticket), true, &auth, NOW, &client).await;
            assert_eq!(err_code(&out.body), "E_KEY_NOT_WRITABLE", "{key}");
        }
        assert!(key_is_writable("radio.channel"));
        assert!(key_is_writable("video.wfb.tx_power_dbm"));
    }

    #[tokio::test]
    async fn a_too_large_body_returns_an_honest_error_not_a_truncation() {
        let (_d, auth, ticket) = paired();
        let big = vec![b'x'; MAX_CONFIG_RESPONSE_BYTES + 1];
        let client = MockClient {
            get: ok(200, &big),
            put: ok(200, b"{}"),
        };
        let req = with_ticket(r#"{"op":"get"}"#, &ticket);
        let out = handle_request(&req, false, &auth, NOW, &client).await;
        assert!(out.is_error);
        assert_eq!(err_code(&out.body), "E_RESPONSE_TOO_LARGE");
    }

    #[tokio::test]
    async fn a_keyed_get_returns_the_subtree_so_it_fits_the_radio_link() {
        let (_d, auth, ticket) = paired();
        // A whole config over the radio-link cap, with a small radio subtree.
        let full = serde_json::json!({
            "radio": {"channel": 149},
            "blob": "x".repeat(MAX_CONFIG_RESPONSE_BYTES),
        });
        let client = MockClient {
            get: ok(200, &serde_json::to_vec(&full).unwrap()),
            put: ok(200, b"{}"),
        };
        let whole = handle_request(
            &with_ticket(r#"{"op":"get"}"#, &ticket),
            false,
            &auth,
            NOW,
            &client,
        )
        .await;
        assert_eq!(err_code(&whole.body), "E_RESPONSE_TOO_LARGE");
        let req = with_ticket(r#"{"op":"get","key":"radio"}"#, &ticket);
        let out = handle_request(&req, false, &auth, NOW, &client).await;
        assert!(!out.is_error);
        assert_eq!(
            serde_json::from_slice::<Value>(&out.body).unwrap(),
            serde_json::json!({"channel": 149})
        );
        let req = with_ticket(r#"{"op":"get","key":"radio.nope"}"#, &ticket);
        let out = handle_request(&req, false, &auth, NOW, &client).await;
        assert_eq!(err_code(&out.body), "E_KEY_NOT_FOUND");
    }

    #[tokio::test]
    async fn an_unreachable_surface_is_honest_not_fabricated() {
        let (_d, auth, ticket) = paired();
        let client = MockClient {
            get: Err(Unreachable("connection refused".into())),
            put: Err(Unreachable("connection refused".into())),
        };
        let req = with_ticket(r#"{"op":"get"}"#, &ticket);
        let out = handle_request(&req, true, &auth, NOW, &client).await;
        assert!(out.is_error);
        assert_eq!(err_code(&out.body), "E_CONFIG_UNAVAILABLE");
    }

    #[tokio::test]
    async fn a_validation_reject_relays_the_upstream_status() {
        let (_d, auth, ticket) = paired();
        let client = MockClient {
            get: ok(200, b"{}"),
            put: ok(422, br#"{"detail":"bad value"}"#),
        };
        let req = with_ticket(r#"{"op":"put","key":"video.k","value":"bad"}"#, &ticket);
        let out = handle_request(&req, true, &auth, NOW, &client).await;
        assert!(out.is_error);
        assert_eq!(err_code(&out.body), "E_CONFIG_STATUS");
        assert!(String::from_utf8_lossy(&out.body).contains("422"));
    }
}
