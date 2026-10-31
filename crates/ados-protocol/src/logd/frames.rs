//! Ingest frame types and their constructors.

use super::*;

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
