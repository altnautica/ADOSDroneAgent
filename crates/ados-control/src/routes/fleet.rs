//! Fleet / mesh roster routes.
//!
//! Fleet awareness is opt-in: until the device enrolls in a fleet,
//! `/api/fleet/enrollment` reports `enrolled: false` and `/api/fleet/peers`
//! returns an empty list. The empty list is the canonical "no peers yet"
//! response, not a placeholder — callers that want richer fleet state read the
//! cloud-relay heartbeat instead.
//!
//! Both routes are static on this surface: the Python handlers return a fixed
//! `{"enrolled": false}` dict and a bare empty list respectively, with no seam
//! to read, so the native handlers reproduce those exact bodies byte-for-byte.
//! Note the asymmetry the GCS depends on: enrollment is a JSON **object**, peers
//! is a bare JSON **array** (not an object wrapping a `peers` key).

use axum::Json;
use serde_json::{json, Value};

/// `GET /api/fleet/enrollment` → `{"enrolled": false}`. Fleet enrollment is off
/// on this device, so the body is the fixed not-enrolled object. Mirrors the
/// Python `fleet.py:get_enrollment`, which returns `{"enrolled": False}`.
pub async fn get_enrollment() -> Json<Value> {
    Json(json!({ "enrolled": false }))
}

/// `GET /api/fleet/peers` → `[]`. With enrollment off, no peers are known, so
/// the body is the empty list — a bare JSON array, the steady-state "no peers
/// yet" response. Mirrors the Python `fleet.py:list_peers`, which returns `[]`.
pub async fn list_peers() -> Json<Value> {
    Json(json!([]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The enrollment body is exactly `{"enrolled": false}` — a JSON object with
    /// the single boolean key, matching the Python `{"enrolled": False}`.
    #[tokio::test]
    async fn enrollment_is_the_not_enrolled_object() {
        let Json(body) = get_enrollment().await;
        // Byte-shape parity with the Python handler's fixed dict.
        assert_eq!(body, json!({ "enrolled": false }));
        // Belt-and-suspenders: it is an object carrying exactly one key.
        let obj = body.as_object().expect("enrollment body is an object");
        assert_eq!(obj.len(), 1);
        assert_eq!(obj.get("enrolled"), Some(&Value::Bool(false)));
    }

    /// The peers body is exactly `[]` — a bare JSON array, NOT an object wrapping
    /// a `peers` key. This asymmetry with the enrollment route is part of the
    /// contract the GCS relies on, so the test pins the array shape explicitly.
    #[tokio::test]
    async fn peers_is_the_empty_array() {
        let Json(body) = list_peers().await;
        // Byte-shape parity with the Python handler's bare `[]`.
        assert_eq!(body, json!([]));
        // It is an array (not an object) and it is empty.
        let arr = body.as_array().expect("peers body is an array");
        assert!(arr.is_empty());
        assert!(
            !body.is_object(),
            "peers must be a bare array, not an object"
        );
    }
}
