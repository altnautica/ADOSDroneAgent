//! CAN passthrough route surface.
//!
//! Reserved for a future agent-side CAN bridge. Today most CAN access flows
//! end-to-end via MAVLink passthrough between the GCS and the flight controller
//! (the MAVLink relay forwards CAN_FRAME / CANFD_FRAME / CAN_FILTER_MODIFY plus
//! the CAN_FORWARD command unfiltered), so this route is a deliberate stub: it
//! answers `501 Not Implemented` with a small JSON envelope so a probing client
//! can distinguish a planned-but-absent surface from a missing route (`404`) or
//! an auth failure. The GCS treats `404` or `501` here as "passthrough disabled"
//! and falls back to the MAVLink CAN_FORWARD path.
//!
//! This is a static, side-effect-free handler: it never opens a CAN channel,
//! reads, or writes anything. Because it cannot carry CAN traffic, the agent
//! does not list `can.passthrough` among the capability flags on `/api/version`
//! — a client that believed the claim would skip the `CAN_FORWARD` relay and be
//! left with no route to the bus. Landing a real bridge means replacing this
//! handler and adding the flag back in the same change.
//!
//! The `501` body (`{"error": ..., "message": ...}`, in that key order) is the
//! shipped wire form a GCS already parses, so it stays fixed even though the
//! FastAPI handler it was first written to mirror has since been retired.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// `POST /api/can/passthrough` → `501` with the fixed not-implemented envelope.
///
/// The envelope key order (`error` then `message`) is load-bearing: it is the
/// byte form already on the wire, so it is pinned rather than left to chance.
pub async fn can_passthrough() -> Response {
    let body: Value = json!({
        "error": "not_implemented",
        "message": "CAN passthrough planned for future agent-side support",
    });
    (StatusCode::NOT_IMPLEMENTED, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use http::header::CONTENT_TYPE;

    /// The handler answers `501` with the exact not-implemented envelope: the two
    /// keys `error` and `message`, in that order. Asserting on the serialized
    /// bytes pins both the field order and the compact JSON form (no inter-token
    /// spaces) the GCS parses.
    #[tokio::test]
    async fn passthrough_is_the_fixed_501_envelope() {
        let resp = can_passthrough().await;
        // 501 Not Implemented: a planned surface, distinguishable from a missing
        // route's 404.
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        // Content-Type is JSON, since axum's `Json` sets it.
        let ct = resp
            .headers()
            .get(CONTENT_TYPE)
            .expect("a content-type header")
            .to_str()
            .expect("an ascii content-type");
        assert!(
            ct.starts_with("application/json"),
            "content-type should be JSON, got {ct}"
        );
        let bytes = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("a buffered body");
        // The shipped byte form: compact, no spaces, `error` before `message`.
        assert_eq!(
            &bytes[..],
            br#"{"error":"not_implemented","message":"CAN passthrough planned for future agent-side support"}"#
        );
        // Belt-and-suspenders: it parses to an object carrying exactly those two
        // keys, with the documented string values.
        let parsed: Value = serde_json::from_slice(&bytes).expect("a JSON body");
        let obj = parsed.as_object().expect("an object body");
        assert_eq!(obj.len(), 2);
        assert_eq!(obj.get("error"), Some(&Value::from("not_implemented")));
        assert_eq!(
            obj.get("message"),
            Some(&Value::from(
                "CAN passthrough planned for future agent-side support"
            ))
        );
    }
}
