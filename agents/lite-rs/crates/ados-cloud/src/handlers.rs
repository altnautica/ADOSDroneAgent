//! Inbound MQTT topic handlers for the cloud relay.
//!
//! Three topics arrive on the eventloop after subscription:
//!
//!   - `ados/{device_id}/mavlink/rx` (QoS 0, opaque MAVLink frame)
//!   - `ados/{device_id}/command`    (QoS 1, JSON command envelope)
//!   - `ados/{device_id}/webrtc/offer` (QoS 1, JSON SDP offer envelope)
//!
//! `mavlink/rx` was already wired in the publish loop; the helpers here
//! re-expose the routing as a function so the test crate can drive it
//! against a synthesized payload without spinning up a broker.
//!
//! `command` is gated on a runtime config flag for the only side-effect
//! that is hard to recover from (reboot). A small LRU cache keys on the
//! envelope `request_id` so a re-delivery (QoS 1 retry, broker fan-out
//! to a reconnecting subscriber) is dropped instead of re-executed.
//!
//! `webrtc/offer` is rejected on the lite agent because the lite v1
//! profile does not host a WebRTC peer — cloud video is pushed over
//! RTSP, not negotiated via SDP. The handler publishes a `rejected`
//! answer back on `webrtc/answer` so the cloud GCS surface renders the
//! "WebRTC not supported on this drone" error instead of waiting on a
//! handshake that will never arrive.

use std::sync::Arc;

use lru::LruCache;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Maximum entries kept in the per-handler request-ID dedup cache.
/// Picked so a steady command rate of one per second carries about
/// four minutes of replay protection against duplicate deliveries.
pub const COMMAND_DEDUP_CACHE_SIZE: usize = 256;

/// Default grace period between accepting a `reboot` command and the
/// actual reboot syscall. Gives the agent time to flush the MQTT ack,
/// log the reason, and let the GCS roundtrip a confirmation toast
/// before the kernel takes the machine offline.
pub const REBOOT_GRACE_SECS: u64 = 3;

/// JSON envelope received on `ados/{device_id}/command`.
///
/// `payload` is intentionally `serde_json::Value` so the dispatcher
/// can route by `type` and let each command read whatever fields it
/// needs. Unknown command types are logged and dropped at the
/// dispatcher.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommandEnvelope {
    /// UUID v4 expected. Drives the dedup cache.
    pub request_id: String,
    /// Command discriminator. Recognised values at v0.1: `"reboot"`,
    /// `"status_request"`. Other values are logged and dropped.
    #[serde(rename = "type")]
    pub command_type: String,
    /// Command-specific payload. May be absent for parameterless
    /// commands (`status_request`, `reboot`).
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

/// JSON envelope received on `ados/{device_id}/webrtc/offer`.
#[derive(Debug, Clone, Deserialize)]
pub struct WebRtcOfferEnvelope {
    /// Standard WebRTC envelope discriminator. Always `"offer"` for
    /// inbound traffic; the field is kept on the type so a malformed
    /// envelope (e.g. a stray "answer" or "candidate") can be
    /// rejected with a clear error rather than silently routed.
    #[serde(rename = "type")]
    pub envelope_type: String,
    /// Session description. Opaque blob; the lite agent never parses
    /// the SDP body itself.
    pub sdp: String,
    /// Optional session/peer correlator for the GCS to match the
    /// answer back to the originating offer when multiple peers race.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Decoded SDP offer + correlator that gets posted onto the optional
/// channel exposed by the agent. Lite v1 does not host a peer so this
/// channel is currently always `None`; the type is part of the public
/// API so the upcoming WebRTC mission can wire it without reshaping
/// the handler surface.
#[derive(Debug, Clone)]
pub struct WebRtcOffer {
    pub sdp: String,
    pub session_id: Option<String>,
}

/// JSON envelope published on `ados/{device_id}/webrtc/answer` when
/// the lite agent rejects an offer. The cloud GCS reads the `reason`
/// to render the right error toast instead of waiting on a
/// negotiation that will never complete.
#[derive(Debug, Clone, Serialize)]
pub struct WebRtcAnswerEnvelope<'a> {
    /// Discriminator. `"rejected"` means the agent declines; the GCS
    /// surfaces the `reason` and clears the local PeerConnection.
    #[serde(rename = "type")]
    pub envelope_type: &'a str,
    /// Stable machine-readable reason code. The GCS maps this to a
    /// localised user-facing string.
    pub reason: &'a str,
    /// Optional session correlator copied back from the offer so the
    /// GCS can match the reject to its outstanding offer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Reason codes published in `WebRtcAnswerEnvelope::reason`.
pub const REASON_LITE_NOT_SUPPORTED: &str = "webrtc-not-supported-on-lite";
pub const REASON_INVALID_OFFER: &str = "invalid-offer-envelope";

/// Side-effect surface for the reboot command. Trait abstraction so
/// the integration tests can swap in a mock that records the call
/// without the test process actually rebooting the host.
pub trait RebootProvider: Send + Sync + 'static {
    /// Trigger a host reboot after the configured grace period. The
    /// implementation may sleep before issuing the syscall.
    ///
    /// Returns immediately with `Ok(())` once the reboot is in flight.
    /// Real implementations on Linux do not return on success because
    /// the kernel takes the userspace process down; the test mock
    /// returns synchronously after recording the call.
    fn schedule_reboot(&self, grace_secs: u64) -> std::io::Result<()>;
}

/// Default reboot provider for production. Calls `nix::sys::reboot`
/// after the configured grace period. Linux-only; the lite agent
/// does not target other Unixes.
#[cfg(target_os = "linux")]
pub struct SystemRebootProvider;

#[cfg(target_os = "linux")]
impl RebootProvider for SystemRebootProvider {
    fn schedule_reboot(&self, grace_secs: u64) -> std::io::Result<()> {
        // Spawn a dedicated thread for the grace sleep + reboot so the
        // calling tokio task can return its ack to the caller (the
        // MQTT publisher needs to ack the QoS-1 receive before the
        // kernel restart pulls the network stack out from under us).
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(grace_secs));
            // The reboot syscall does not return on success.
            // RB_AUTOBOOT mirrors the behaviour of `/sbin/reboot` and
            // gives systemd shutdown.target a chance to run when the
            // init system is supervising us. Errors are logged but
            // unrecoverable here — there is nothing the agent can do
            // if the kernel refuses to reboot.
            if let Err(e) = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_AUTOBOOT) {
                tracing::error!(error = ?e, "reboot syscall failed");
            }
        });
        Ok(())
    }
}

/// Cross-platform fallback for non-Linux dev hosts (macOS, etc.).
/// The lite agent target is Linux; this exists so `cargo test` on a
/// developer's macOS workstation compiles without pulling in a Linux
/// shim. The default provider on non-Linux refuses to reboot and
/// logs a warning so an operator who somehow runs the lite agent on
/// the wrong platform sees the failure instead of a silent no-op.
#[cfg(not(target_os = "linux"))]
pub struct SystemRebootProvider;

#[cfg(not(target_os = "linux"))]
impl RebootProvider for SystemRebootProvider {
    fn schedule_reboot(&self, _grace_secs: u64) -> std::io::Result<()> {
        tracing::warn!("reboot requested on a non-Linux build; ignoring");
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "reboot not supported on this platform",
        ))
    }
}

/// Result of dispatching a single `command` envelope. Returned to the
/// caller so tests can pin the side effect without observing journal
/// output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The command was decoded and dispatched. Payload-level success
    /// is reported in the structured log; this enum captures whether
    /// the dispatcher reached the side-effect path or not.
    Executed { command_type: String },
    /// The envelope re-used a `request_id` already seen recently. No
    /// side effect was taken.
    DuplicateDropped { request_id: String },
    /// The command type is recognised but the runtime configuration
    /// did not allow it (today: `reboot` with `cloud.allow_reboot=false`).
    Disabled { command_type: String },
    /// The command type was not recognised. No side effect was taken.
    UnknownType { command_type: String },
    /// The envelope failed to parse as JSON or was missing required
    /// fields.
    InvalidEnvelope,
}

/// Stateful command handler. Holds the dedup cache + the heartbeat
/// trigger channel + the reboot provider + the runtime allow-reboot
/// flag. Construct once at agent startup and share via `Arc`.
pub struct CommandHandler {
    dedup: Mutex<LruCache<String, ()>>,
    heartbeat_trigger: Option<tokio::sync::mpsc::Sender<()>>,
    reboot_provider: Arc<dyn RebootProvider>,
    allow_reboot: bool,
}

impl CommandHandler {
    /// Build a handler. `heartbeat_trigger` is the channel the cloud
    /// loop or the heartbeat task reads to fire an immediate
    /// heartbeat (rather than waiting for the next 5s tick). Pass
    /// `None` if the agent has no heartbeat path wired yet; the
    /// `status_request` command then logs and drops.
    pub fn new(
        heartbeat_trigger: Option<tokio::sync::mpsc::Sender<()>>,
        reboot_provider: Arc<dyn RebootProvider>,
        allow_reboot: bool,
    ) -> Self {
        // SAFETY: 256 is well within `NonZeroUsize::new`'s precondition.
        let cap = std::num::NonZeroUsize::new(COMMAND_DEDUP_CACHE_SIZE)
            .expect("dedup cache size is non-zero by construction");
        Self {
            dedup: Mutex::new(LruCache::new(cap)),
            heartbeat_trigger,
            reboot_provider,
            allow_reboot,
        }
    }

    /// Dispatch a raw payload received on the `command` topic.
    ///
    /// Returns the outcome so callers (tests, observability) can
    /// distinguish dropped duplicates from disabled commands from
    /// successful executions. Errors during JSON decode collapse to
    /// `CommandOutcome::InvalidEnvelope` with a structured WARN log.
    pub async fn dispatch(&self, payload: &[u8]) -> CommandOutcome {
        let envelope: CommandEnvelope = match serde_json::from_slice(payload) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    bytes = payload.len(),
                    "received malformed cloud command envelope; dropping"
                );
                return CommandOutcome::InvalidEnvelope;
            }
        };

        // Idempotency check before any side effect. The MQTT broker
        // may redeliver QoS-1 packets after a reconnect; the GCS may
        // also retry a command if its own ack was lost. Both paths
        // arrive here with the same `request_id` and must be a no-op.
        {
            let mut cache = self.dedup.lock().await;
            if cache.contains(&envelope.request_id) {
                tracing::info!(
                    request_id = %envelope.request_id,
                    command_type = %envelope.command_type,
                    "dropped duplicate command"
                );
                return CommandOutcome::DuplicateDropped {
                    request_id: envelope.request_id,
                };
            }
            cache.put(envelope.request_id.clone(), ());
        }

        match envelope.command_type.as_str() {
            "reboot" => {
                if !self.allow_reboot {
                    tracing::warn!(
                        request_id = %envelope.request_id,
                        "reboot command received but cloud.allow_reboot is false; dropping"
                    );
                    return CommandOutcome::Disabled {
                        command_type: envelope.command_type,
                    };
                }
                tracing::info!(
                    request_id = %envelope.request_id,
                    grace_secs = REBOOT_GRACE_SECS,
                    "reboot command accepted; scheduling host reboot"
                );
                if let Err(e) = self.reboot_provider.schedule_reboot(REBOOT_GRACE_SECS) {
                    tracing::error!(
                        error = %e,
                        "reboot provider rejected schedule_reboot call"
                    );
                }
                CommandOutcome::Executed {
                    command_type: envelope.command_type,
                }
            }
            "status_request" => {
                if let Some(ref trigger) = self.heartbeat_trigger {
                    // try_send so a backed-up heartbeat task never
                    // stalls the cloud-command dispatcher. Drops are
                    // logged so an operator can see when the trigger
                    // channel is wedged.
                    match trigger.try_send(()) {
                        Ok(()) => tracing::info!(
                            request_id = %envelope.request_id,
                            "status_request command fired immediate heartbeat"
                        ),
                        Err(e) => tracing::warn!(
                            request_id = %envelope.request_id,
                            error = %e,
                            "heartbeat trigger channel full or closed; status_request dropped"
                        ),
                    }
                } else {
                    tracing::info!(
                        request_id = %envelope.request_id,
                        "status_request received but no heartbeat trigger wired; dropping"
                    );
                }
                CommandOutcome::Executed {
                    command_type: envelope.command_type,
                }
            }
            other => {
                tracing::info!(
                    request_id = %envelope.request_id,
                    command_type = %other,
                    "unknown cloud command type; dropping"
                );
                CommandOutcome::UnknownType {
                    command_type: envelope.command_type,
                }
            }
        }
    }
}

/// Decode a `webrtc/offer` payload and produce the answer envelope
/// the eventloop should publish on `webrtc/answer`. Lite v1 always
/// rejects with `webrtc-not-supported-on-lite` because the lite
/// binary does not host a WebRTC peer.
///
/// `webrtc_route` is reserved for the future video mission: when a
/// lite WebRTC peer lands the dispatcher will start forwarding valid
/// offers onto the channel and only synthesize a rejection when the
/// channel is `None`. The argument is plumbed today so the handler
/// signature does not change later.
pub fn handle_webrtc_offer(
    payload: &[u8],
    webrtc_route: Option<&tokio::sync::mpsc::Sender<WebRtcOffer>>,
) -> serde_json::Value {
    let envelope: WebRtcOfferEnvelope = match serde_json::from_slice::<WebRtcOfferEnvelope>(
        payload,
    ) {
        Ok(e) if e.envelope_type == "offer" => e,
        Ok(other) => {
            tracing::warn!(
                envelope_type = %other.envelope_type,
                "received webrtc/offer with non-offer type discriminator; rejecting"
            );
            let reject = WebRtcAnswerEnvelope {
                envelope_type: "rejected",
                reason: REASON_INVALID_OFFER,
                session_id: other.session_id,
            };
            return serde_json::to_value(reject).expect("static struct serializes");
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                bytes = payload.len(),
                "received malformed webrtc/offer envelope; rejecting"
            );
            let reject = WebRtcAnswerEnvelope {
                envelope_type: "rejected",
                reason: REASON_INVALID_OFFER,
                session_id: None,
            };
            return serde_json::to_value(reject).expect("static struct serializes");
        }
    };

    if let Some(route) = webrtc_route {
        // try_send: a backed-up WebRTC SM never stalls MQTT.
        if let Err(e) = route.try_send(WebRtcOffer {
            sdp: envelope.sdp,
            session_id: envelope.session_id.clone(),
        }) {
            tracing::warn!(
                error = %e,
                "webrtc route channel full or closed; falling back to reject"
            );
            let reject = WebRtcAnswerEnvelope {
                envelope_type: "rejected",
                reason: REASON_LITE_NOT_SUPPORTED,
                session_id: envelope.session_id,
            };
            return serde_json::to_value(reject).expect("static struct serializes");
        }
        // Future: when a peer is wired, return a placeholder JSON null
        // so the publish loop knows to skip the answer publish until
        // the SM emits one of its own. Not reachable today.
        return serde_json::Value::Null;
    }

    let reject = WebRtcAnswerEnvelope {
        envelope_type: "rejected",
        reason: REASON_LITE_NOT_SUPPORTED,
        session_id: envelope.session_id,
    };
    serde_json::to_value(reject).expect("static struct serializes")
}

/// Forward a raw MAVLink frame received from the cloud relay to the
/// FC writer mpsc, preserving the existing publish-loop semantics.
/// Logs and drops when the FC queue is full or absent.
pub fn handle_mavlink_rx(
    payload: Vec<u8>,
    fc_writer: Option<&tokio::sync::mpsc::Sender<Vec<u8>>>,
) {
    let len = payload.len();
    if let Some(fc) = fc_writer {
        if let Err(e) = fc.try_send(payload) {
            tracing::warn!(
                error = %e,
                bytes = len,
                "fc writer queue full; dropping cloud-relayed mavlink frame"
            );
        }
    } else {
        tracing::debug!(
            bytes = len,
            "received mavlink/rx frame but no FC writer wired"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts schedule_reboot calls. Used by the unit tests in this
    /// module; the integration tests under `tests/` use a richer
    /// fixture that captures the grace period too.
    struct CountingRebootProvider {
        calls: Arc<AtomicUsize>,
    }

    impl RebootProvider for CountingRebootProvider {
        fn schedule_reboot(&self, _grace_secs: u64) -> std::io::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn make_handler(
        allow_reboot: bool,
    ) -> (
        CommandHandler,
        Arc<AtomicUsize>,
        tokio::sync::mpsc::Receiver<()>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingRebootProvider {
            calls: calls.clone(),
        });
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let handler = CommandHandler::new(Some(tx), provider, allow_reboot);
        (handler, calls, rx)
    }

    #[tokio::test]
    async fn invalid_envelope_returns_invalid_outcome() {
        let (handler, _, _) = make_handler(false);
        let outcome = handler.dispatch(b"not json").await;
        assert!(matches!(outcome, CommandOutcome::InvalidEnvelope));
    }

    #[tokio::test]
    async fn unknown_type_drops() {
        let (handler, _, _) = make_handler(false);
        let env = serde_json::to_vec(&serde_json::json!({
            "request_id": "r-1",
            "type": "noop",
        }))
        .unwrap();
        let outcome = handler.dispatch(&env).await;
        assert!(matches!(
            outcome,
            CommandOutcome::UnknownType { command_type } if command_type == "noop"
        ));
    }

    #[tokio::test]
    async fn webrtc_rejects_with_lite_reason() {
        let payload = serde_json::to_vec(&serde_json::json!({
            "type": "offer",
            "sdp": "v=0\r\n...",
            "session_id": "session-42",
        }))
        .unwrap();
        let answer = handle_webrtc_offer(&payload, None);
        let obj = answer.as_object().expect("answer is object");
        assert_eq!(obj.get("type").and_then(|v| v.as_str()), Some("rejected"));
        assert_eq!(
            obj.get("reason").and_then(|v| v.as_str()),
            Some(REASON_LITE_NOT_SUPPORTED)
        );
        assert_eq!(
            obj.get("session_id").and_then(|v| v.as_str()),
            Some("session-42")
        );
    }

    #[tokio::test]
    async fn webrtc_rejects_invalid_envelope() {
        let answer = handle_webrtc_offer(b"not json", None);
        assert_eq!(
            answer.get("reason").and_then(|v| v.as_str()),
            Some(REASON_INVALID_OFFER)
        );
    }
}
