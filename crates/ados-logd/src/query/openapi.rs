//! The generated OpenAPI document for the `/v1` surface.
//!
//! Served at `/v1/openapi.json` so a CLI, a generated client, or a coding agent
//! can discover every path and filter without reading source. Built from a
//! single description of the routes and their parameters, so it stays in step
//! with the handlers (the path table here is the same set `routes.rs` mounts).
//! Descriptions are bland and technical — no internal tags, no upstream
//! attribution, no partner names.

use serde_json::{json, Value};

/// Build the OpenAPI 3.0 document describing the read surface.
pub fn document() -> Value {
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "ADOS Black Box query API",
            "description": "Read surface over the durable local logging and telemetry store: logs, telemetry, events, hardware samples, and sessions.",
            "version": "1"
        },
        "servers": [
            { "url": "/", "description": "the unix query socket or the LAN TCP port" }
        ],
        "components": {
            "parameters": {
                "from":    param("from", "query", "start of the time window (microsecond epoch, an ISO-8601 timestamp, or a relative duration like -5m)", "string"),
                "to":      param("to", "query", "end of the time window (same forms as 'from')", "string"),
                "since":   param("since", "query", "relative lower bound (alias for a relative 'from', e.g. -5m)", "string"),
                "kind":    enum_param("kind", "which table to read", &["logs", "events", "metrics", "hw"]),
                "source":  array_param("source", "filter by one or more emitting sources"),
                "metric":  array_param("metric", "filter by one or more dotted metric keys"),
                "event_kind": array_param("event_kind", "filter by one or more event kinds"),
                "level":   param("level", "query", "minimum severity (trace|debug|info|warn|error or 0..4)", "string"),
                "text":    param("text", "query", "substring match against the message or target", "string"),
                "session": param("session", "query", "restrict to one session id", "integer"),
                "limit":   param("limit", "query", "page size (default 200, capped)", "integer"),
                "cursor":  param("cursor", "query", "opaque keyset cursor from a prior page", "string")
            },
            "schemas": {
                "Envelope": {
                    "type": "object",
                    "properties": {
                        "data": { "description": "endpoint-specific rows or aggregates" },
                        "page": {
                            "type": "object",
                            "properties": {
                                "next_cursor": { "type": "string", "nullable": true },
                                "count": { "type": "integer" }
                            }
                        },
                        "meta": {
                            "type": "object",
                            "properties": {
                                "source": { "type": "string" },
                                "v": { "type": "integer" },
                                "ts": { "type": "integer" },
                                "db_lag_ms": { "type": "integer" }
                            }
                        }
                    }
                },
                "Error": {
                    "type": "object",
                    "properties": {
                        "error": {
                            "type": "object",
                            "properties": {
                                "code": { "type": "string" },
                                "message": { "type": "string" }
                            }
                        }
                    }
                }
            }
        },
        "paths": {
            "/v1/query": op(
                "Keyset-paginated rows across the logs, events, metrics, or hardware tables.",
                &["from", "to", "since", "kind", "source", "metric", "event_kind", "level", "text", "session", "limit", "cursor"]
            ),
            "/v1/tail": op(
                "Live Server-Sent-Events stream of newly-ingested rows matching the filters; replay=N sends recent context first.",
                &["kind", "source", "metric", "event_kind", "level", "text"]
            ),
            "/v1/aggregate": op(
                "Downsampled metric series for charts (bucket=auto|1s|1m|1h, agg=avg|min|max|p50|p95|last|count).",
                &["metric", "from", "to", "since", "session"]
            ),
            "/v1/export": op(
                "Streamed bulk export of a window as jsonl or jsonl.zst.",
                &["from", "to", "since", "kind", "source", "metric", "event_kind", "level", "text", "session"]
            ),
            "/v1/sessions": op(
                "List boot, flight, and manual sessions with per-session counts.",
                &["from", "to", "limit", "cursor"]
            ),
            "/v1/stats": op(
                "Store health, ingest and drop rates, and the explicit-push watermark.",
                &[]
            ),
            "/v1/healthz": op_public("Liveness and readiness of the daemon and store."),
            "/v1/openapi.json": op_public("This document.")
        }
    })
}

/// A scalar query parameter descriptor.
fn param(name: &str, location: &str, desc: &str, ty: &str) -> Value {
    json!({
        "name": name,
        "in": location,
        "required": false,
        "description": desc,
        "schema": { "type": ty }
    })
}

/// A repeated (array) query parameter descriptor.
fn array_param(name: &str, desc: &str) -> Value {
    json!({
        "name": name,
        "in": "query",
        "required": false,
        "description": desc,
        "style": "form",
        "explode": true,
        "schema": { "type": "array", "items": { "type": "string" } }
    })
}

/// An enum-valued scalar query parameter descriptor.
fn enum_param(name: &str, desc: &str, values: &[&str]) -> Value {
    json!({
        "name": name,
        "in": "query",
        "required": false,
        "description": desc,
        "schema": { "type": "string", "enum": values }
    })
}

/// A GET operation referencing shared parameter components and the envelope.
fn op(summary: &str, params: &[&str]) -> Value {
    let refs: Vec<Value> = params
        .iter()
        .map(|p| json!({ "$ref": format!("#/components/parameters/{p}") }))
        .collect();
    json!({
        "get": {
            "summary": summary,
            "parameters": refs,
            "responses": {
                "200": {
                    "description": "success",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Envelope" } } }
                },
                "400": {
                    "description": "invalid filter or cursor",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } }
                },
                "401": { "description": "missing or invalid key on the LAN port" },
                "429": { "description": "rate limited or subscriber cap reached" },
                "503": { "description": "store degraded" }
            }
        }
    })
}

/// A public GET operation (no auth, no parameters).
fn op_public(summary: &str) -> Value {
    json!({
        "get": {
            "summary": summary,
            "responses": { "200": { "description": "ok" } }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_document_lists_every_v1_path() {
        let doc = document();
        let paths = doc["paths"].as_object().unwrap();
        for p in [
            "/v1/query",
            "/v1/tail",
            "/v1/aggregate",
            "/v1/export",
            "/v1/sessions",
            "/v1/stats",
            "/v1/healthz",
            "/v1/openapi.json",
        ] {
            assert!(paths.contains_key(p), "missing path {p}");
        }
        assert_eq!(doc["openapi"], "3.0.3");
    }

    #[test]
    fn query_op_references_the_cursor_and_kind_params() {
        let doc = document();
        let params = doc["paths"]["/v1/query"]["get"]["parameters"]
            .as_array()
            .unwrap();
        let refs: Vec<&str> = params.iter().filter_map(|p| p["$ref"].as_str()).collect();
        assert!(refs.iter().any(|r| r.ends_with("/cursor")));
        assert!(refs.iter().any(|r| r.ends_with("/kind")));
    }
}
