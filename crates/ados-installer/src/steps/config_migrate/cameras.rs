//! The install-time move of the retired `video.cameras` and `vision.cameras`
//! lists into the top-level `cameras[]` list.
//!
//! The rewrite itself is [`ados_config::cameras::migrate_legacy_camera_lists`];
//! this applies it to the file under the shared config lock. It runs here, once
//! per install or upgrade, and never from a service's startup path: the
//! services read only the top-level list.

use std::path::Path;

use super::arbitration::acquire_lock;

/// Migrate the camera lists of the config at `config`, under the lock at
/// `lock_path`. `Ok(true)` when the file changed; an absent or unparseable
/// config is left alone.
pub fn apply(config: &Path, lock_path: &Path) -> Result<bool, String> {
    if !config.is_file() {
        return Ok(false);
    }
    let _lock = acquire_lock(lock_path)?;
    let body = std::fs::read_to_string(config).map_err(|e| e.to_string())?;
    let Some(updated) = migrate_body(&body)? else {
        return Ok(false);
    };
    crate::env::write_atomic_durable(config, updated.as_bytes(), Some(0o600))
        .map_err(|e| e.to_string())?;
    tracing::info!("moved the camera lists to the top-level cameras list");
    Ok(true)
}

/// The migrated config body, or `None` when there is nothing to move.
fn migrate_body(body: &str) -> Result<Option<String>, String> {
    let mut doc: serde_norway::Value = serde_norway::from_str(body).map_err(|e| e.to_string())?;
    if !ados_config::cameras::migrate_legacy_camera_lists(&mut doc) {
        return Ok(None);
    }
    serde_norway::to_string(&doc)
        .map(Some)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: &str = "agent:\n  profile: drone\nvideo:\n  cameras:\n    - id: main\n      source: /dev/video0\nvision:\n  cameras:\n    - id: main\n      kind: capture\n";

    #[test]
    fn the_file_is_rewritten_once_and_then_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let lock = dir.path().join("config.yaml.lock");
        std::fs::write(&config, LEGACY).unwrap();
        assert_eq!(apply(&config, &lock), Ok(true));
        let doc: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        let want: serde_norway::Value = serde_norway::from_str(
            "agent:\n  profile: drone\nvideo: {}\nvision: {}\ncameras:\n  - id: main\n    source: /dev/video0\n    capture: true\n",
        )
        .unwrap();
        assert_eq!(doc, want);
        let before = std::fs::read_to_string(&config).unwrap();
        assert_eq!(apply(&config, &lock), Ok(false));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    }

    #[test]
    fn an_absent_config_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("missing.yaml");
        assert_eq!(apply(&config, &dir.path().join("lock")), Ok(false));
    }

    #[test]
    fn an_unparseable_config_is_reported_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, "video: [unclosed\n").unwrap();
        assert!(apply(&config, &dir.path().join("lock")).is_err());
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "video: [unclosed\n"
        );
    }
}
