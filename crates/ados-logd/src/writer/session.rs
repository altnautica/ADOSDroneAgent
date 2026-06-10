//! Flight-session boundary handling.
//!
//! Rows are attributed to a session: the boot session opened at start, or the
//! flight session opened on arm and closed on disarm. This module owns the rule
//! that derives a session transition from a frame and the pure helpers that open
//! and close a session row, keeping the boundary logic in one place.

use rusqlite::{params, Connection};

use ados_protocol::logd::IngestFrame;

/// A session-boundary transition derived from a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionTransition {
    /// A flight session should open.
    Arm,
    /// The open flight session should close.
    Disarm,
}

/// Derive a session transition from a frame. Arm/disarm is signalled by an event
/// whose `reason` detail is `arm`/`disarm`, or by a `pairing`/`service` event
/// carrying the same reason field. The state-socket tap in a later chunk feeds
/// these events; this keeps the rule in one place.
pub(crate) fn session_transition(frame: &IngestFrame) -> Option<SessionTransition> {
    let IngestFrame::Event(e) = frame else {
        return None;
    };
    let reason = e.detail.get("reason").and_then(|v| v.as_str())?;
    match reason {
        "arm" => Some(SessionTransition::Arm),
        "disarm" => Some(SessionTransition::Disarm),
        _ => None,
    }
}

/// Insert a session row, returning its id.
pub(crate) fn open_session(
    conn: &Connection,
    started_us: i64,
    kind: &str,
    reason: Option<&str>,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO sessions (started_us, kind, reason) VALUES (?1, ?2, ?3)",
        params![started_us, kind, reason],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Close a session row: stamp `ended_us` and the closing `reason`.
pub(crate) fn close_session(
    conn: &Connection,
    id: i64,
    ended_us: i64,
    reason: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE sessions SET ended_us = ?1, reason = ?2 WHERE id = ?3",
        params![ended_us, reason, id],
    )?;
    Ok(())
}
