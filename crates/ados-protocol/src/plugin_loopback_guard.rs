//! The plugin loopback-guard verdict sidecar.
//!
//! The plugin-host daemon loads an nftables rule at startup that keeps a plugin
//! granted `network.outbound` off the agent's own loopback listeners, and
//! records whether it loaded here. The plugin lifecycle reads it before it
//! grants network access or renders a unit with it; the status surface reports
//! it. An absent or unreadable sidecar is `unavailable`: until the daemon has
//! loaded the rule, nothing may assume it is there.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The sidecar file name under the run dir.
pub const SIDECAR_NAME: &str = "plugin-loopback-guard.json";

/// The guard verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardState {
    /// `true` when the rule is loaded.
    pub active: bool,
    /// Why the guard is not active; empty when it is.
    #[serde(default)]
    pub reason: String,
}

impl GuardState {
    /// An inactive verdict carrying `reason`.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            active: false,
            reason: reason.into(),
        }
    }

    /// The status-surface word for this verdict.
    pub fn label(&self) -> &'static str {
        if self.active {
            "active"
        } else {
            "unavailable"
        }
    }
}

/// The canonical sidecar path (`ADOS_RUN_DIR`, default `/run/ados`).
pub fn sidecar_path() -> PathBuf {
    let run = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&run).join(SIDECAR_NAME)
}

/// Read the verdict at `path`; absent or unparseable reads `unavailable`.
pub fn read_state_at(path: &Path) -> GuardState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| GuardState::unavailable("the plugin host has not loaded the guard"))
}

/// [`read_state_at`] against the canonical sidecar.
pub fn read_state() -> GuardState {
    read_state_at(&sidecar_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_garbled_sidecar_reads_unavailable() {
        let dir = std::env::temp_dir().join(format!("ados-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SIDECAR_NAME);
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_state_at(&path).label(), "unavailable");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_state_at(&path).label(), "unavailable");
        std::fs::write(&path, r#"{"active":true}"#).unwrap();
        assert_eq!(read_state_at(&path).label(), "active");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
