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
    let relay = WebrtcSignalingRelay::new("dev1", FakeTransport::default(), FakeWhep::default());
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
