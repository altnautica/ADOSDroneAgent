//! WebRTC SDP signaling relay over MQTT.
//!
//! A pure SDP-string rendezvous (no `webrtc` crate — the media flows
//! peer-to-peer after the handshake, this only relays signaling text):
//! * subscribe `ados/{id}/webrtc/offer` (q1)
//! * on each offer, POST the SDP to the local mediamtx WHEP endpoint
//!   (`http://localhost:8889/main/whep`, PLAINTEXT localhost — no TLS)
//! * publish the SDP answer to `ados/{id}/webrtc/answer` (q1), or a JSON error
//!   doc so the browser fails fast.
//!
//! [`run_webrtc_signaling`] runs the lane on its own broker session
//! (`ados-{id}-webrtc`) beside the MAVLink relay. The offer-handling decision
//! (POST → answer, or which error to publish) is factored into [`build_answer`]
//! behind a [`WhepPoster`] seam so it is unit-testable with no MQTT and no
//! mediamtx.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use super::transport::{
    IncomingMessage, MqttQos, MqttTransport, RumqttcTransport, TransportConfig, TransportError,
};
use super::{relay_username, topic_webrtc_answer, topic_webrtc_offer, WEBRTC_LANE};

/// The local mediamtx WHEP endpoint the offer is posted to. Plaintext loopback:
/// mediamtx is started by the video service and listens on the node's loopback.
pub const LOCAL_WHEP_URL: &str = "http://localhost:8889/main/whep";

/// Bound on one WHEP POST, so a wedged mediamtx fails the offer fast.
const WHEP_TIMEOUT: Duration = Duration::from_secs(5);

/// The result of posting an SDP offer to the local WHEP endpoint.
pub enum WhepResult {
    /// mediamtx returned an SDP answer (2xx).
    Answer(String),
    /// mediamtx returned a non-2xx status; the body is dropped, the status kept.
    HttpError(u16),
    /// The POST itself failed (mediamtx unreachable / transport error).
    Exception,
}

/// The local-WHEP POST seam. Production posts over plaintext HTTP to mediamtx;
/// tests inject a fake so the answer/error branching is exercised without a
/// running mediamtx.
#[async_trait]
pub trait WhepPoster: Send + Sync {
    /// POST `sdp_offer` to the local WHEP endpoint and return the outcome.
    async fn post_offer(&self, sdp_offer: &str) -> WhepResult;
}

/// What to publish on the answer topic for a given offer outcome. The browser
/// distinguishes an SDP answer (always starts with `v=0`) from a JSON error (starts
/// with `{`) by a single-character check, so an error is published as a JSON doc.
pub enum AnswerPayload {
    /// The SDP answer text to publish verbatim.
    Sdp(String),
    /// A JSON error doc `{"error": <e>, "status": <s>}` to publish.
    Error { error: String, status: u16 },
}

impl AnswerPayload {
    /// The bytes to publish on the answer topic.
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            AnswerPayload::Sdp(s) => s.into_bytes(),
            AnswerPayload::Error { error, status } => {
                // Compact JSON, error then status. Exact text is not wire-critical
                // (the browser only checks the leading char), but keep it stable.
                serde_json::json!({"error": error, "status": status})
                    .to_string()
                    .into_bytes()
            }
        }
    }
}

/// Decide what to publish for an offer outcome.
pub fn build_answer(result: WhepResult) -> AnswerPayload {
    match result {
        WhepResult::Answer(sdp) => AnswerPayload::Sdp(sdp),
        WhepResult::HttpError(status) => AnswerPayload::Error {
            error: "whep_failed".to_string(),
            status,
        },
        WhepResult::Exception => AnswerPayload::Error {
            error: "whep_exception".to_string(),
            status: 0,
        },
    }
}

/// The production WHEP poster: an async HTTP POST to the local mediamtx WHEP
/// endpoint (plaintext loopback, no TLS), bounded by [`WHEP_TIMEOUT`].
pub struct LocalWhepPoster {
    client: reqwest::Client,
}

impl LocalWhepPoster {
    pub fn new() -> Self {
        // The crate's preconfigured rustls path: reqwest needs a provider set
        // even for a plaintext loopback request in this no-default-features
        // build.
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(crate::tls::client_config())
            .timeout(WHEP_TIMEOUT)
            .build()
            .expect("reqwest client builds with the rustls config");
        LocalWhepPoster { client }
    }
}

impl Default for LocalWhepPoster {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WhepPoster for LocalWhepPoster {
    async fn post_offer(&self, sdp_offer: &str) -> WhepResult {
        let resp = self
            .client
            .post(LOCAL_WHEP_URL)
            .header("Content-Type", "application/sdp")
            .body(sdp_offer.to_string())
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => match r.text().await {
                Ok(body) => WhepResult::Answer(body),
                Err(_) => WhepResult::Exception,
            },
            Ok(r) => WhepResult::HttpError(r.status().as_u16()),
            Err(_) => WhepResult::Exception,
        }
    }
}

/// The WebRTC signaling relay. Built over a connected MQTT transport + a WHEP
/// poster; [`run`](Self::run) serves offers until shutdown.
pub struct WebrtcSignalingRelay<T: MqttTransport, W: WhepPoster> {
    device_id: String,
    transport: T,
    whep: W,
    topic_offer: String,
    topic_answer: String,
}

impl<T: MqttTransport, W: WhepPoster> WebrtcSignalingRelay<T, W> {
    pub fn new(device_id: impl Into<String>, transport: T, whep: W) -> Self {
        let device_id = device_id.into();
        WebrtcSignalingRelay {
            topic_offer: topic_webrtc_offer(&device_id),
            topic_answer: topic_webrtc_answer(&device_id),
            transport,
            whep,
            device_id,
        }
    }

    /// The relay's MQTT username (`ados-{device_id}`).
    pub fn username(&self) -> String {
        relay_username(&self.device_id)
    }

    /// Subscribe to the offer topic at q1, through the transport so the topic is
    /// replayed on every fresh broker session.
    pub async fn subscribe_offers(&self) -> Result<(), TransportError> {
        self.transport
            .subscribe(&self.topic_offer, MqttQos::AtLeastOnce)
            .await
    }

    /// Handle one SDP offer: POST it to the local WHEP endpoint, then publish the SDP
    /// answer (or a JSON error) to the answer topic at q1.
    pub async fn handle_offer(&self, sdp_offer: &str) -> Result<(), TransportError> {
        let result = self.whep.post_offer(sdp_offer).await;
        self.publish_answer(build_answer(result)).await
    }

    async fn publish_answer(&self, payload: AnswerPayload) -> Result<(), TransportError> {
        self.transport
            .publish(
                &self.topic_answer,
                MqttQos::AtLeastOnce,
                payload.into_bytes(),
            )
            .await
    }

    /// The offer topic this relay subscribes to.
    pub fn offer_topic(&self) -> &str {
        &self.topic_offer
    }

    /// Serve offers from `incoming` until `shutdown` fires or the transport's
    /// incoming channel closes. While `video_allowed` is false (a data-capped
    /// ground station) an offer is answered with a `video_data_cap` error
    /// instead of opening a stream, so the browser fails fast rather than
    /// timing out.
    pub async fn run(
        &self,
        mut incoming: mpsc::Receiver<IncomingMessage>,
        video_allowed: &AtomicBool,
        mut shutdown: watch::Receiver<bool>,
    ) {
        if let Err(e) = self.subscribe_offers().await {
            tracing::warn!(error = %e, "webrtc signaling: offer subscribe failed");
        }
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return;
                    }
                }
                msg = incoming.recv() => {
                    let Some(msg) = msg else { return };
                    if msg.topic != self.topic_offer || msg.payload.is_empty() {
                        continue;
                    }
                    let result = if video_allowed.load(Ordering::Acquire) {
                        self.handle_offer(&String::from_utf8_lossy(&msg.payload)).await
                    } else {
                        self.publish_answer(AnswerPayload::Error {
                            error: "video_data_cap".to_string(),
                            status: 0,
                        })
                        .await
                    };
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "webrtc signaling: answer publish failed");
                    }
                }
            }
        }
    }
}

/// Run the WebRTC signaling lane on its own broker session until `shutdown`
/// fires, answering every offer through the local WHEP endpoint.
///
/// `relay_config` is the MAVLink relay's dial config; its ClientID is REPLACED
/// with this lane's own (`ados-{id}-webrtc`) here, so no spawn site can hand the
/// lane a ClientID that evicts the MAVLink relay's session.
pub async fn run_webrtc_signaling(
    device_id: &str,
    relay_config: &TransportConfig,
    video_allowed: Arc<AtomicBool>,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut config = relay_config.clone();
    config.client_id = format!("ados-{device_id}-{WEBRTC_LANE}");
    let transport = RumqttcTransport::connect(&config)?;
    let incoming = transport
        .take_incoming()
        .await
        .ok_or_else(|| anyhow::anyhow!("transport incoming channel already taken"))?;
    let relay = WebrtcSignalingRelay::new(device_id, transport, LocalWhepPoster::new());
    relay.run(incoming, &video_allowed, shutdown).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::transport::test_support::FakeTransport;

    struct FakeWhep(parking_lot::Mutex<Option<WhepResult>>);
    impl FakeWhep {
        fn answer(sdp: &str) -> Self {
            FakeWhep(parking_lot::Mutex::new(Some(WhepResult::Answer(
                sdp.to_string(),
            ))))
        }
        fn http_error(code: u16) -> Self {
            FakeWhep(parking_lot::Mutex::new(Some(WhepResult::HttpError(code))))
        }
        fn exception() -> Self {
            FakeWhep(parking_lot::Mutex::new(Some(WhepResult::Exception)))
        }
    }
    #[async_trait]
    impl WhepPoster for FakeWhep {
        async fn post_offer(&self, _offer: &str) -> WhepResult {
            self.0.lock().take().unwrap_or(WhepResult::Exception)
        }
    }

    /// Drive the run loop with one offer arriving on the incoming stream, the way
    /// the broker delivers it, and return what was published.
    async fn run_one_offer(whep: FakeWhep, video_allowed: bool) -> Vec<(String, Vec<u8>)> {
        let relay = WebrtcSignalingRelay::new("dev1", FakeTransport::default(), whep);
        let (tx, rx) = mpsc::channel(4);
        let (_stop_tx, stop_rx) = watch::channel(false);
        tx.send(IncomingMessage {
            topic: "ados/dev1/webrtc/offer".to_string(),
            payload: b"v=0\noffer-sdp".to_vec(),
        })
        .await
        .unwrap();
        // Closing the stream ends the loop once the queued offer is served.
        drop(tx);
        let allowed = AtomicBool::new(video_allowed);
        tokio::time::timeout(Duration::from_secs(5), relay.run(rx, &allowed, stop_rx))
            .await
            .expect("the run loop ends when the incoming stream closes");
        let subs = relay.transport.subscriptions.lock().clone();
        assert_eq!(
            subs,
            vec![("ados/dev1/webrtc/offer".to_string(), MqttQos::AtLeastOnce)],
            "the lane subscribes to the offer topic before serving"
        );
        let published: Vec<(String, Vec<u8>)> = relay
            .transport
            .publishes
            .lock()
            .iter()
            .map(|(t, _, p)| (t.clone(), p.clone()))
            .collect();
        published
    }

    #[tokio::test]
    async fn an_offer_on_the_broker_is_answered_with_the_local_sdp() {
        let pubs = run_one_offer(FakeWhep::answer("v=0\nanswer-sdp"), true).await;
        assert_eq!(
            pubs,
            vec![(
                "ados/dev1/webrtc/answer".to_string(),
                b"v=0\nanswer-sdp".to_vec()
            )]
        );
    }

    #[tokio::test]
    async fn a_data_capped_node_refuses_the_offer_without_opening_a_stream() {
        let pubs = run_one_offer(FakeWhep::answer("v=0\nanswer-sdp"), false).await;
        assert_eq!(pubs.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&pubs[0].1).unwrap();
        assert_eq!(body["error"], "video_data_cap");
    }

    #[test]
    fn build_answer_maps_outcomes() {
        assert!(matches!(
            build_answer(WhepResult::Answer("v=0\n".into())),
            AnswerPayload::Sdp(_)
        ));
        match build_answer(WhepResult::HttpError(503)) {
            AnswerPayload::Error { error, status } => {
                assert_eq!(error, "whep_failed");
                assert_eq!(status, 503);
            }
            _ => panic!("expected error"),
        }
        match build_answer(WhepResult::Exception) {
            AnswerPayload::Error { error, status } => {
                assert_eq!(error, "whep_exception");
                assert_eq!(status, 0);
            }
            _ => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn offer_answer_round_trip_publishes_sdp_on_q1() {
        let relay = WebrtcSignalingRelay::new(
            "dev1",
            FakeTransport::default(),
            FakeWhep::answer("v=0\nanswer-sdp"),
        );
        relay.subscribe_offers().await.unwrap();
        relay.handle_offer("v=0\noffer-sdp").await.unwrap();

        let subs = relay.transport.subscriptions.lock();
        assert_eq!(
            subs[0],
            ("ados/dev1/webrtc/offer".to_string(), MqttQos::AtLeastOnce)
        );
        drop(subs);
        let pubs = relay.transport.publishes.lock();
        assert_eq!(pubs.len(), 1);
        assert_eq!(pubs[0].0, "ados/dev1/webrtc/answer");
        assert_eq!(pubs[0].1, MqttQos::AtLeastOnce);
        // The SDP answer is published verbatim (starts with v=0).
        assert_eq!(pubs[0].2, b"v=0\nanswer-sdp");
    }

    #[tokio::test]
    async fn whep_http_error_publishes_json_error() {
        let relay =
            WebrtcSignalingRelay::new("dev1", FakeTransport::default(), FakeWhep::http_error(500));
        relay.handle_offer("v=0\noffer").await.unwrap();
        let pubs = relay.transport.publishes.lock();
        let body: serde_json::Value = serde_json::from_slice(&pubs[0].2).unwrap();
        assert_eq!(body["error"], "whep_failed");
        assert_eq!(body["status"], 500);
        // A JSON error starts with '{' so the browser distinguishes it from SDP.
        assert_eq!(pubs[0].2[0], b'{');
    }

    #[tokio::test]
    async fn whep_exception_publishes_json_error() {
        let relay =
            WebrtcSignalingRelay::new("dev1", FakeTransport::default(), FakeWhep::exception());
        relay.handle_offer("v=0\noffer").await.unwrap();
        let pubs = relay.transport.publishes.lock();
        let body: serde_json::Value = serde_json::from_slice(&pubs[0].2).unwrap();
        assert_eq!(body["error"], "whep_exception");
        assert_eq!(body["status"], 0);
    }
}
