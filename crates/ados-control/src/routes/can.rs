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
//! reads, or writes anything. It reproduces the residual Python `can.py`
//! handler's `501` body byte-for-byte (`{"error": ..., "message": ...}`, in that
//! key order). When a real bridge lands, the stub gets replaced with a streaming
//! handler; until then the body is fixed.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// `POST /api/can/passthrough` → `501` with the fixed not-implemented envelope.
///
/// Mirrors the Python `can.py:can_passthrough`, which returns
/// `JSONResponse(status_code=501, content={"error": "not_implemented",
/// "message": "CAN passthrough planned for future agent-side support"})`. The
/// envelope key order (`error` then `message`) matches the Python dict's
/// insertion order so the two bodies are byte-identical.
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
    /// keys `error` and `message`, in that order, with the same string values the
    /// Python stub returns. Asserting on the serialized bytes pins both the field
    /// order and the compact JSON form (no inter-token spaces) the GCS parses.
    #[tokio::test]
    async fn passthrough_is_the_fixed_501_envelope() {
        let resp = can_passthrough().await;
        // Status: 501 Not Implemented, matching the Python `status_code=501`.
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        // Content-Type is JSON (axum's `Json` sets `application/json`), the same
        // type FastAPI's `JSONResponse` sets.
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
        // Byte-exact parity with the Python `JSONResponse` body: compact, no
        // spaces, `error` before `message`.
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
