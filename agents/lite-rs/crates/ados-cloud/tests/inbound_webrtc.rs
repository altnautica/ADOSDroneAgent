//! Integration tests for the inbound `webrtc/offer` topic dispatcher.
//!
//! Lite v1 does not host a WebRTC peer — cloud video is an RTSP push
//! pipeline, not an SDP-negotiated peer connection. The handler must
//! synthesize a `rejected` answer with a stable reason code so the
//! cloud GCS surface renders the right error toast instead of waiting
//! on a handshake that will never arrive.

use ados_cloud::handlers::{
    handle_webrtc_offer, REASON_INVALID_OFFER, REASON_LITE_NOT_SUPPORTED,
};

#[test]
fn valid_offer_returns_rejected_answer_for_lite() {
    let payload = serde_json::to_vec(&serde_json::json!({
        "type": "offer",
        "sdp": "v=0\r\no=- 12345 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n",
        "session_id": "session-abc",
    }))
    .unwrap();

    let answer = handle_webrtc_offer(&payload, None);
    let obj = answer.as_object().expect("answer serializes to a JSON object");

    assert_eq!(
        obj.get("type").and_then(|v| v.as_str()),
        Some("rejected"),
        "answer type must be `rejected` so the GCS clears its PeerConnection"
    );
    assert_eq!(
        obj.get("reason").and_then(|v| v.as_str()),
        Some(REASON_LITE_NOT_SUPPORTED),
        "reason must match the documented lite-rejection code"
    );
    assert_eq!(
        obj.get("session_id").and_then(|v| v.as_str()),
        Some("session-abc"),
        "session_id must be echoed back so the GCS matches the reject to its outstanding offer"
    );
}

#[test]
fn missing_session_id_omits_session_id_in_answer() {
    let payload = serde_json::to_vec(&serde_json::json!({
        "type": "offer",
        "sdp": "v=0\r\n",
    }))
    .unwrap();

    let answer = handle_webrtc_offer(&payload, None);
    assert!(
        answer.get("session_id").is_none(),
        "session_id key must be omitted when the offer didn't carry one"
    );
}

#[test]
fn malformed_envelope_returns_invalid_offer_reason() {
    let answer = handle_webrtc_offer(b"not json at all", None);
    assert_eq!(
        answer.get("reason").and_then(|v| v.as_str()),
        Some(REASON_INVALID_OFFER),
    );
}

#[test]
fn wrong_type_discriminator_returns_invalid_offer_reason() {
    // A stray "answer" or "candidate" envelope hitting the offer
    // topic must be rejected with the invalid-offer reason rather
    // than the lite-not-supported one. The discriminators serve
    // different audiences and must not collapse.
    let payload = serde_json::to_vec(&serde_json::json!({
        "type": "answer",
        "sdp": "v=0\r\n",
        "session_id": "session-xyz",
    }))
    .unwrap();

    let answer = handle_webrtc_offer(&payload, None);
    let obj = answer.as_object().unwrap();
    assert_eq!(obj.get("type").and_then(|v| v.as_str()), Some("rejected"));
    assert_eq!(
        obj.get("reason").and_then(|v| v.as_str()),
        Some(REASON_INVALID_OFFER),
    );
    // session_id still echoed so the GCS can correlate.
    assert_eq!(
        obj.get("session_id").and_then(|v| v.as_str()),
        Some("session-xyz"),
    );
}
