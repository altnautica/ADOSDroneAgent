//! Wire contracts for the local logging and telemetry service.
//!
//! The agent records logs from every process, telemetry history, discrete
//! events, and hardware samples into one durable local store. This module
//! carries the shared types that travel between the producers (Rust services,
//! the Python agent) and the writer, plus the request/response envelope the
//! read API and its clients share.
//!
//! All ingest frames are msgpack with short field-rename keys and a `v` version
//! byte, so a producer and a writer on different versions stay forward
//! compatible: a reader that sees an unknown `v` can skip the frame, and new
//! fields ride in the open `fields`/`tags`/`extra` maps without a schema
//! change. The frames travel over the same 4-byte big-endian length framing the
//! other sockets use ([`crate::frame`]); this module reuses that codec and does
//! not duplicate it.
//!
//! Redaction ([`redact`]) hashes any field whose key looks secret-bearing. It
//! mirrors the agent's structured-logging redaction so no value is ever written
//! to the store in the clear, regardless of which producer emitted it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::frame;

/// The producer-side `tracing` layer that ships log events to the logging
/// daemon's ingest socket. Behind the `tracing-layer` feature so consumers that
/// do not need it (the daemon itself, build tools, the interactive client) stay
/// lean.
#[cfg(feature = "tracing-layer")]
pub mod layer;

/// Maximum ingest frame payload. Log records and hardware snapshots are small;
/// the cap is generous headroom and guards against a runaway producer.
pub const LOGD_MAX_FRAME: usize = 1024 * 1024;

/// The current wire version stamped on every ingest frame.
pub const WIRE_VERSION: u8 = 1;

/// The current read-envelope version reported in [`Meta::v`].
pub const ENVELOPE_VERSION: u8 = 1;

/// Idempotency sentinel: a value already prefixed with this has been redacted
/// and is passed through unchanged.
pub const REDACT_PREFIX: &str = "redacted:";

/// Key suffixes (and exact keys, case-insensitive) that mark a field as
/// secret-bearing. Kept in lockstep with the agent's logging redaction.
pub const SECRET_SUFFIXES: [&str; 5] = ["key", "code", "token", "password", "secret"];

/// Severity levels, matching the integer encoding stored in the log table.
/// `Trace`/`Debug` are the high-volume levels dropped first under backpressure;
/// `Warn` and above are always kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// 0 — most verbose.
    Trace,
    /// 1.
    Debug,
    /// 2.
    Info,
    /// 3.
    Warn,
    /// 4 — most severe.
    Error,
}

impl Level {
    /// The integer encoding stored in the log table.
    pub fn as_u8(self) -> u8 {
        match self {
            Level::Trace => 0,
            Level::Debug => 1,
            Level::Info => 2,
            Level::Warn => 3,
            Level::Error => 4,
        }
    }

    /// Map an integer back to a level, clamping out-of-range values to `Error`.
    pub fn from_u8(n: u8) -> Level {
        match n {
            0 => Level::Trace,
            1 => Level::Debug,
            2 => Level::Info,
            3 => Level::Warn,
            _ => Level::Error,
        }
    }
}

/// Errors raised encoding or decoding an ingest frame.
#[derive(Debug, Error)]
pub enum LogdError {
    #[error("msgpack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("framing error: {0}")]
    Frame(#[from] frame::FrameError),
    #[error("unsupported wire version {got} (this build speaks {ours})")]
    Version { got: u8, ours: u8 },
}

/// An open msgpack map of extra fields. Values are arbitrary so new telemetry
/// round-trips without a schema change (mirrors the open-state pattern).
pub type Fields = BTreeMap<String, rmpv::Value>;

/// One log record from any producer (Rust service, Python agent, subprocess
/// tap). The `src` is the emitting process or component; `tgt` is the module or
/// logger target. Structured fields ride in the open `f` map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogFrame {
    /// Wire version.
    #[serde(rename = "v")]
    pub v: u8,
    /// Microsecond epoch timestamp.
    #[serde(rename = "ts")]
    pub ts_us: i64,
    /// Emitting process or component (e.g. the daemon name).
    #[serde(rename = "src")]
    pub source: String,
    /// Severity level.
    #[serde(rename = "lvl")]
    pub level: Level,
    /// Module or logger target.
    #[serde(rename = "tgt", default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The rendered message.
    #[serde(rename = "msg")]
    pub msg: String,
    /// Open structured-fields map.
    #[serde(rename = "f", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: Fields,
}

impl LogFrame {
    /// Build a record stamped with the current wire version.
    pub fn new(
        ts_us: i64,
        source: impl Into<String>,
        level: Level,
        msg: impl Into<String>,
    ) -> Self {
        Self {
            v: WIRE_VERSION,
            ts_us,
            source: source.into(),
            level,
            target: None,
            msg: msg.into(),
            fields: Fields::new(),
        }
    }

    /// Redact every secret-bearing field in place (idempotent). Returns `true`
    /// if any field value was changed.
    pub fn redact_fields(&mut self) -> bool {
        redact_map(&mut self.fields)
    }
}

/// One telemetry sample: a single dotted metric key with a numeric value and an
/// open tags map. Replaces the per-field history arrays sampled in the GCS.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryFrame {
    /// Wire version.
    #[serde(rename = "v")]
    pub v: u8,
    /// Microsecond epoch timestamp.
    #[serde(rename = "ts")]
    pub ts_us: i64,
    /// Dotted metric key (e.g. `cpu.load`, `link.rssi_dbm`).
    #[serde(rename = "m")]
    pub metric: String,
    /// The numeric value.
    #[serde(rename = "val")]
    pub value: f64,
    /// Open tag map (dimensions such as iface, core index, zone).
    #[serde(rename = "tg", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: Fields,
}

impl TelemetryFrame {
    /// Build a sample stamped with the current wire version.
    pub fn new(ts_us: i64, metric: impl Into<String>, value: f64) -> Self {
        Self {
            v: WIRE_VERSION,
            ts_us,
            metric: metric.into(),
            value,
            tags: Fields::new(),
        }
    }
}

/// One discrete event: a state transition, a radio lock change, a sidecar drop,
/// a pairing change, a transport error. `kind` is a dotted classifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventFrame {
    /// Wire version.
    #[serde(rename = "v")]
    pub v: u8,
    /// Microsecond epoch timestamp.
    #[serde(rename = "ts")]
    pub ts_us: i64,
    /// Dotted event classifier (e.g. `service.transition`, `radio.lock`).
    #[serde(rename = "k")]
    pub kind: String,
    /// Emitting component.
    #[serde(rename = "src")]
    pub source: String,
    /// Severity level.
    #[serde(rename = "sev")]
    pub severity: Level,
    /// Open detail map.
    #[serde(rename = "d", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detail: Fields,
}

impl EventFrame {
    /// Build an event stamped with the current wire version.
    pub fn new(
        ts_us: i64,
        kind: impl Into<String>,
        source: impl Into<String>,
        severity: Level,
    ) -> Self {
        Self {
            v: WIRE_VERSION,
            ts_us,
            kind: kind.into(),
            source: source.into(),
            severity,
            detail: Fields::new(),
        }
    }

    /// Redact every secret-bearing field in the detail map (idempotent). Returns
    /// `true` if any field value was changed.
    pub fn redact_detail(&mut self) -> bool {
        redact_map(&mut self.detail)
    }
}

/// A periodic hardware sample: temperatures, per-core frequency, power rails,
/// pressure-stall, per-iface counters, USB speed, throttle flags. Every reading
/// rides in the open `s` map so a new signal does not need a schema bump.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HwSnapshot {
    /// Wire version.
    #[serde(rename = "v")]
    pub v: u8,
    /// Microsecond epoch timestamp.
    #[serde(rename = "ts")]
    pub ts_us: i64,
    /// Open map of dotted signal keys to numeric or string readings.
    #[serde(rename = "s", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub signals: Fields,
}

impl HwSnapshot {
    /// Build an empty snapshot stamped with the current wire version.
    pub fn new(ts_us: i64) -> Self {
        Self {
            v: WIRE_VERSION,
            ts_us,
            signals: Fields::new(),
        }
    }
}

/// The kind tag on an ingest frame, so the writer can dispatch one stream of
/// mixed frames to the right table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum IngestFrame {
    /// A log record.
    Log(LogFrame),
    /// A telemetry sample.
    Telemetry(TelemetryFrame),
    /// A discrete event.
    Event(EventFrame),
    /// A hardware snapshot.
    Hw(HwSnapshot),
}

impl IngestFrame {
    /// Encode this frame as a complete length-prefixed msgpack frame ready to
    /// write on the ingest socket.
    pub fn encode(&self) -> Result<Vec<u8>, LogdError> {
        let body = rmp_serde::to_vec_named(self)?;
        Ok(frame::encode_frame(&body, LOGD_MAX_FRAME)?)
    }

    /// Decode one msgpack body (the frame payload, without the length prefix).
    /// Rejects a frame whose wire version this build does not speak.
    pub fn decode(body: &[u8]) -> Result<Self, LogdError> {
        let frame: IngestFrame = rmp_serde::from_slice(body)?;
        let v = frame.version();
        if v != WIRE_VERSION {
            return Err(LogdError::Version {
                got: v,
                ours: WIRE_VERSION,
            });
        }
        Ok(frame)
    }

    /// The wire version carried by the inner frame.
    pub fn version(&self) -> u8 {
        match self {
            IngestFrame::Log(f) => f.v,
            IngestFrame::Telemetry(f) => f.v,
            IngestFrame::Event(f) => f.v,
            IngestFrame::Hw(f) => f.v,
        }
    }

    /// Redact secret-bearing fields on the inner frame in place. Returns `true`
    /// if any field value was actually changed, so a writer can record on the row
    /// whether redaction touched it.
    pub fn redact(&mut self) -> bool {
        match self {
            IngestFrame::Log(f) => f.redact_fields(),
            IngestFrame::Event(f) => f.redact_detail(),
            // Telemetry tags and hardware signals carry no secret material; the
            // metric/signal keys are fixed dotted names, not free-form fields.
            IngestFrame::Telemetry(_) | IngestFrame::Hw(_) => false,
        }
    }
}

// --- read API request/response envelope ---------------------------------

/// A read request against the query API. All fields are optional; an empty
/// request returns the most recent page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueryRequest {
    /// Inclusive lower bound, microsecond epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    /// Exclusive upper bound, microsecond epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<i64>,
    /// Restrict to these dotted metric keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric: Vec<String>,
    /// Restrict to these emitting sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source: Vec<String>,
    /// Minimum severity level (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    /// Free-text substring match against the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Restrict to one session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<i64>,
    /// Page size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Opaque keyset cursor returned by the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Pagination block: the cursor for the next page and the count in this page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Page {
    /// Opaque keyset cursor for the next page, absent when exhausted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Number of rows in this page.
    pub count: u32,
}

/// Response metadata: which plane served it, the envelope version, the server
/// time, and how far the writer is behind real time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    /// The serving plane (e.g. `local` for the unix socket, `lan` for TCP).
    pub source: String,
    /// Envelope version.
    pub v: u8,
    /// Server time, microsecond epoch.
    pub ts: i64,
    /// Writer lag behind real time, milliseconds. Zero when fully caught up.
    pub db_lag_ms: i64,
}

/// The success envelope shared by every read endpoint. `data` is the typed
/// payload (rows, buckets, sessions, stats), `page` carries pagination, `meta`
/// carries serving metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryResponse<T> {
    /// The result payload.
    pub data: T,
    /// Pagination block.
    pub page: Page,
    /// Serving metadata.
    pub meta: Meta,
}

/// The error body returned for a failed request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiError {
    /// Stable machine-readable error code.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

/// The error envelope: `{ "error": { "code", "message" } }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorEnvelope {
    /// The error body.
    pub error: ApiError,
}

impl ErrorEnvelope {
    /// Build an error envelope.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ApiError {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

// --- redaction ----------------------------------------------------------

/// True when `key` marks a secret-bearing field: a case-insensitive match where
/// the lower-cased key ends with, or exactly equals, one of [`SECRET_SUFFIXES`].
pub fn is_secret_key(key: &str) -> bool {
    let kl = key.to_ascii_lowercase();
    SECRET_SUFFIXES.iter().any(|s| kl.ends_with(s) || kl == *s)
}

/// Redact one string value for the given key.
///
/// Mirrors the agent's structured-logging redaction byte-for-byte:
///
/// - if `key` is not secret-bearing, the value is returned unchanged;
/// - if the value is empty, it is returned unchanged;
/// - if the value already carries the [`REDACT_PREFIX`] sentinel, it is returned
///   unchanged (idempotent — a value that traverses the chain twice is not
///   double-hashed);
/// - otherwise the value is replaced with `redacted:<first 4 chars>...<first 8
///   hex chars of the SHA-256 digest>`.
///
/// The head is the first four Unicode characters (not bytes), matching Python's
/// `v[:4]` slice; the digest is over the UTF-8 bytes.
pub fn redact(key: &str, value: &str) -> String {
    if value.is_empty() || value.starts_with(REDACT_PREFIX) {
        return value.to_string();
    }
    if !is_secret_key(key) {
        return value.to_string();
    }
    let head: String = value.chars().take(4).collect();
    let digest = Sha256::digest(value.as_bytes());
    let hex = hex::encode(digest);
    let short = &hex[..8];
    format!("{REDACT_PREFIX}{head}...{short}")
}

/// Redact every secret-bearing string field of an open map in place. Non-string
/// values, empty strings, and already-redacted values are left untouched.
/// Returns `true` if at least one field value was changed.
pub fn redact_map(fields: &mut Fields) -> bool {
    let mut changed = false;
    for (k, v) in fields.iter_mut() {
        if let rmpv::Value::String(s) = v {
            if let Some(text) = s.as_str() {
                let redacted = redact(k, text);
                if redacted != text {
                    *v = rmpv::Value::from(redacted);
                    changed = true;
                }
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::Value as MpVal;

    fn mk_fields(pairs: &[(&str, &str)]) -> Fields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), MpVal::from(*v)))
            .collect()
    }

    #[test]
    fn log_frame_round_trips_through_an_ingest_frame() {
        let mut log = LogFrame::new(1_700_000_000_000_000, "ados-logd", Level::Warn, "hello");
        log.target = Some("logd::db".to_string());
        log.fields.insert("attempt".to_string(), MpVal::from(3u64));
        let frame = IngestFrame::Log(log.clone());

        let wire = frame.encode().unwrap();
        // 4-byte big-endian length prefix + body, same framing as the others.
        assert_eq!(&wire[..4], &(wire.len() as u32 - 4).to_be_bytes());

        let back = IngestFrame::decode(&wire[frame::HEADER_SIZE..]).unwrap();
        assert_eq!(back, IngestFrame::Log(log));
    }

    #[test]
    fn telemetry_event_and_hw_frames_round_trip() {
        let tele = IngestFrame::Telemetry(TelemetryFrame::new(1, "cpu.load", 0.42));
        let evt = IngestFrame::Event(EventFrame::new(2, "radio.lock", "ados-radio", Level::Info));
        let mut hw = HwSnapshot::new(3);
        hw.signals
            .insert("thermal.soc_c".to_string(), MpVal::from(54.5));
        let hw = IngestFrame::Hw(hw);

        for frame in [tele, evt, hw] {
            let wire = frame.encode().unwrap();
            let back = IngestFrame::decode(&wire[frame::HEADER_SIZE..]).unwrap();
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn decode_rejects_an_unknown_wire_version() {
        let mut log = LogFrame::new(1, "src", Level::Info, "m");
        log.v = 99;
        let frame = IngestFrame::Log(log);
        let wire = frame.encode().unwrap();
        let err = IngestFrame::decode(&wire[frame::HEADER_SIZE..]).unwrap_err();
        assert!(matches!(err, LogdError::Version { got: 99, ours: 1 }));
    }

    #[test]
    fn level_integer_encoding_round_trips() {
        for lvl in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
        ] {
            assert_eq!(Level::from_u8(lvl.as_u8()), lvl);
        }
        // Out-of-range integers clamp to the most severe level.
        assert_eq!(Level::from_u8(200), Level::Error);
    }

    #[test]
    fn query_response_envelope_serializes_with_the_expected_shape() {
        let resp = QueryResponse {
            data: vec![1u32, 2, 3],
            page: Page {
                next_cursor: Some("abc".to_string()),
                count: 3,
            },
            meta: Meta {
                source: "local".to_string(),
                v: ENVELOPE_VERSION,
                ts: 42,
                db_lag_ms: 0,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["data"], serde_json::json!([1, 2, 3]));
        assert_eq!(json["page"]["next_cursor"], "abc");
        assert_eq!(json["page"]["count"], 3);
        assert_eq!(json["meta"]["source"], "local");
        assert_eq!(json["meta"]["v"], 1);
        assert_eq!(json["meta"]["db_lag_ms"], 0);
    }

    #[test]
    fn error_envelope_shape() {
        let err = ErrorEnvelope::new("bad_request", "unknown filter");
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"]["code"], "bad_request");
        assert_eq!(json["error"]["message"], "unknown filter");
    }

    #[test]
    fn is_secret_key_matches_suffix_and_exact_case_insensitively() {
        assert!(is_secret_key("api_key"));
        assert!(is_secret_key("KEY"));
        assert!(is_secret_key("pairing_code"));
        assert!(is_secret_key("token"));
        assert!(is_secret_key("Password"));
        assert!(is_secret_key("device_secret"));
        // Not secret-bearing.
        assert!(!is_secret_key("device_id"));
        assert!(!is_secret_key("keyboard")); // "key" is a prefix, not a suffix
        assert!(!is_secret_key("message"));
    }

    #[test]
    fn redact_leaves_non_secret_and_empty_and_sentinel_values_untouched() {
        // Non-secret key: unchanged.
        assert_eq!(redact("device_id", "abc123"), "abc123");
        // Empty value: unchanged even for a secret key.
        assert_eq!(redact("api_key", ""), "");
        // Already redacted: idempotent.
        let once = redact("api_key", "ABCDEFGHIJ1234567890");
        assert_eq!(redact("api_key", &once), once);
    }

    /// Byte-for-byte parity with the agent's structured-logging redaction. The
    /// expected values were computed from the reference implementation; the
    /// digest fragment is the first 8 hex chars of SHA-256 over the UTF-8 value.
    #[test]
    fn redact_parity_vectors() {
        let vectors: &[(&str, &str, &str)] = &[
            (
                "api_key",
                "ABCDEFGHIJ1234567890",
                "redacted:ABCD...bb2a0cee",
            ),
            ("pairing_code", "999888", "redacted:9998...685f188e"),
            ("token", "tok_supersecretvalue", "redacted:tok_...160e465f"),
            ("password", "hunter2", "redacted:hunt...f52fbd32"),
            ("secret", "s", "redacted:s...043a7187"),
            (
                "device_secret",
                "0xDEADBEEFCAFE",
                "redacted:0xDE...c19821b8",
            ),
        ];
        for (key, value, expected) in vectors {
            assert_eq!(&redact(key, value), expected, "key={key} value={value}");
            // Idempotent: feeding the redacted output back yields the same string.
            assert_eq!(&redact(key, expected), expected, "idempotent key={key}");
        }
    }

    #[test]
    fn redact_map_redacts_only_secret_string_fields() {
        let mut fields = mk_fields(&[("api_key", "ABCDEFGHIJ1234567890"), ("device_id", "abc123")]);
        fields.insert("attempt".to_string(), MpVal::from(7u64)); // non-string, skipped
        let changed = redact_map(&mut fields);
        assert!(changed, "redaction changed the secret field");
        assert_eq!(
            fields.get("api_key").and_then(|v| v.as_str()),
            Some("redacted:ABCD...bb2a0cee")
        );
        assert_eq!(
            fields.get("device_id").and_then(|v| v.as_str()),
            Some("abc123")
        );
        assert_eq!(fields.get("attempt").and_then(|v| v.as_u64()), Some(7));
    }

    #[test]
    fn redact_map_reports_no_change_when_nothing_is_secret() {
        // A map with only non-secret and already-redacted values reports no
        // change, so a caller can record an accurate redaction flag on the row.
        let mut fields = mk_fields(&[
            ("device_id", "abc123"),
            ("api_key", "redacted:ABCD...bb2a0cee"),
        ]);
        let changed = redact_map(&mut fields);
        assert!(!changed, "no value should change");
        assert_eq!(
            fields.get("api_key").and_then(|v| v.as_str()),
            Some("redacted:ABCD...bb2a0cee")
        );
    }

    #[test]
    fn frame_level_redaction_is_applied_through_ingest() {
        let mut log = LogFrame::new(1, "src", Level::Info, "m");
        log.fields.insert(
            "session_token".to_string(),
            MpVal::from("tok_supersecretvalue"),
        );
        let mut frame = IngestFrame::Log(log);
        frame.redact();
        if let IngestFrame::Log(l) = &frame {
            assert_eq!(
                l.fields.get("session_token").and_then(|v| v.as_str()),
                Some("redacted:tok_...160e465f")
            );
        } else {
            panic!("expected log frame");
        }
    }
}
