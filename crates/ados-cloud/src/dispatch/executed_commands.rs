//! Persisted record of executed cloud commands, keyed by command id.
//!
//! The command queue re-leases a row whose ack never landed (a lost uplink, a
//! crash between execute and ack), so the same command id can arrive more than
//! once. A command must execute at most once: the poll loop records each
//! executed id together with the ack it produced, and a repeat is answered by
//! re-sending that stored ack without running the command again. The record
//! lives at [`EXECUTED_COMMANDS_PATH`], keeps the most recent
//! [`EXECUTED_COMMANDS_MAX`] ids, and is written atomically so it survives a
//! restart.
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

/// Commands that act on the vehicle and must never run on a redelivery the
/// node has no execution record for.
const FLIGHT_COMMANDS: &[&str] = &["send_command"];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    ack: serde_json::Value,
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

/// The executed-command record, loaded once and updated after each execution.
#[derive(Debug)]
pub struct ExecutedCommands {
    path: PathBuf,
    entries: VecDeque<Entry>,
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
        }
    }

    /// The stored ack for an executed command id.
    pub fn stored_ack(&self, id: &str) -> Option<&serde_json::Value> {
        self.entries.iter().find(|e| e.id == id).map(|e| &e.ack)
    }

    /// Decide what to do with a leased command row (`_id`, `command`,
    /// `attempts`). A row without an id cannot be deduplicated and executes.
    pub fn precheck(&self, row: &serde_json::Value) -> Precheck {
        let id = row.get("_id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            return Precheck::Execute;
        }
        if let Some(ack) = self.stored_ack(id) {
            return Precheck::Replay(ack.clone());
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

    /// Record an executed command id with the ack it produced and persist the
    /// record. Empty ids are not recorded.
    pub fn record(&mut self, id: &str, ack: serde_json::Value) -> std::io::Result<()> {
        if id.is_empty() {
            return Ok(());
        }
        self.entries.retain(|e| e.id != id);
        self.entries.push_back(Entry {
            id: id.to_string(),
            ack,
        });
        while self.entries.len() > EXECUTED_COMMANDS_MAX {
            self.entries.pop_front();
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
