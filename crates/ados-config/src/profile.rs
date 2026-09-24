//! Node profile resolution: the one place a service learns what kind of node it
//! runs on.
//!
//! The profile is stored in two places. `agent.profile` in `config.yaml` is the
//! operator's explicit choice; `profile.conf` is the sentinel the installer
//! writes and `ados profile set` flips. An explicit config value wins; `auto`,
//! empty, or absent defers to `profile.conf`; anything unresolved is a drone.
//!
//! The result is the hyphenated wire form: `drone`, `ground-station`,
//! `workstation`, or `compute`.
//!
//! A ground station also carries a role (`direct`, `relay` or `receiver`) in the
//! `/etc/ados/mesh/role` sentinel the role manager writes; [`ground_station_role`]
//! reads it for a ground-station profile and answers `None` for every other.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::log_store::CONFIG_YAML;

/// The profile sentinel the installer writes. Overridable at runtime through
/// `ADOS_PROFILE_CONF` (see [`profile_conf_path`]).
pub const PROFILE_CONF: &str = "/etc/ados/profile.conf";

/// The profile sentinel path, honouring the `ADOS_PROFILE_CONF` override a
/// rootless per-user install (and tests) set. Unset: [`PROFILE_CONF`].
pub fn profile_conf_path() -> PathBuf {
    std::env::var_os("ADOS_PROFILE_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(PROFILE_CONF))
}

/// The ground-station role sentinel the role manager writes. Overridable at
/// runtime through `ADOS_MESH_ROLE` (see [`mesh_role_path`]).
pub const MESH_ROLE_PATH: &str = "/etc/ados/mesh/role";

/// The valid ground-station roles, matching the Python `VALID_ROLES`.
const VALID_ROLES: [&str; 3] = ["direct", "relay", "receiver"];

/// The role sentinel path, honouring the `ADOS_MESH_ROLE` override tests set.
/// Unset: [`MESH_ROLE_PATH`].
pub fn mesh_role_path() -> PathBuf {
    std::env::var_os("ADOS_MESH_ROLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(MESH_ROLE_PATH))
}

/// Read the ground-station role sentinel, defaulting to `"direct"` when the file
/// is missing, unreadable, or carries an unknown value: a ground station with no
/// sentinel runs the direct plane. Mirrors `role_manager.get_current_role`.
pub fn read_mesh_role(path: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(path) {
        let value = text.trim();
        if VALID_ROLES.contains(&value) {
            return value.to_string();
        }
    }
    "direct".to_string()
}

/// The ground-station role for a node of wire-form `profile`: the sentinel at
/// `role_path` on a `ground-station`, `None` on every other profile (a role is
/// meaningless off a ground station, so none is reported).
pub fn ground_station_role(profile: &str, role_path: &Path) -> Option<String> {
    (profile == "ground-station").then(|| read_mesh_role(role_path))
}

/// Wire-contract profile string from a raw value. `ground_station` and
/// `ground-station` become `ground-station`; `workstation` (the operator's
/// console) and `compute` (a lean engine-only worker) stay as-is;
/// `drone`/`auto`/empty/unknown collapse to `drone`.
pub fn normalize_profile(raw: Option<&str>) -> String {
    match raw {
        Some("ground_station") | Some("ground-station") => "ground-station".to_string(),
        Some("workstation") => "workstation".to_string(),
        Some("compute") => "compute".to_string(),
        _ => "drone".to_string(),
    }
}

/// Profile resolution order: explicit `config.agent.profile`, else the
/// `profile:` value in `profile.conf`, else `drone`. Returns the wire form.
pub fn resolve_profile(config_profile: Option<&str>, profile_conf: &Path) -> String {
    let raw = match config_profile {
        None | Some("") | Some("auto") => read_profile_conf_value(profile_conf),
        Some(v) => Some(v.to_string()),
    };
    normalize_profile(raw.as_deref())
}

/// Read the canonical `profile:` value out of `profile.conf`. Accepts the YAML
/// form (`profile: X`) and the legacy `key=value` form (`profile=X`). Returns
/// the underscore form, or `None` on any error / unrecognized value.
pub fn read_profile_conf_value(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let value = if let Some(rest) = stripped.strip_prefix("profile:") {
            Some(rest)
        } else {
            stripped.strip_prefix("profile=")
        };
        if let Some(value) = value {
            let v = value.trim().trim_matches(|c| c == '"' || c == '\'');
            if matches!(
                v,
                "drone" | "ground_station" | "ground-station" | "workstation" | "compute"
            ) {
                return Some(v.replace('-', "_"));
            }
        }
    }
    None
}

/// This node's profile in wire form, resolved from the same sources the
/// supervisor reads at startup: `agent.profile` in the config at `ADOS_CONFIG`
/// (default [`CONFIG_YAML`]) and the sentinel at `ADOS_PROFILE_CONF` (default
/// [`PROFILE_CONF`]). A missing or malformed config falls through to
/// `profile.conf`; nothing here panics.
pub fn node_profile() -> String {
    let config_yaml = std::env::var_os("ADOS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(CONFIG_YAML));
    node_profile_at(&config_yaml, &profile_conf_path())
}

/// The path-injectable core of [`node_profile`].
pub fn node_profile_at(config_yaml: &Path, profile_conf: &Path) -> String {
    #[derive(Debug, Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        agent: AgentSection,
    }
    #[derive(Debug, Default, Deserialize)]
    struct AgentSection {
        #[serde(default)]
        profile: Option<String>,
    }
    let raw: Raw = crate::load_yaml_or_default(config_yaml, "profile");
    resolve_profile(raw.agent.profile.as_deref(), profile_conf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn legacy_keyvalue_profile_conf_parses() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("profile.conf");
        write(&pc, "profile=drone\n");
        assert_eq!(read_profile_conf_value(&pc).as_deref(), Some("drone"));
    }

    #[test]
    fn profile_conf_hyphen_form_reads_back_as_underscore() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("profile.conf");
        write(
            &pc,
            "# header\nprofile: \"ground-station\"\nmesh_capable: true\n",
        );
        assert_eq!(
            read_profile_conf_value(&pc).as_deref(),
            Some("ground_station")
        );
    }

    #[test]
    fn unrecognized_or_missing_profile_conf_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("profile.conf");
        assert_eq!(read_profile_conf_value(&pc), None);
        write(&pc, "profile: toaster\n");
        assert_eq!(read_profile_conf_value(&pc), None);
    }

    #[test]
    fn workstation_and_compute_profiles_resolve_and_never_collapse_to_drone() {
        // Both the operator's workstation and a lean engine-only compute worker
        // must survive profile resolution, from the config and from profile.conf.
        let dir = tempfile::tempdir().unwrap();
        let no_conf = dir.path().join("nope.conf");
        assert_eq!(
            resolve_profile(Some("workstation"), &no_conf),
            "workstation"
        );
        assert_eq!(resolve_profile(Some("compute"), &no_conf), "compute");
        let pc = dir.path().join("profile.conf");
        write(&pc, "profile: compute\n");
        assert_eq!(read_profile_conf_value(&pc).as_deref(), Some("compute"));
        assert_eq!(resolve_profile(Some("auto"), &pc), "compute");
    }

    #[test]
    fn explicit_config_profile_wins_over_profile_conf() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("profile.conf");
        write(&pc, "profile: ground_station\n");
        assert_eq!(resolve_profile(Some("drone"), &pc), "drone");
        assert_eq!(
            resolve_profile(Some("ground-station"), &pc),
            "ground-station"
        );
    }

    #[test]
    fn auto_empty_and_absent_defer_to_profile_conf_then_drone() {
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("profile.conf");
        for raw in [None, Some(""), Some("auto")] {
            assert_eq!(resolve_profile(raw, &pc), "drone");
        }
        write(&pc, "profile: ground-station\n");
        for raw in [None, Some(""), Some("auto")] {
            assert_eq!(resolve_profile(raw, &pc), "ground-station");
        }
        // An unknown explicit value does not consult profile.conf.
        assert_eq!(resolve_profile(Some("nonsense"), &pc), "drone");
    }

    #[test]
    fn node_profile_reads_config_then_profile_conf() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let pc = dir.path().join("profile.conf");
        // Nothing on disk: a drone.
        assert_eq!(node_profile_at(&cfg, &pc), "drone");
        write(&pc, "profile: workstation\n");
        // No config: profile.conf decides.
        assert_eq!(node_profile_at(&cfg, &pc), "workstation");
        // `auto` in the config defers to profile.conf.
        write(
            &cfg,
            "agent:\n  profile: auto\n  name: n1\nvideo:\n  mode: auto\n",
        );
        assert_eq!(node_profile_at(&cfg, &pc), "workstation");
        // An explicit config value wins.
        write(&cfg, "agent:\n  profile: ground_station\n");
        assert_eq!(node_profile_at(&cfg, &pc), "ground-station");
        // A malformed config does not panic; it falls through to profile.conf.
        write(&cfg, "agent: [unterminated\n");
        assert_eq!(node_profile_at(&cfg, &pc), "workstation");
    }

    #[test]
    fn ground_station_role_reads_the_sentinel_only_on_a_ground_station() {
        let dir = tempfile::tempdir().unwrap();
        let role = dir.path().join("role");
        // A drone has no role, whatever the sentinel says.
        write(&role, "relay\n");
        assert_eq!(ground_station_role("drone", &role), None);
        assert_eq!(
            ground_station_role("ground-station", &role).as_deref(),
            Some("relay")
        );
        // Absent or unknown: the direct plane.
        write(&role, "bogus\n");
        assert_eq!(
            ground_station_role("ground-station", &role).as_deref(),
            Some("direct")
        );
        let absent = dir.path().join("absent");
        assert_eq!(
            ground_station_role("ground-station", &absent).as_deref(),
            Some("direct")
        );
    }
}
