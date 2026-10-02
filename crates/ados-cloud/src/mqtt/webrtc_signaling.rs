//! WebRTC SDP signaling relay over MQTT.
//!
//! A pure SDP-string rendezvous (no `webrtc` crate — the media flows
//! peer-to-peer after the handshake, this only relays signaling text):
//! * subscribe `ados/{id}/webrtc/offer` (q1). Every message is a JSON envelope
//!   carrying the browser's session id: `{"sessionId", "sdp"}` opens a
//!   session, `{"sessionId", "close": true}` ends it.
//! * on an offer, POST the SDP to the local mediamtx WHEP endpoint
//!   (`http://localhost:8889/main/whep`, PLAINTEXT localhost — no TLS) and keep
//!   the WHEP session's `Location` under the session id
//! * publish the SDP answer to `ados/{id}/webrtc/answer/{sessionId}` (q1), or a
//!   JSON error doc so the browser fails fast. Each viewer only ever sees its
//!   own answer.
//! * on a close, a re-offer for the same session, an answer that could not be
//!   published, more than [`MAX_SESSIONS`] open sessions, or lane shutdown,
//!   DELETE the WHEP session so mediamtx stops sending to a viewer that left.
//!   The browser registers its close as the MQTT last will, so a viewer that
//!   vanishes without closing is still torn down once the broker notices.
//!
//! [`run_webrtc_signaling`] runs the lane on its own broker session
//! (`ados-{id}-webrtc`) beside the MAVLink relay. The offer-handling decision
//! (POST → answer, or which error to publish) is factored into [`build_answer`]
//! behind a [`WhepPoster`] seam so it is unit-testable with no MQTT and no
//! mediamtx.

use std::collections::VecDeque;
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

/// The origin a WHEP session `Location` resolves against. Only locations on
/// this origin are ever deleted.
const LOCAL_WHEP_ORIGIN: &str = "http://localhost:8889";

/// Bound on one WHEP POST, so a wedged mediamtx fails the offer fast.
const WHEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Most WHEP sessions the lane keeps open at once. A new one past the cap
/// tears down the oldest, so viewers that vanished without a close cannot pile
/// up readers on a capped uplink.
pub const MAX_SESSIONS: usize = 4;

/// The result of posting an SDP offer to the local WHEP endpoint.
pub enum WhepResult {
    /// mediamtx returned an SDP answer (2xx), plus the WHEP session `Location`
    /// when it sent one.
    Answer {
        sdp: String,
        location: Option<String>,
    },
    /// mediamtx returned a non-2xx status; the body is dropped, the status kept.
    HttpError(u16),
    /// The POST itself failed (mediamtx unreachable / transport error).
    Exception,
}

/// The local-WHEP seam. Production talks plaintext HTTP to mediamtx; tests
/// inject a fake so the answer/error branching and the session teardown are
/// exercised without a running mediamtx.
#[async_trait]
pub trait WhepPoster: Send + Sync {
    /// POST `sdp_offer` to the local WHEP endpoint and return the outcome.
    async fn post_offer(&self, sdp_offer: &str) -> WhepResult;
    /// DELETE the WHEP session at `location` (as returned by [`post_offer`]).
    ///
    /// [`post_offer`]: WhepPoster::post_offer
    async fn delete_session(&self, location: &str);
}

/// One message on the offer topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalingMessage {
    /// Open (or replace) the session's stream with this SDP offer.
    Offer { session_id: String, sdp: String },
    /// The viewer left; tear the session down.
    Close { session_id: String },
}

/// Whether `id` is a usable session id: 8 to 64 ASCII letters, digits, `-` or
/// `_`, so it can never add a topic level or an MQTT wildcard.
fn valid_session_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Parse an offer-topic payload. `None` for anything that is not a JSON
/// envelope with a valid `sessionId` and either a non-empty `sdp` or
/// `"close": true`: without a session id there is no topic to answer on.
pub fn parse_signaling(payload: &[u8]) -> Option<SignalingMessage> {
    let doc: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let session_id = doc.get("sessionId")?.as_str()?;
    if !valid_session_id(session_id) {
        return None;
    }
    let session_id = session_id.to_string();
    if doc.get("close").and_then(|v| v.as_bool()) == Some(true) {
        return Some(SignalingMessage::Close { session_id });
    }
    let sdp = doc.get("sdp")?.as_str().filter(|s| !s.is_empty())?;
    Some(SignalingMessage::Offer {
        session_id,
        sdp: sdp.to_string(),
    })
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
        WhepResult::Answer { sdp, .. } => AnswerPayload::Sdp(sdp),
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

/// Resolve a WHEP `Location` header to the URL to DELETE. Relative paths
/// resolve against the local mediamtx origin; an absolute URL is accepted only
/// on that origin, so the lane never sends a DELETE anywhere else.
fn resolve_location(location: &str) -> Option<String> {
    if location.starts_with('/') {
        return Some(format!("{LOCAL_WHEP_ORIGIN}{location}"));
    }
    location
        .strip_prefix(LOCAL_WHEP_ORIGIN)
        .filter(|rest| rest.starts_with('/'))
        .map(|_| location.to_string())
}

/// The production WHEP poster: async HTTP to the local mediamtx WHEP endpoint
/// (plaintext loopback, no TLS), each request bounded by [`WHEP_TIMEOUT`].
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
            Ok(r) if r.status().is_success() => {
                let location = r
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                match r.text().await {
                    Ok(sdp) => WhepResult::Answer { sdp, location },
                    Err(_) => WhepResult::Exception,
                }
            }
            Ok(r) => WhepResult::HttpError(r.status().as_u16()),
            Err(_) => WhepResult::Exception,
        }
    }

    async fn delete_session(&self, location: &str) {
        let Some(url) = resolve_location(location) else {
            tracing::warn!(
                location,
                "webrtc signaling: WHEP location not on the local origin; not deleted"
            );
            return;
        };
        match self.client.delete(&url).send().await {
            Ok(r) if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND => {}
            Ok(r) => {
                tracing::debug!(
                    status = r.status().as_u16(),
                    "webrtc signaling: WHEP session delete refused"
                )
            }
            Err(e) => tracing::debug!(error = %e, "webrtc signaling: WHEP session delete failed"),
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
    /// Open WHEP sessions, oldest first: `(session id, WHEP location)`.
    sessions: parking_lot::Mutex<VecDeque<(String, String)>>,
}

impl<T: MqttTransport, W: WhepPoster> WebrtcSignalingRelay<T, W> {
    pub fn new(device_id: impl Into<String>, transport: T, whep: W) -> Self {
        let device_id = device_id.into();
        WebrtcSignalingRelay {
            topic_offer: topic_webrtc_offer(&device_id),
            transport,
            whep,
            device_id,
            sessions: parking_lot::Mutex::new(VecDeque::new()),
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

    /// Handle one SDP offer for `session_id`: drop any stream that session
    /// already holds, POST the offer to the local WHEP endpoint, then publish
    /// the SDP answer (or a JSON error) on the session's answer topic at q1.
    /// The new WHEP session is kept for a later close, or deleted at once when
    /// its answer could not be published.
    pub async fn handle_offer(
        &self,
        session_id: &str,
        sdp_offer: &str,
    ) -> Result<(), TransportError> {
        self.close_session(session_id).await;
        let result = self.whep.post_offer(sdp_offer).await;
        let location = match &result {
            WhepResult::Answer { location, .. } => location.clone(),
            _ => None,
        };
        let published = self.publish_answer(session_id, build_answer(result)).await;
        if let Some(location) = location {
            if published.is_ok() {
                self.track_session(session_id, location).await;
            } else {
                // The browser never got the answer; nobody will close this.
                self.whep.delete_session(&location).await;
            }
        }
        published
    }

    /// Remember a WHEP session, tearing down the oldest past [`MAX_SESSIONS`].
    async fn track_session(&self, session_id: &str, location: String) {
        let evicted: Vec<String> = {
            let mut sessions = self.sessions.lock();
            sessions.push_back((session_id.to_string(), location));
            let mut evicted = Vec::new();
            while sessions.len() > MAX_SESSIONS {
                if let Some((_, loc)) = sessions.pop_front() {
                    evicted.push(loc);
                }
            }
            evicted
        };
        for location in evicted {
            self.whep.delete_session(&location).await;
        }
    }

    /// Tear down the WHEP session `session_id` holds, if any.
    pub async fn close_session(&self, session_id: &str) {
        let location = {
            let mut sessions = self.sessions.lock();
            sessions
                .iter()
                .position(|(id, _)| id == session_id)
                .and_then(|i| sessions.remove(i))
                .map(|(_, loc)| loc)
        };
        if let Some(location) = location {
            self.whep.delete_session(&location).await;
        }
    }

    /// Tear down every open WHEP session (lane shutdown).
    async fn close_all(&self) {
        let all: Vec<(String, String)> = self.sessions.lock().drain(..).collect();
        for (_, location) in all {
            self.whep.delete_session(&location).await;
        }
    }

    async fn publish_answer(
        &self,
        session_id: &str,
        payload: AnswerPayload,
    ) -> Result<(), TransportError> {
        self.transport
            .publish(
                &topic_webrtc_answer(&self.device_id, session_id),
                MqttQos::AtLeastOnce,
                payload.into_bytes(),
            )
            .await
    }

    /// The offer topic this relay subscribes to.
    pub fn offer_topic(&self) -> &str {
        &self.topic_offer
    }

    /// Serve offers and closes from `incoming` until `shutdown` fires or the
    /// transport's incoming channel closes, then tear down every open session.
    /// While `video_allowed` is false (a data-capped ground station) an offer
    /// is answered with a `video_data_cap` error instead of opening a stream,
    /// so the browser fails fast rather than timing out.
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
                        break;
                    }
                }
                msg = incoming.recv() => {
                    let Some(msg) = msg else { break };
                    if msg.topic != self.topic_offer {
                        continue;
                    }
                    let Some(message) = parse_signaling(&msg.payload) else {
                        tracing::debug!("webrtc signaling: offer-topic message without a valid session envelope dropped");
                        continue;
                    };
                    let result = match message {
                        SignalingMessage::Close { session_id } => {
                            self.close_session(&session_id).await;
                            Ok(())
                        }
                        SignalingMessage::Offer { session_id, sdp } => {
                            if video_allowed.load(Ordering::Acquire) {
                                self.handle_offer(&session_id, &sdp).await
                            } else {
                                self.publish_answer(
                                    &session_id,
                                    AnswerPayload::Error {
                                        error: "video_data_cap".to_string(),
                                        status: 0,
                                    },
                                )
                                .await
                            }
                        }
                    };
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "webrtc signaling: answer publish failed");
                    }
                }
            }
        }
        self.close_all().await;
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

    const SID_A: &str = "sessionaaaa1";
    const SID_B: &str = "sessionbbbb2";

    /// A fake WHEP endpoint: every offer answers with a fresh session location
    /// (or the queued outcome), and every DELETE is recorded.
    #[derive(Default)]
    struct FakeWhep {
        outcome: parking_lot::Mutex<Option<WhepResult>>,
        posted: parking_lot::Mutex<u32>,
        deleted: parking_lot::Mutex<Vec<String>>,
    }
    impl FakeWhep {
        fn with(outcome: WhepResult) -> Self {
            FakeWhep {
                outcome: parking_lot::Mutex::new(Some(outcome)),
                ..FakeWhep::default()
            }
        }
        fn deleted(&self) -> Vec<String> {
            self.deleted.lock().clone()
        }
    }
    #[async_trait]
    impl WhepPoster for FakeWhep {
        async fn post_offer(&self, _offer: &str) -> WhepResult {
            let n = {
                let mut posted = self.posted.lock();
                *posted += 1;
                *posted
            };
            self.outcome.lock().take().unwrap_or(WhepResult::Answer {
                sdp: "v=0\nanswer-sdp".to_string(),
                location: Some(format!("/main/whep/s{n}")),
            })
        }
        async fn delete_session(&self, location: &str) {
            self.deleted.lock().push(location.to_string());
        }
    }

    fn offer(session_id: &str) -> Vec<u8> {
        serde_json::json!({"sessionId": session_id, "sdp": "v=0\noffer-sdp"})
            .to_string()
            .into_bytes()
    }

    fn close(session_id: &str) -> Vec<u8> {
        serde_json::json!({"sessionId": session_id, "close": true})
            .to_string()
            .into_bytes()
    }

    /// Drive the run loop with `payloads` arriving on the offer topic, the way
    /// the broker delivers them, and return the relay for inspection.
    async fn run_messages(
        whep: FakeWhep,
        video_allowed: bool,
        payloads: Vec<Vec<u8>>,
    ) -> WebrtcSignalingRelay<FakeTransport, FakeWhep> {
        let relay = WebrtcSignalingRelay::new("dev1", FakeTransport::default(), whep);
        let (tx, rx) = mpsc::channel(16);
        let (_stop_tx, stop_rx) = watch::channel(false);
        for payload in payloads {
            tx.send(IncomingMessage {
                topic: "ados/dev1/webrtc/offer".to_string(),
                payload,
            })
            .await
            .unwrap();
        }
        // Closing the stream ends the loop once the queued messages are served.
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
        relay
    }

    fn published(relay: &WebrtcSignalingRelay<FakeTransport, FakeWhep>) -> Vec<(String, Vec<u8>)> {
        relay
            .transport
            .publishes
            .lock()
            .iter()
            .map(|(t, _, p)| (t.clone(), p.clone()))
            .collect()
    }

    /// Two viewers each get their own answer on their own session topic.
    #[tokio::test]
    async fn concurrent_viewers_are_answered_on_their_own_topics() {
        let relay = run_messages(FakeWhep::default(), true, vec![offer(SID_A), offer(SID_B)]).await;
        let pubs = published(&relay);
        assert_eq!(
            pubs.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec![
                "ados/dev1/webrtc/answer/sessionaaaa1",
                "ados/dev1/webrtc/answer/sessionbbbb2"
            ]
        );
        assert_eq!(pubs[0].1, b"v=0\nanswer-sdp");
    }

    /// A close deletes exactly that session's WHEP stream; the rest are torn
    /// down when the lane stops.
    #[tokio::test]
    async fn a_close_deletes_that_sessions_whep_stream() {
        let relay = run_messages(
            FakeWhep::default(),
            true,
            vec![offer(SID_A), offer(SID_B), close(SID_A)],
        )
        .await;
        let deleted = relay.whep.deleted();
        assert_eq!(deleted[0], "/main/whep/s1", "the closed session goes first");
        assert_eq!(
            deleted[1..],
            ["/main/whep/s2".to_string()],
            "shutdown clears the rest"
        );
    }

    /// A re-offer on the same session replaces its stream instead of leaking
    /// the first one.
    #[tokio::test]
    async fn a_reoffer_replaces_the_sessions_stream() {
        let relay = run_messages(FakeWhep::default(), true, vec![offer(SID_A), offer(SID_A)]).await;
        let deleted = relay.whep.deleted();
        assert_eq!(
            deleted,
            vec!["/main/whep/s1".to_string(), "/main/whep/s2".to_string()]
        );
    }

    /// Past the session cap the oldest stream is torn down.
    #[tokio::test]
    async fn sessions_past_the_cap_evict_the_oldest() {
        let offers: Vec<Vec<u8>> = (0..=MAX_SESSIONS)
            .map(|i| offer(&format!("session-{i:04}")))
            .collect();
        let relay =
            WebrtcSignalingRelay::new("dev1", FakeTransport::default(), FakeWhep::default());
        let allowed = AtomicBool::new(true);
        let (tx, rx) = mpsc::channel(16);
        let (stop_tx, stop_rx) = watch::channel(false);
        let run = relay.run(rx, &allowed, stop_rx);
        tokio::pin!(run);
        for payload in offers {
            tx.send(IncomingMessage {
                topic: "ados/dev1/webrtc/offer".to_string(),
                payload,
            })
            .await
            .unwrap();
        }
        // Let the loop drain the queue, then check before shutdown clears all.
        let _ = tokio::time::timeout(Duration::from_millis(200), &mut run).await;
        assert_eq!(relay.whep.deleted(), vec!["/main/whep/s1".to_string()]);
        stop_tx.send(true).unwrap();
        run.await;
    }

    #[tokio::test]
    async fn a_data_capped_node_refuses_the_offer_without_opening_a_stream() {
        let relay = run_messages(FakeWhep::default(), false, vec![offer(SID_A)]).await;
        let pubs = published(&relay);
        assert_eq!(pubs.len(), 1);
        assert_eq!(pubs[0].0, "ados/dev1/webrtc/answer/sessionaaaa1");
        let body: serde_json::Value = serde_json::from_slice(&pubs[0].1).unwrap();
        assert_eq!(body["error"], "video_data_cap");
        assert_eq!(*relay.whep.posted.lock(), 0);
    }

    /// Payloads without a session envelope have no topic to answer on and are
    /// dropped without opening a stream.
    #[tokio::test]
    async fn an_offer_without_a_session_id_is_dropped() {
        let relay = run_messages(
            FakeWhep::default(),
            true,
            vec![
                b"v=0\nbare-sdp".to_vec(),
                serde_json::json!({"sessionId": "a/b+#", "sdp": "v=0"})
                    .to_string()
                    .into_bytes(),
            ],
        )
        .await;
        assert!(published(&relay).is_empty());
        assert_eq!(*relay.whep.posted.lock(), 0);
    }

    #[test]
    fn parse_signaling_reads_offers_and_closes() {
        assert_eq!(
            parse_signaling(&offer(SID_A)),
            Some(SignalingMessage::Offer {
                session_id: SID_A.to_string(),
                sdp: "v=0\noffer-sdp".to_string()
            })
        );
        assert_eq!(
            parse_signaling(&close(SID_A)),
            Some(SignalingMessage::Close {
                session_id: SID_A.to_string()
            })
        );
        assert_eq!(parse_signaling(&offer("short")), None);
        assert_eq!(parse_signaling(br#"{"sessionId":"sessionaaaa1"}"#), None);
    }

    #[test]
    fn locations_resolve_only_to_the_local_origin() {
        assert_eq!(
            resolve_location("/main/whep/abc").as_deref(),
            Some("http://localhost:8889/main/whep/abc")
        );
        assert_eq!(
            resolve_location("http://localhost:8889/main/whep/abc").as_deref(),
            Some("http://localhost:8889/main/whep/abc")
        );
        assert_eq!(resolve_location("http://example.com/main/whep/abc"), None);
        assert_eq!(resolve_location("http://localhost:88890/x"), None);
    }

    #[test]
    fn build_answer_maps_outcomes() {
        assert!(matches!(
            build_answer(WhepResult::Answer {
                sdp: "v=0\n".into(),
                location: None
            }),
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
    async fn whep_http_error_publishes_json_error() {
        let relay = WebrtcSignalingRelay::new(
            "dev1",
            FakeTransport::default(),
            FakeWhep::with(WhepResult::HttpError(500)),
        );
        relay.handle_offer(SID_A, "v=0\noffer").await.unwrap();
        let pubs = relay.transport.publishes.lock();
        assert_eq!(pubs[0].0, "ados/dev1/webrtc/answer/sessionaaaa1");
        assert_eq!(pubs[0].1, MqttQos::AtLeastOnce);
        let body: serde_json::Value = serde_json::from_slice(&pubs[0].2).unwrap();
        assert_eq!(body["error"], "whep_failed");
        assert_eq!(body["status"], 500);
        // A JSON error starts with '{' so the browser distinguishes it from SDP.
        assert_eq!(pubs[0].2[0], b'{');
    }

    #[tokio::test]
    async fn whep_exception_publishes_json_error() {
        let relay = WebrtcSignalingRelay::new(
            "dev1",
            FakeTransport::default(),
            FakeWhep::with(WhepResult::Exception),
        );
        relay.handle_offer(SID_A, "v=0\noffer").await.unwrap();
        let pubs = relay.transport.publishes.lock();
        let body: serde_json::Value = serde_json::from_slice(&pubs[0].2).unwrap();
        assert_eq!(body["error"], "whep_exception");
        assert_eq!(body["status"], 0);
    }
}
