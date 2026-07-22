//! `GET /api/config/schema` — the agent-config JSON Schema.
//!
//! Serves the machine-readable schema of the agent config (field types,
//! enums, defaults, secret markers) so a schema-driven settings UI can render
//! and validate the config surface without hand-typed forms. The body is the
//! committed asset at `schemas/agent-config.schema.json`, generated from the
//! config model by `scripts/emit_config_schema.py` and drift-guarded by
//! `tests/test_config_schema_parity.py`, embedded here at build time — the
//! route never touches the filesystem or the residual runtime.
//!
//! Fields whose values must not render in a UI carry `"x-secret": true` on
//! their property node; a consumer renders set/not-set for those instead of
//! the value. The schema describes SHAPE only (no live values), but it stays
//! behind the standard read-auth posture like every other native read.

use axum::http::header;
use axum::response::{IntoResponse, Response};

/// The committed schema asset. Included at compile time so the served bytes
/// are exactly the reviewed file; a malformed asset is caught by the unit
/// tests below (and by the Python parity guard), never at request time.
const AGENT_CONFIG_SCHEMA: &str = include_str!("../../../../schemas/agent-config.schema.json");

/// Serve the embedded schema verbatim as JSON. Infallible: no state, no I/O.
pub async fn get_config_schema() -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        AGENT_CONFIG_SCHEMA,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_schema_is_valid_json_with_the_config_shape() {
        let schema: serde_json::Value =
            serde_json::from_str(AGENT_CONFIG_SCHEMA).expect("committed schema parses");
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("schema has a properties object");
        for block in ["agent", "mavlink", "video", "network", "server", "security"] {
            assert!(props.contains_key(block), "missing top-level block {block}");
        }
        assert!(
            schema.get("$defs").is_some(),
            "schema carries its definitions"
        );
    }

    #[test]
    fn embedded_schema_marks_the_secret_fields() {
        // The emitter marks the redaction set + the plaintext credentials with
        // `x-secret: true` so a UI renders set/not-set instead of values. Pin
        // the count and one representative path so a regenerate that drops the
        // markers fails here, not on a bench.
        fn count(v: &serde_json::Value) -> usize {
            match v {
                serde_json::Value::Object(map) => {
                    let own = usize::from(map.get("x-secret") == Some(&serde_json::json!(true)));
                    own + map.values().map(count).sum::<usize>()
                }
                serde_json::Value::Array(items) => items.iter().map(count).sum(),
                _ => 0,
            }
        }
        let schema: serde_json::Value =
            serde_json::from_str(AGENT_CONFIG_SCHEMA).expect("committed schema parses");
        assert_eq!(count(&schema), 8, "secret-marker set drifted");
        assert_eq!(
            schema["$defs"]["ApiSecurityConfig"]["properties"]["api_key"]["x-secret"],
            serde_json::json!(true)
        );
    }
}
