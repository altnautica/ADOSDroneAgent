//! Profile + role resolution for the pairing-info wire contract.
//!
//! The profile itself comes from the shared [`ados_config::resolve_profile`]
//! (explicit `agent.profile`, else `profile.conf`, else `drone`, in the
//! hyphenated wire form). This module pairs it with the ground-station role, the
//! same `profile` + `role` discriminators the cloud heartbeat emits.
//!
//! `role` is `"direct" | "relay" | "receiver"` for a ground station (read from
//! the `/etc/ados/mesh/role` sentinel through [`ados_config::ground_station_role`],
//! defaulting to `"direct"`), and `None` for every other profile.

use std::path::Path;

/// Resolve `(profile, role)` from the config's `agent.profile` plus the on-disk
/// sentinels, matching `current_profile_and_role`. `profile` is the hyphen-form
/// wire string; `role` is `Some("direct"|"relay"|"receiver")` for a ground
/// station and `None` for a drone.
pub fn current_profile_and_role(config_profile: &str) -> (String, Option<String>) {
    current_profile_and_role_at(
        config_profile,
        &ados_config::profile_conf_path(),
        &ados_config::mesh_role_path(),
    )
}

/// The path-injectable core, for tests. `config_profile` is the raw
/// `agent.profile` value from the loaded config.
pub fn current_profile_and_role_at(
    config_profile: &str,
    profile_conf: &Path,
    role_path: &Path,
) -> (String, Option<String>) {
    let profile = ados_config::resolve_profile(Some(config_profile), profile_conf);
    let role = ados_config::ground_station_role(&profile, role_path);
    (profile, role)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

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
    fn workstation_config_is_workstation_with_no_role() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, role) = current_profile_and_role_at(
            "workstation",
            &dir.path().join("absent.conf"),
            &dir.path().join("absent.role"),
        );
        assert_eq!(profile, "workstation");
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
