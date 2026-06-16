//! The `/run/ados/gpio-output.json` state sidecar: the current driven line
//! states, written atomically (tmp sibling + fsync + rename) so a reader never
//! sees a half-written file. Modelled on `ados-hid`'s `ground-station-input.json`
//! persister and `ados-net`'s `write_atomic`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Level;

/// Canonical GPIO-output state sidecar path under the runtime dir (tmpfs; wiped
/// on reboot, which is correct — a fresh boot drives no line until commanded).
pub const GPIO_OUTPUT_PATH: &str = "/run/ados/gpio-output.json";

/// One driven line's reported state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineState {
    /// The chip index (`N` in `/dev/gpiochipN`).
    pub chip: u32,
    /// The line offset on that chip.
    pub pin: u32,
    /// The level the line is currently held at.
    pub level: Level,
}

/// The full `gpio-output.json` payload: the set of lines the service is driving.
/// An empty list means the service is up but has driven nothing yet (the
/// safe-by-default state).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpioOutputState {
    /// The driven lines, in a stable order.
    #[serde(default)]
    pub lines: Vec<LineState>,
}

impl GpioOutputState {
    /// Build a state blob from the `(chip, pin, level)` triples a driver
    /// snapshot yields.
    pub fn from_snapshot(snapshot: &[(u32, u32, Level)]) -> Self {
        Self {
            lines: snapshot
                .iter()
                .map(|(chip, pin, level)| LineState {
                    chip: *chip,
                    pin: *pin,
                    level: *level,
                })
                .collect(),
        }
    }

    /// Read the state from `path`. Returns `None` when the file is missing or
    /// malformed; the caller then treats the service as having driven nothing.
    pub fn load(path: &Path) -> Option<GpioOutputState> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Atomically persist the state to `path` (tmp sibling + fsync + rename),
    /// creating the parent. Compact JSON separators.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        atomic_write(path, &body)
    }
}

/// Atomic tmp-sibling write. The tmp name is disambiguated by pid so two writers
/// in the same directory never collide. Mirrors the `ados-hid` sidecar persister.
fn atomic_write(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ados-sidecar");
    let tmp = parent.join(format!("{}.{}.tmp", file_name, std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpio-output.json");
        let state = GpioOutputState::from_snapshot(&[(0, 17, Level::High), (1, 4, Level::Low)]);
        state.save(&path).unwrap();

        let loaded = GpioOutputState::load(&path).unwrap();
        assert_eq!(loaded, state);
        assert_eq!(loaded.lines.len(), 2);
        assert_eq!(loaded.lines[0].pin, 17);
        assert_eq!(loaded.lines[0].level, Level::High);

        // Compact separators, no stray tmp.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(", "));
        assert!(!text.contains(": "));
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray);
    }

    #[test]
    fn empty_state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpio-output.json");
        let state = GpioOutputState::default();
        state.save(&path).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(v["lines"].as_array().unwrap().is_empty());
        assert_eq!(GpioOutputState::load(&path).unwrap(), state);
    }

    #[test]
    fn load_missing_or_malformed_is_none() {
        assert!(GpioOutputState::load(Path::new("/nonexistent/gpio-output.json")).is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpio-output.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(GpioOutputState::load(&path).is_none());
    }

    #[test]
    fn path_constant_is_under_the_run_dir() {
        assert_eq!(GPIO_OUTPUT_PATH, "/run/ados/gpio-output.json");
    }
}
