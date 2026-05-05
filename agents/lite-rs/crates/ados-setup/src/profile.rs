//! Apply a ProfileChoiceRequest to agent.yaml.
//!
//! Mirrors `ados.setup.profile.apply_profile()` in the Python reference.
//! On lite the profile choice is much narrower than on the full agent —
//! lite doesn't ship ground-station mode, so only `profile=drone` is a
//! supported value at v0.1. We accept and persist `ground_station` for
//! protocol compatibility with the wizard but log a warning that the
//! lite agent has no ground-station support.

use std::path::Path;

use serde_yaml::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("invalid profile: {0}")]
    InvalidProfile(String),

    #[error("ground_role required for ground_station profile")]
    GroundRoleRequired,

    #[error("invalid ground_role: {0}")]
    InvalidGroundRole(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

const VALID_PROFILES: &[&str] = &["drone", "ground_station"];
const VALID_GROUND_ROLES: &[&str] = &["direct", "relay", "receiver"];

pub fn apply_profile(
    agent_yaml: &Path,
    profile: &str,
    ground_role: Option<&str>,
) -> Result<(), ProfileError> {
    if !VALID_PROFILES.contains(&profile) {
        return Err(ProfileError::InvalidProfile(profile.to_string()));
    }
    if profile == "ground_station" {
        let role = ground_role.ok_or(ProfileError::GroundRoleRequired)?;
        if !VALID_GROUND_ROLES.contains(&role) {
            return Err(ProfileError::InvalidGroundRole(role.to_string()));
        }
    }
    if profile == "ground_station" {
        tracing::warn!(
            "lite agent does not ship ground-station services; \
             persisting choice for protocol compat but no ground-station \
             features will start"
        );
    }

    let mut doc = load_or_default(agent_yaml)?;
    let map = ensure_mapping(&mut doc);
    let agent = ensure_section(map, "agent");
    set_string(agent, "profile", profile);
    if let Some(role) = ground_role {
        let gs = ensure_section(map, "ground_station");
        set_string(gs, "role", role);
    }
    write_atomic(agent_yaml, &doc)?;
    Ok(())
}

fn load_or_default(path: &Path) -> Result<Value, ProfileError> {
    if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            return Ok(Value::Mapping(Default::default()));
        }
        Ok(serde_yaml::from_str(&raw)?)
    } else {
        Ok(Value::Mapping(Default::default()))
    }
}

fn ensure_mapping(doc: &mut Value) -> &mut serde_yaml::Mapping {
    if !doc.is_mapping() {
        *doc = Value::Mapping(Default::default());
    }
    doc.as_mapping_mut().expect("doc is mapping")
}

fn ensure_section<'a>(
    map: &'a mut serde_yaml::Mapping,
    key: &str,
) -> &'a mut serde_yaml::Mapping {
    let key_val = Value::String(key.into());
    let entry = map
        .entry(key_val)
        .or_insert_with(|| Value::Mapping(Default::default()));
    if !entry.is_mapping() {
        *entry = Value::Mapping(Default::default());
    }
    entry.as_mapping_mut().expect("section is mapping")
}

fn set_string(map: &mut serde_yaml::Mapping, key: &str, value: &str) {
    map.insert(Value::String(key.into()), Value::String(value.into()));
}

fn write_atomic(path: &Path, doc: &Value) -> Result<(), ProfileError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".agent.yaml.{}.tmp", std::process::id()));
    let serialized = serde_yaml::to_string(doc)?;
    std::fs::write(&tmp, serialized)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640)).ok();
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drone_profile_writes_into_existing_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(&path, "agent:\n  device_id: \"test\"\n").unwrap();
        apply_profile(&path, "drone", None).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: Value = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            doc.get("agent")
                .and_then(|a| a.get("profile"))
                .and_then(|p| p.as_str()),
            Some("drone")
        );
        // device_id preserved
        assert_eq!(
            doc.get("agent")
                .and_then(|a| a.get("device_id"))
                .and_then(|p| p.as_str()),
            Some("test")
        );
    }

    #[test]
    fn ground_station_requires_role() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(&path, "agent:\n  device_id: \"test\"\n").unwrap();
        let err = apply_profile(&path, "ground_station", None).unwrap_err();
        assert!(matches!(err, ProfileError::GroundRoleRequired));
    }

    #[test]
    fn ground_station_role_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(&path, "agent:\n  device_id: \"test\"\n").unwrap();
        apply_profile(&path, "ground_station", Some("relay")).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: Value = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            doc.get("ground_station")
                .and_then(|a| a.get("role"))
                .and_then(|p| p.as_str()),
            Some("relay")
        );
    }

    #[test]
    fn invalid_profile_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        let err = apply_profile(&path, "starship", None).unwrap_err();
        assert!(matches!(err, ProfileError::InvalidProfile(_)));
    }
}
