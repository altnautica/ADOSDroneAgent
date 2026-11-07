//! The `control:` and `pilot:` config slices, and persisting live changes.

use std::path::{Path, PathBuf};

use ados_protocol::pilot::{AvoidMode, ControlMode, Profile};
use serde::{Deserialize, Serialize};

/// Default agent config path.
pub const CONFIG_PATH: &str = "/etc/ados/config.yaml";

/// The agent config path (`ADOS_CONFIG` overrides it, as for every service).
pub fn config_path() -> PathBuf {
    std::env::var_os("ADOS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(CONFIG_PATH))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlSection {
    pub mode: ControlMode,
    pub rc_ados_position: u8,
}

impl Default for ControlSection {
    fn default() -> Self {
        Self {
            mode: ControlMode::Ados,
            rc_ados_position: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeightRef {
    #[default]
    Agl,
    Ekf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkLossAction {
    #[default]
    Rth,
    Hold,
    Land,
}

impl LinkLossAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rth => "rth",
            Self::Hold => "hold",
            Self::Land => "land",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PilotSection {
    pub profile: Profile,
    pub avoid: AvoidMode,
    pub height_ref: HeightRef,
    pub obstacle_margin_m: f32,
    pub max_alt_agl_m: f32,
    pub max_dist_m: f32,
    pub min_agl_fw_m: f32,
    pub link_loss_s: u32,
    pub link_loss_action: LinkLossAction,
}

impl Default for PilotSection {
    fn default() -> Self {
        Self {
            profile: Profile::Normal,
            avoid: AvoidMode::Brake,
            height_ref: HeightRef::Agl,
            obstacle_margin_m: 1.5,
            max_alt_agl_m: 120.0,
            max_dist_m: 500.0,
            min_agl_fw_m: 30.0,
            link_loss_s: 10,
            link_loss_action: LinkLossAction::Rth,
        }
    }
}

/// Both slices.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub control: ControlSection,
    pub pilot: PilotSection,
}

impl Settings {
    /// Read both slices from the effective config (schema defaults overlaid
    /// with `config.yaml`). A malformed slice falls back to its default and is
    /// reported through the config-status sidecar.
    pub fn load(path: &Path) -> Self {
        let control = ados_config::schema::load_slice(path, "/control");
        let pilot = ados_config::schema::load_slice(path, "/pilot");
        let err = control.as_ref().err().or(pilot.as_ref().err()).cloned();
        ados_config::write_config_status("pilot", err.as_deref());
        Self {
            control: control.unwrap_or_default(),
            pilot: pilot.unwrap_or_default(),
        }
    }
}

/// Write `section.key = value` into the YAML config, preserving every other
/// key, through a temp file and rename.
pub fn persist(path: &Path, section: &str, key: &str, value: &str) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mut doc: serde_norway::Value = if text.trim().is_empty() {
        serde_norway::Value::Mapping(Default::default())
    } else {
        serde_norway::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?
    };
    let root = doc.as_mapping_mut().ok_or("config root is not a mapping")?;
    let entry = root
        .entry(serde_norway::Value::from(section))
        .or_insert_with(|| serde_norway::Value::Mapping(Default::default()));
    if !entry.is_mapping() {
        *entry = serde_norway::Value::Mapping(Default::default());
    }
    if let Some(m) = entry.as_mapping_mut() {
        // Integer-valued keys (the RC slot) are written as numbers.
        let v = value
            .parse::<i64>()
            .map(serde_norway::Value::from)
            .unwrap_or_else(|_| serde_norway::Value::from(value));
        m.insert(serde_norway::Value::from(key), v);
    }
    let out = serde_norway::to_string(&doc).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("yaml.pilot-tmp");
    std::fs::write(&tmp, out).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_match_the_schema_defaults() {
        ados_config::schema::assert_slice_conforms::<ControlSection>("/control");
        ados_config::schema::assert_slice_conforms::<PilotSection>("/pilot");
    }

    #[test]
    fn persist_keeps_unrelated_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(
            &p,
            "agent:\n  name: test\ncontrol:\n  rc_ados_position: 3\n",
        )
        .unwrap();
        persist(&p, "control", "mode", "fc_manual").unwrap();
        persist(&p, "pilot", "avoid", "off").unwrap();
        persist(&p, "control", "rc_ados_position", "4").unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.control.mode, ControlMode::FcManual);
        assert_eq!(s.control.rc_ados_position, 4);
        assert_eq!(s.pilot.avoid, AvoidMode::Off);
        assert!(std::fs::read_to_string(&p).unwrap().contains("name: test"));
    }
}
