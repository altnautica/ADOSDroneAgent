//! Persisted record of executed cloud commands, keyed by command id.
//!
//! The command queue re-leases a row whose ack never landed (a lost uplink, a
//! crash between execute and ack), so the same command id can arrive more than
//! once. A command must execute at most once: the poll loop records each id
//! before it runs ([`ExecutedCommands::begin`]) and again with the ack it
//! produced, and a repeat is answered by re-sending that stored ack without
//! running the command again. An id the record shows started but never
//! finished was cut short by a restart: it may have acted, so it is answered
//! `failed` ([`INTERRUPTED`]) and never run again. The record lives at
//! [`EXECUTED_COMMANDS_PATH`], keeps the most recent [`EXECUTED_COMMANDS_MAX`]
//! ids, and is written atomically so it survives a restart.
//!
//! The record is also the ack outbox: an ack stays undelivered until the queue
//! answers it, across restarts, and the poll loop sends the undelivered ones
//! before it claims again.
//!
//! A flight command (`send_command`) that the queue has delivered before but
//! this node has no record of executing is refused rather than run: the earlier
//! delivery may have reached the vehicle before the node lost its record, and
//! an arm or takeoff must never run a second time minutes later.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CommandResult;

/// Where the executed-command record lives. Under `/var/lib` so it survives a
/// reboot.
pub const EXECUTED_COMMANDS_PATH: &str = "/var/lib/ados/cloud-executed-commands.json";

/// How many executed command ids the record keeps (oldest evicted first).
pub const EXECUTED_COMMANDS_MAX: usize = 256;

/// The reason a command cut short by a restart is answered with.
pub const INTERRUPTED: &str =
    "command interrupted: the node restarted while it ran; its outcome is unknown and it is not run again";

/// Commands that act on the vehicle and must never run on a redelivery the
/// node has no execution record for.
const FLIGHT_COMMANDS: &[&str] = &["send_command"];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    /// The ack the command produced; `None` while it runs.
    #[serde(default)]
    ack: Option<serde_json::Value>,
    /// Whether the queue has the ack. A record written before the outbox
    /// existed holds only acks it already sent.
    #[serde(default = "sent_before")]
    delivered: bool,
}

fn sent_before() -> bool {
    true
}

/// What the poll loop does with one leased command.
#[derive(Debug, Clone, PartialEq)]
pub enum Precheck {
    /// Already executed: re-send this stored ack, do not execute.
    Replay(serde_json::Value),
    /// Do not execute; ack this result.
    Refuse(CommandResult),
    /// First delivery: execute it.
    Execute,
}

/// The executed-command record, loaded once and updated around each execution.
#[derive(Debug)]
pub struct ExecutedCommands {
    path: PathBuf,
    entries: VecDeque<Entry>,
    /// Since when undelivered acks have failed to go out (process memory).
    pub(crate) outbox_stalled_since: Option<tokio::time::Instant>,
}

impl ExecutedCommands {
    /// Load the record from `path`. A missing file is an empty record; an
    /// unreadable or corrupt one is logged and treated as empty.
    pub fn load(path: &Path) -> Self {
        let entries = match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<VecDeque<Entry>>(&bytes) {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "executed-command record unreadable; starting empty");
                    VecDeque::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => VecDeque::new(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "executed-command record unreadable; starting empty");
                VecDeque::new()
            }
        };
        Self {
            path: path.to_path_buf(),
            entries,
            outbox_stalled_since: None,
        }
    }

    /// The stored ack for an executed command id.
    pub fn stored_ack(&self, id: &str) -> Option<&serde_json::Value> {
        self.entry(id).and_then(|e| e.ack.as_ref())
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Decide what to do with a leased command row (`_id`, `command`,
    /// `attempts`). A row without an id cannot be deduplicated and executes.
    pub fn precheck(&self, row: &serde_json::Value) -> Precheck {
        let id = row.get("_id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            return Precheck::Execute;
        }
        if let Some(entry) = self.entry(id) {
            return match &entry.ack {
                Some(ack) => Precheck::Replay(ack.clone()),
                None => Precheck::Refuse(CommandResult::failed(INTERRUPTED)),
            };
        }
        let name = row.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let attempts = row.get("attempts").and_then(|v| v.as_u64()).unwrap_or(0);
        if attempts > 1 && FLIGHT_COMMANDS.contains(&name) {
            return Precheck::Refuse(CommandResult::failed(
                "flight command redelivered with no record of execution; not executed",
            ));
        }
        Precheck::Execute
    }

    /// Record that a command id starts running, before it acts. Empty ids are
    /// not recorded.
    pub fn begin(&mut self, id: &str) -> std::io::Result<()> {
        self.put(id, None)
    }

    /// Record a command id with the ack it produced, undelivered, and persist
    /// the record. Empty ids are not recorded.
    pub fn record(&mut self, id: &str, ack: serde_json::Value) -> std::io::Result<()> {
        self.put(id, Some(ack))
    }

    fn put(&mut self, id: &str, ack: Option<serde_json::Value>) -> std::io::Result<()> {
        if id.is_empty() {
            return Ok(());
        }
        self.entries.retain(|e| e.id != id);
        self.entries.push_back(Entry {
            id: id.to_string(),
            ack,
            delivered: false,
        });
        while self.entries.len() > EXECUTED_COMMANDS_MAX {
            self.entries.pop_front();
        }
        self.save()
    }

    /// Ids that started and never finished: a restart cut them short.
    pub fn interrupted(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.ack.is_none())
            .map(|e| e.id.clone())
            .collect()
    }

    /// The acks the queue does not have yet, oldest first.
    pub fn undelivered(&self) -> Vec<serde_json::Value> {
        self.entries
            .iter()
            .filter(|e| !e.delivered)
            .filter_map(|e| e.ack.clone())
            .collect()
    }

    /// Mark an id's ack delivered (no-op for an id not in the record).
    pub fn delivered(&mut self, id: &str) -> std::io::Result<()> {
        match self.entries.iter_mut().find(|e| e.id == id) {
            Some(e) if !e.delivered => e.delivered = true,
            _ => return Ok(()),
        }
        self.save()
    }

    /// Stop holding every undelivered ack.
    pub fn drop_undelivered(&mut self) -> std::io::Result<()> {
        for e in self.entries.iter_mut().filter(|e| e.ack.is_some()) {
            e.delivered = true;
        }
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".cloud-executed-commands.{}.tmp",
            std::process::id()
        ));
        let body = serde_json::to_vec(&self.entries).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests;
