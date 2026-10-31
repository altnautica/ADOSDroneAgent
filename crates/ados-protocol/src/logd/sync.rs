//! Cloud sync request/response types.

use super::*;

/// One of the four durable tables a window export and its mark-synced selector
/// can name. Serializes to its lowercase table name so the wire vocabulary
/// matches the `kind` query parameter the read API already speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncTable {
    /// The `logs` table.
    Logs,
    /// The `metrics` table.
    Metrics,
    /// The `events` table.
    Events,
    /// The `hw` table.
    Hw,
}

impl SyncTable {
    /// The SQL/table name this variant maps to.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Logs => "logs",
            Self::Metrics => "metrics",
            Self::Events => "events",
            Self::Hw => "hw",
        }
    }

    /// Every table, in a fixed order. Used to expand an empty selector to all
    /// four and to report the remaining unsynced count per table.
    pub const ALL: [SyncTable; 4] = [Self::Logs, Self::Metrics, Self::Events, Self::Hw];
}

/// The window selector a mark-synced request carries. An exported window is
/// flipped from unsynced to synced over exactly this selector, so it shares the
/// vocabulary of the export filter: an optional session, a half-open
/// `[from_us, to_us)` microsecond window, and an optional table list. An empty
/// `tables` means all four. All fields absent marks every unsynced row.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SyncRequest {
    /// Restrict the mark to one session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<i64>,
    /// Inclusive lower bound, microsecond epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_us: Option<i64>,
    /// Exclusive upper bound, microsecond epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_us: Option<i64>,
    /// The tables to mark; empty means all four.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<SyncTable>,
}

impl SyncRequest {
    /// The requested tables, expanding an empty selector to all four.
    pub fn tables_or_all(&self) -> Vec<SyncTable> {
        if self.tables.is_empty() {
            SyncTable::ALL.to_vec()
        } else {
            self.tables.clone()
        }
    }
}

/// The result of a mark-synced request: the count flipped per requested table,
/// and the remaining unsynced count per table after the flip (always all four,
/// so a caller sees the whole store's pending-push watermark in one response).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SyncResponse {
    /// Rows flipped to synced, keyed by table name.
    pub marked: BTreeMap<String, i64>,
    /// Rows still unsynced after the flip, keyed by table name (all four).
    pub unsynced_after: BTreeMap<String, i64>,
}
