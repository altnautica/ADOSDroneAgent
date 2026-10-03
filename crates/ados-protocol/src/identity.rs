//! This node's device id: one resolver every service reads it through.
//!
//! The installer provisions the id once into `/etc/ados/device-id` (12 hex,
//! never rewritten), and that file is the authority. Every reader resolving
//! the id on its own once let one node answer to two ids: a short form in
//! `config.yaml` and the full form in the file. A request addressed by one was
//! then refused by a service that knew only the other.
//!
//! Precedence, identical in the Python agent (`ados.core.identity`):
//!
//! 1. the file — `$ADOS_DEVICE_ID_PATH` when set, else [`DEVICE_ID_FILE`];
//! 2. `$ADOS_DEVICE_ID` (the installer exports it equal to the file);
//! 3. the configured `agent.device_id` (a development host with no file);
//! 4. empty — an unprovisioned node.
//!
//! The id is used whole, never truncated.

use std::path::{Path, PathBuf};

/// The provisioned identity file.
pub const DEVICE_ID_FILE: &str = "/etc/ados/device-id";

/// The environment variable that relocates [`DEVICE_ID_FILE`].
pub const ENV_DEVICE_ID_PATH: &str = "ADOS_DEVICE_ID_PATH";

/// The environment variable carrying the id itself.
pub const ENV_DEVICE_ID: &str = "ADOS_DEVICE_ID";

/// The identity file this process reads: `$ADOS_DEVICE_ID_PATH`, else
/// [`DEVICE_ID_FILE`].
pub fn device_id_path() -> PathBuf {
    std::env::var(ENV_DEVICE_ID_PATH)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEVICE_ID_FILE))
}

/// This node's device id, resolved against the live file and environment.
/// `configured` is the `agent.device_id` a caller loaded from config, if any.
pub fn device_id(configured: Option<&str>) -> String {
    device_id_at(&device_id_path(), configured)
}

/// [`device_id`] with the identity file at an explicit path, for a service
/// that carries its own (test-injectable) path to it.
pub fn device_id_at(file: &Path, configured: Option<&str>) -> String {
    resolve(
        file,
        std::env::var(ENV_DEVICE_ID).ok().as_deref(),
        configured,
    )
}

/// The precedence itself, over explicit inputs. Pure apart from the file read.
pub fn resolve(file: &Path, env: Option<&str>, configured: Option<&str>) -> String {
    let from_file = std::fs::read_to_string(file).ok();
    let id = [from_file.as_deref(), env, configured]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|id| !id.is_empty())
        .unwrap_or_default()
        .to_string();
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provisioned_file_wins_over_env_and_config() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("device-id");
        std::fs::write(&file, "4e7a083410f5\n").unwrap();
        assert_eq!(
            resolve(&file, Some("aaaaaaaaaaaa"), Some("4e7a0834")),
            "4e7a083410f5",
            "a short configured id must not shadow the provisioned one"
        );
    }

    #[test]
    fn env_then_config_then_empty_when_the_file_is_absent_or_blank() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");
        let blank = dir.path().join("blank");
        std::fs::write(&blank, "  \n").unwrap();
        for file in [&missing, &blank] {
            assert_eq!(
                resolve(file, Some(" 0011aabbccdd "), Some("cfg")),
                "0011aabbccdd"
            );
            assert_eq!(
                resolve(file, Some(""), Some("630e079b69d6")),
                "630e079b69d6"
            );
            assert_eq!(resolve(file, None, None), "");
            assert_eq!(resolve(file, Some(" "), Some(" ")), "");
        }
    }
}
