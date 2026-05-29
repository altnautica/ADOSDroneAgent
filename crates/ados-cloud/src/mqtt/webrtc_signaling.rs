//! WebRTC SDP signaling relay over MQTT.
//!
//! A pure SDP-string rendezvous (no `webrtc` crate — the media flows
//! peer-to-peer after the handshake, this only relays signaling text). Ports
//! `src/ados/services/cloud/webrtc_signaling.py`:
//! * subscribe `ados/{id}/webrtc/offer` (q1)
//! * on each offer, POST the SDP to the local mediamtx WHEP endpoint
//!   (`http://localhost:8889/main/whep`, PLAINTEXT localhost — no TLS)
//! * publish the SDP answer to `ados/{id}/webrtc/answer` (q1), or a JSON error
//!   doc so the browser fails fast.
//!
//! The offer-handling decision (POST → answer, or which error to publish) is
//! factored into [`build_answer`] behind a [`WhepPoster`] seam so it is
//! unit-testable with no MQTT and no mediamtx.

use super::transport::{MqttQos, MqttTransport, TransportError};
use super::{relay_username, topic_webrtc_answer, topic_webrtc_offer};

/// The local mediamtx WHEP endpoint the offer is posted to. Plaintext loopback:
/// mediamtx is started by the video service and listens on the SBC's loopback.
/// Mirrors `_LOCAL_WHEP_URL`.
pub const LOCAL_WHEP_URL: &str = "http://localhost:8889/main/whep";

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
pub trait WhepPoster {
    /// POST `sdp_offer` to the local WHEP endpoint and return the outcome.
    fn post_offer(&self, sdp_offer: &str) -> WhepResult;
}

/// What to publish on the answer topic for a given offer outcome. The browser
/// distinguishes an SDP answer (always starts with `v=0`) from a JSON error
/// (starts with `{`) by a single-character check, so an error is published as a
/// JSON doc. Mirrors `_handle_offer` + `_publish_error`.
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
                // Compact JSON, key order matching the Python json.dumps default
                // (insertion order: error then status). Exact text is not
                // wire-critical here (the browser only checks the leading char),
                // but keep it stable.
                serde_json::json!({"error": error, "status": status})
                    .to_string()
                    .into_bytes()
            }
        }
    }
}

/// Decide what to publish for an offer outcome. Mirrors `_handle_offer`: a 2xx
/// yields the SDP answer; a non-2xx yields `{"error":"whep_failed","status":N}`;
/// a POST exception yields `{"error":"whep_exception","status":0}`.
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

/// The production WHEP poster: a blocking HTTP POST to the local mediamtx WHEP
/// endpoint (plaintext loopback, no TLS).
pub struct LocalWhepPoster {
    client: reqwest::blocking::Client,
}

impl LocalWhepPoster {
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("reqwest blocking client builds");
        LocalWhepPoster { client }
    }
}

impl Default for LocalWhepPoster {
    fn default() -> Self {
        Self::new()
    }
}

impl WhepPoster for LocalWhepPoster {
    fn post_offer(&self, sdp_offer: &str) -> WhepResult {
        let resp = self
            .client
            .post(LOCAL_WHEP_URL)
            .header("Content-Type", "application/sdp")
            .body(sdp_offer.to_string())
            .send();
        match resp {
            Ok(r) => {
                let status = r.status();
                if status.is_success() {
                    match r.text() {
                        Ok(body) => WhepResult::Answer(body),
                        Err(_) => WhepResult::Exception,
                    }
                } else {
                    WhepResult::HttpError(status.as_u16())
                }
            }
            Err(_) => WhepResult::Exception,
        }
    }
}

/// The WebRTC signaling relay. Built over a connected MQTT transport + a WHEP
/// poster; one offer is handled by [`handle_offer`](Self::handle_offer).
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

    /// Subscribe to the offer topic at q1. Mirrors the on_connect subscribe.
    pub async fn subscribe_offers(&self) -> Result<(), TransportError> {
        self.transport
            .subscribe(&self.topic_offer, MqttQos::AtLeastOnce)
            .await
    }

    /// Handle one SDP offer: POST it to the local WHEP endpoint, then publish the
    /// SDP answer (or a JSON error) to the answer topic at q1. Mirrors
    /// `_handle_offer`.
    pub async fn handle_offer(&self, sdp_offer: &str) -> Result<(), TransportError> {
        let result = self.whep.post_offer(sdp_offer);
        let payload = build_answer(result).into_bytes();
        self.transport
            .publish(&self.topic_answer, MqttQos::AtLeastOnce, payload)
            .await
    }

    /// The offer topic this relay subscribes to.
    pub fn offer_topic(&self) -> &str {
        &self.topic_offer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::transport::test_support::FakeTransport;

    struct FakeWhep(std::cell::RefCell<Option<WhepResult>>);
    impl FakeWhep {
        fn answer(sdp: &str) -> Self {
            FakeWhep(std::cell::RefCell::new(Some(WhepResult::Answer(
                sdp.to_string(),
            ))))
        }
        fn http_error(code: u16) -> Self {
            FakeWhep(std::cell::RefCell::new(Some(WhepResult::HttpError(code))))
        }
        fn exception() -> Self {
            FakeWhep(std::cell::RefCell::new(Some(WhepResult::Exception)))
        }
    }
    impl WhepPoster for FakeWhep {
        fn post_offer(&self, _offer: &str) -> WhepResult {
            self.0.borrow_mut().take().unwrap_or(WhepResult::Exception)
        }
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

        let subs = relay.transport.subscriptions.lock().unwrap();
        assert_eq!(
            subs[0],
            ("ados/dev1/webrtc/offer".to_string(), MqttQos::AtLeastOnce)
        );
        drop(subs);
        let pubs = relay.transport.publishes.lock().unwrap();
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
        let pubs = relay.transport.publishes.lock().unwrap();
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
        let pubs = relay.transport.publishes.lock().unwrap();
        let body: serde_json::Value = serde_json::from_slice(&pubs[0].2).unwrap();
        assert_eq!(body["error"], "whep_exception");
        assert_eq!(body["status"], 0);
    }
}
