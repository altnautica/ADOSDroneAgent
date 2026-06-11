//! Profile + role resolution for the pairing-info wire contract.
//!
//! The agent stores `profile` internally as `"drone"` / `"ground_station"`
//! (underscore form), but the GCS-facing wire contract uses the hyphenated
//! `"ground-station"`. This module bridges the two exactly as the Python
//! `ados.core.profile.current_profile_and_role` does, so the native pairing-info
//! endpoint emits the same `profile` + `role` discriminators the FastAPI
//! endpoint (and the cloud heartbeat) do.
//!
//! Resolution order, matching the Python:
//!
//! 1. `agent.profile` from the loaded config when it is an explicit value
//!    (`"drone"` / `"ground_station"`).
//! 2. `/etc/ados/profile.conf` when the config field is `"auto"`, empty, or
//!    absent — install.sh writes this file and `ados profile set` flips it.
//! 3. `"drone"` as the final fallback.
//!
//! `role` is `"direct" | "relay" | "receiver"` for a ground station (read from
//! the `/etc/ados/mesh/role` sentinel, defaulting to `"direct"`), and `None` for
//! a drone.

use std::path::{Path, PathBuf};

/// The profile-source sentinel install.sh writes. Overridable via
/// `ADOS_PROFILE_CONF` for tests (no env in production; the path is fixed).
pub const PROFILE_CONF: &str = "/etc/ados/profile.conf";

/// The ground-station role sentinel the role manager writes. Overridable via
/// `ADOS_MESH_ROLE` for tests.
pub const MESH_ROLE_PATH: &str = "/etc/ados/mesh/role";

/// The valid ground-station roles, matching the Python `VALID_ROLES`.
const VALID_ROLES: [&str; 3] = ["direct", "relay", "receiver"];

/// Normalize a raw profile value to the wire-contract string. `"ground_station"`
/// becomes `"ground-station"`; `"drone"`, `"auto"`, `""`, and any unknown value
/// fall back to `"drone"` for wire purposes. Mirrors `normalize_profile`.
fn normalize_profile(raw: Option<&str>) -> &'static str {
    match raw {
        Some("ground_station") => "ground-station",
        _ => "drone",
    }
}

/// Read the canonical `profile:` value out of `profile.conf`, returning the
/// underscore form (`"drone"` / `"ground_station"`) or `None` on any error /
/// unrecognized value. Accepts both the YAML form (`profile: X`) and the legacy
/// key=value form (`profile=X`), mirroring `_read_profile_conf_value`.
fn read_profile_conf_value(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let raw = if let Some(rest) = stripped.strip_prefix("profile:") {
            rest
        } else if let Some(rest) = stripped.strip_prefix("profile=") {
            rest
        } else {
            continue;
        };
        let value = raw.trim().trim_matches(|c| c == '"' || c == '\'');
        if matches!(value, "drone" | "ground_station" | "ground-station") {
            return Some(value.replace('-', "_"));
        }
    }
    None
}

/// Read the on-disk ground-station role sentinel, defaulting to `"direct"` when
/// the file is missing, unreadable, or carries an unknown value. Mirrors
/// `role_manager.get_current_role`.
fn read_role(path: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(path) {
        let value = text.trim();
        if VALID_ROLES.contains(&value) {
            return value.to_string();
        }
    }
    "direct".to_string()
}

fn profile_conf_path() -> PathBuf {
    std::env::var("ADOS_PROFILE_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(PROFILE_CONF))
}

fn mesh_role_path() -> PathBuf {
    std::env::var("ADOS_MESH_ROLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(MESH_ROLE_PATH))
}

/// Resolve `(profile, role)` from the config's `agent.profile` plus the on-disk
/// sentinels, matching `current_profile_and_role`. `profile` is the hyphen-form
/// wire string; `role` is `Some("direct"|"relay"|"receiver")` for a ground
/// station and `None` for a drone.
pub fn current_profile_and_role(config_profile: &str) -> (String, Option<String>) {
    current_profile_and_role_at(config_profile, &profile_conf_path(), &mesh_role_path())
}

/// The path-injectable core, for tests. `config_profile` is the raw
/// `agent.profile` value from the loaded config.
pub fn current_profile_and_role_at(
    config_profile: &str,
    profile_conf: &Path,
    role_path: &Path,
) -> (String, Option<String>) {
    // An explicit config value wins; "auto"/empty falls back to profile.conf.
    let raw_owned;
    let raw: Option<&str> = if config_profile.is_empty() || config_profile == "auto" {
        match read_profile_conf_value(profile_conf) {
            Some(v) => {
                raw_owned = v;
                Some(raw_owned.as_str())
            }
            None => None,
        }
    } else {
        Some(config_profile)
    };

    let profile = normalize_profile(raw);
    if profile == "ground-station" {
        (profile.to_string(), Some(read_role(role_path)))
    } else {
        (profile.to_string(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn explicit_drone_config_is_drone_with_no_role() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, role) = current_profile_and_role_at(
            "drone",
            &dir.path().join("absent.conf"),
            &dir.path().join("absent.role"),
        );
        assert_eq!(profile, "drone");
        assert_eq!(role, None);
    }

    #[test]
    fn explicit_ground_station_config_reads_the_role() {
        let dir = tempfile::tempdir().unwrap();
        let role_path = write(dir.path(), "role", "relay\n");
        let (profile, role) = current_profile_and_role_at(
            "ground_station",
            &dir.path().join("absent.conf"),
            &role_path,
        );
        assert_eq!(profile, "ground-station");
        assert_eq!(role, Some("relay".to_string()));
    }

    #[test]
    fn ground_station_role_defaults_to_direct_when_the_sentinel_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, role) = current_profile_and_role_at(
            "ground_station",
            &dir.path().join("absent.conf"),
            &dir.path().join("absent.role"),
        );
        assert_eq!(profile, "ground-station");
        assert_eq!(role, Some("direct".to_string()));
    }

    #[test]
    fn auto_falls_back_to_profile_conf_yaml_form() {
        let dir = tempfile::tempdir().unwrap();
        let conf = write(
            dir.path(),
            "profile.conf",
            "# a comment\nprofile: ground_station\n",
        );
        let role_path = write(dir.path(), "role", "receiver\n");
        let (profile, role) = current_profile_and_role_at("auto", &conf, &role_path);
        assert_eq!(profile, "ground-station");
        assert_eq!(role, Some("receiver".to_string()));
    }

    #[test]
    fn auto_falls_back_to_profile_conf_legacy_kv_form() {
        let dir = tempfile::tempdir().unwrap();
        let conf = write(dir.path(), "profile.conf", "profile=drone\n");
        let (profile, role) = current_profile_and_role_at("auto", &conf, &dir.path().join("r"));
        assert_eq!(profile, "drone");
        assert_eq!(role, None);
    }

    #[test]
    fn empty_config_and_absent_conf_falls_back_to_drone() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, role) = current_profile_and_role_at(
            "",
            &dir.path().join("absent.conf"),
            &dir.path().join("absent.role"),
        );
        assert_eq!(profile, "drone");
        assert_eq!(role, None);
    }

    #[test]
    fn the_hyphen_form_in_profile_conf_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        // install.sh / the wizard may persist the hyphen form; it normalizes back
        // to the underscore internal form before the wire normalization.
        let conf = write(dir.path(), "profile.conf", "profile: ground-station\n");
        let role_path = write(dir.path(), "role", "direct\n");
        let (profile, role) = current_profile_and_role_at("auto", &conf, &role_path);
        assert_eq!(profile, "ground-station");
        assert_eq!(role, Some("direct".to_string()));
    }

    #[test]
    fn an_unknown_role_value_defaults_to_direct() {
        let dir = tempfile::tempdir().unwrap();
        let role_path = write(dir.path(), "role", "bogus\n");
        let (_p, role) =
            current_profile_and_role_at("ground_station", &dir.path().join("c"), &role_path);
        assert_eq!(role, Some("direct".to_string()));
    }
}
