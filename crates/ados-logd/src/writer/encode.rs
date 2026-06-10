//! Row insertion and field encoding.
//!
//! The pure database-write helpers the writer calls: insert one redacted frame
//! into its table, encode an open fields/tags/detail map to a msgpack blob, and
//! the shared microsecond-epoch clock. These touch the connection but hold no
//! writer state, so they live apart from the run loop.

use rusqlite::{params, Connection};

use ados_protocol::logd::IngestFrame;

/// Insert one frame into its table under `session`. The frame is already
/// redacted by the caller; `redacted` records whether that pass actually changed
/// a value, so the stored flag reflects a real redaction rather than the mere
/// presence of structured fields.
pub(crate) fn insert_frame(
    conn: &Connection,
    frame: &IngestFrame,
    session: i64,
    redacted: bool,
) -> Result<(), rusqlite::Error> {
    match frame {
        IngestFrame::Log(l) => {
            let fields = encode_map(&l.fields);
            let redacted = i64::from(redacted);
            conn.execute(
                "INSERT INTO logs (ts_us, session, source, level, target, msg, fields, redacted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    l.ts_us,
                    session,
                    l.source,
                    i64::from(l.level.as_u8()),
                    l.target,
                    l.msg,
                    fields,
                    redacted,
                ],
            )?;
        }
        IngestFrame::Telemetry(t) => {
            let tags = encode_map(&t.tags);
            conn.execute(
                "INSERT INTO metrics (ts_us, session, metric, value, tags) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![t.ts_us, session, t.metric, t.value, tags],
            )?;
        }
        IngestFrame::Event(e) => {
            let detail = encode_map(&e.detail);
            conn.execute(
                "INSERT INTO events (ts_us, session, kind, source, severity, detail) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    e.ts_us,
                    session,
                    e.kind,
                    e.source,
                    i64::from(e.severity.as_u8()),
                    detail,
                ],
            )?;
        }
        IngestFrame::Hw(h) => {
            // The whole snapshot rides in the signals blob; an empty snapshot
            // still encodes a valid (empty) msgpack map so the NOT NULL holds.
            let signals = rmp_serde::to_vec_named(&h.signals).unwrap_or_default();
            conn.execute(
                "INSERT INTO hw (ts_us, session, signals) VALUES (?1, ?2, ?3)",
                params![h.ts_us, session, signals],
            )?;
        }
    }
    Ok(())
}

/// Encode an open fields/tags/detail map to a msgpack blob, or `NULL` when empty
/// so an absent map does not waste a row's blob column.
pub(crate) fn encode_map(map: &ados_protocol::logd::Fields) -> Option<Vec<u8>> {
    if map.is_empty() {
        None
    } else {
        rmp_serde::to_vec_named(map).ok()
    }
}

/// The current wall-clock time in microseconds since the Unix epoch. A clock set
/// before the epoch yields zero rather than a negative timestamp.
pub fn now_us() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
