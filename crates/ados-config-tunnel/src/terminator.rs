//! The drone-side terminator: turn a reassembled config request into a call
//! against the local `/api/config` surface and chunk the reply back.
//!
//! [`handle_request`] is the pure request→response logic (testable with a mock
//! [`ConfigClient`]); [`run_terminator`] is the loop that ties it to the
//! bearer transport, the reassembler, and the honest counters. The terminator
//! restricts every call to `/api/config` exactly (via [`ConfigOp`]), so the
//! channel can only read/write agent config — never a general command proxy.

use std::sync::Arc;

use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use ados_protocol::mavlink::{build_tunnel_v2, tunnel_payload, tunnel_payload_type, MavHeader};
use ados_protocol::tunnel_config::{
    chunk_message, CompletedMessage, PushOutcome, Reassembler, CONFIG_TUNNEL_PAYLOAD_TYPE,
};

use crate::config_client::{ConfigClient, ConfigResponse, Unreachable};
use crate::message::{error_body, parse_request, ConfigOp, MAX_CONFIG_RESPONSE_BYTES};
use crate::stats::Counters;
use crate::transport::TunnelTransport;
use crate::{MAX_BODY_BYTES, MAX_CHUNKS, REASSEMBLY_TIMEOUT, SWEEP_INTERVAL};

/// A prepared response: the body bytes and whether they are an error envelope
/// (mirrored into the chunk header's `is_error` flag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandledResponse {
    pub body: Vec<u8>,
    pub is_error: bool,
}

/// Turn a request body into a response body. Reads (`GET`) are served whenever
/// the channel is enabled; writes (`PUT`) additionally require
/// `command_enabled` and are otherwise refused honestly. The config surface's
/// own JSON is relayed verbatim on success; an unreachable surface, a non-2xx
/// status, or an over-cap body each become an honest error envelope — never a
/// fabricated value.
pub async fn handle_request(
    body: &[u8],
    command_enabled: bool,
    client: &dyn ConfigClient,
) -> HandledResponse {
    match parse_request(body) {
        Err(err) => HandledResponse {
            body: err,
            is_error: true,
        },
        Ok(ConfigOp::Get) => relay(client.get().await),
        Ok(ConfigOp::Put { key, value }) => {
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
            relay(client.put(&key, &value).await)
        }
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
                            &transport, command_enabled, &client, &counters, &mut seq, msg,
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
    client: &Arc<dyn ConfigClient>,
    counters: &Arc<Counters>,
    seq: &mut u8,
    request: CompletedMessage,
) {
    counters.mark_request();
    let handled = handle_request(&request.body, command_enabled, client.as_ref()).await;
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

    #[tokio::test]
    async fn get_relays_the_config_body_verbatim() {
        let client = MockClient {
            get: ok(200, br#"{"radio":{"tunnel":{"enabled":true}}}"#),
            put: ok(200, b"{}"),
        };
        let out = handle_request(br#"{"op":"get"}"#, false, &client).await;
        assert!(!out.is_error);
        assert_eq!(out.body, br#"{"radio":{"tunnel":{"enabled":true}}}"#);
    }

    #[tokio::test]
    async fn put_is_refused_until_command_enabled() {
        let client = MockClient {
            get: ok(200, b"{}"),
            put: ok(200, br#"{"status":"ok","persisted":true}"#),
        };
        let req = br#"{"op":"put","key":"radio.tunnel.command_enabled","value":"true"}"#;
        let refused = handle_request(req, false, &client).await;
        assert!(refused.is_error);
        assert_eq!(err_code(&refused.body), "E_WRITE_DISABLED");
        // With command_enabled, the write goes through and the result relays.
        let ok = handle_request(req, true, &client).await;
        assert!(!ok.is_error);
        assert_eq!(ok.body, br#"{"status":"ok","persisted":true}"#);
    }

    #[tokio::test]
    async fn a_too_large_body_returns_an_honest_error_not_a_truncation() {
        let big = vec![b'x'; MAX_CONFIG_RESPONSE_BYTES + 1];
        let client = MockClient {
            get: ok(200, &big),
            put: ok(200, b"{}"),
        };
        let out = handle_request(br#"{"op":"get"}"#, false, &client).await;
        assert!(out.is_error);
        assert_eq!(err_code(&out.body), "E_RESPONSE_TOO_LARGE");
    }

    #[tokio::test]
    async fn an_unreachable_surface_is_honest_not_fabricated() {
        let client = MockClient {
            get: Err(Unreachable("connection refused".into())),
            put: Err(Unreachable("connection refused".into())),
        };
        let out = handle_request(br#"{"op":"get"}"#, true, &client).await;
        assert!(out.is_error);
        assert_eq!(err_code(&out.body), "E_CONFIG_UNAVAILABLE");
    }

    #[tokio::test]
    async fn a_validation_reject_relays_the_upstream_status() {
        let client = MockClient {
            get: ok(200, b"{}"),
            put: ok(422, br#"{"detail":"bad value"}"#),
        };
        let out = handle_request(br#"{"op":"put","key":"k","value":"bad"}"#, true, &client).await;
        assert!(out.is_error);
        assert_eq!(err_code(&out.body), "E_CONFIG_STATUS");
        assert!(String::from_utf8_lossy(&out.body).contains("422"));
    }
}
