//! Per-aircraft setup state on disk: `<dir>/<uid>/state.json` and the
//! parameter snapshots `<dir>/<uid>/<iso>.param`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::schema::{SnapshotRef, StepState, WizardState};
use super::step::WizardData;

/// Default base directory.
pub const DEFAULT_DIR: &str = "/var/ados/fc-setup";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StepRecord {
    pub state: Option<StepState>,
    pub result: Option<Map<String, Value>>,
    pub at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WizardRecord {
    pub state: WizardState,
    pub completed_at: Option<String>,
    pub steps: Map<String, Value>,
}

impl Default for WizardRecord {
    fn default() -> Self {
        WizardRecord {
            state: WizardState::NotStarted,
            completed_at: None,
            steps: Map::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AircraftState {
    pub v: u32,
    pub firmware: Option<String>,
    pub firmware_version: Option<String>,
    pub vehicle_class: Option<String>,
    /// Where the identity this state is filed under came from.
    pub uid_source: Option<super::fcstate::IdSource>,
    pub ground: WizardRecord,
    pub air: WizardRecord,
    pub data: WizardData,
    pub updated_at: Option<String>,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Store { dir: dir.into() }
    }

    fn uid_dir(&self, uid: &str) -> PathBuf {
        self.dir.join(uid)
    }

    pub fn load(&self, uid: &str) -> AircraftState {
        let p = self.uid_dir(uid).join("state.json");
        std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| AircraftState {
                v: 1,
                ..Default::default()
            })
    }

    pub fn save(&self, uid: &str, s: &AircraftState) -> std::io::Result<()> {
        let d = self.uid_dir(uid);
        std::fs::create_dir_all(&d)?;
        let tmp = d.join("state.json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(tmp, d.join("state.json"))
    }

    /// Writes a snapshot; returns its file name.
    pub fn write_snapshot(&self, uid: &str, iso: &str, body: &str) -> std::io::Result<String> {
        let d = self.uid_dir(uid);
        std::fs::create_dir_all(&d)?;
        let name = format!("{}.param", iso.replace(':', "-"));
        std::fs::write(d.join(&name), body)?;
        Ok(name)
    }

    pub fn read_snapshot(&self, uid: &str, name: &str) -> Option<String> {
        if name.contains('/') || name.contains("..") || !name.ends_with(".param") {
            return None;
        }
        std::fs::read_to_string(self.uid_dir(uid).join(name)).ok()
    }

    pub fn snapshots(&self, uid: &str) -> Vec<SnapshotRef> {
        let mut v: Vec<SnapshotRef> = std::fs::read_dir(self.uid_dir(uid))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name.strip_suffix(".param")?;
                Some(SnapshotRef {
                    ts: stem.to_string(),
                    file: name,
                })
            })
            .collect();
        v.sort_by(|a, b| a.file.cmp(&b.file));
        v
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix time in ms.
pub fn iso_utc(epoch_ms: u64) -> String {
    let secs = epoch_ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_formats_known_instants() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_794_268_800_000), "2026-11-10T00:00:00Z");
        assert_eq!(iso_utc(951_782_400_000 + 3_723_000), "2000-02-29T01:02:03Z");
    }

    #[test]
    fn snapshot_names_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path());
        let name = s
            .write_snapshot("ab", "2026-11-07T10:00:00Z", "X,1\n")
            .unwrap();
        assert_eq!(name, "2026-11-07T10-00-00Z.param");
        assert_eq!(s.read_snapshot("ab", &name).as_deref(), Some("X,1\n"));
        assert!(s.read_snapshot("ab", "../state.json").is_none());
        assert_eq!(s.snapshots("ab").len(), 1);
    }
}
