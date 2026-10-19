//! Write access for an explicit capture root.
//!
//! The supervisor runs under `ProtectSystem=strict`; its unit grants the
//! agent's own directories plus `-/mnt -/media`, which covers the `auto` NVMe
//! choice. An explicit `storage.capture_root` elsewhere (`/data/capture`) is
//! granted through a drop-in derived from the config on every install, and
//! the directory is created here, outside the sandbox, because the supervisor
//! cannot create a path its namespace mounts read-only. It is created (or
//! corrected) as `2775 root:ados-capture`, the access the supervisor's janitor
//! also keeps on every root it resolves, so a sandboxed capture plugin in the
//! capture group can create its session directories.

use super::*;

/// The supervisor's drop-in directory and the capture-root drop-in in it.
pub(super) const SUPERVISOR_DROPIN_DIR: &str = "/etc/systemd/system/ados-supervisor.service.d";
pub(super) const CAPTURE_ROOT_DROPIN_NAME: &str = "capture-root.conf";

/// Roots the supervisor unit already makes writable.
const COVERED: &[&str] = &["/mnt", "/media", "/var/ados", "/var/lib/ados"];

/// What the config asks of the supervisor's sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRootGrant {
    /// `auto`, unset, or a root the unit already covers: no drop-in.
    Covered,
    /// An explicit root the unit does not cover.
    Grant(String),
    /// A value that cannot be granted (not absolute, `/` itself, `..`, or
    /// characters a unit file would read differently). Flagged, not granted.
    Invalid(String),
}

/// Parse `storage.capture_root` from config.yaml text. A malformed or absent
/// config is `auto`. Pure.
pub fn capture_root_grant(config_body: Option<&str>) -> CaptureRootGrant {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        storage: Option<ados_config::capture_storage::CaptureStorageConfig>,
    }
    let raw = config_body
        .and_then(|t| serde_norway::from_str::<Raw>(t).ok())
        .and_then(|r| r.storage)
        .map(|s| s.capture_root)
        .unwrap_or_default();
    let root = raw.trim();
    if root.is_empty() || root == "auto" {
        return CaptureRootGrant::Covered;
    }
    let path = Path::new(root);
    let unit_safe = !root
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || "%\\\"';".contains(c));
    let normal = path
        .components()
        .all(|c| !matches!(c, std::path::Component::ParentDir));
    if !path.is_absolute() || root == "/" || !unit_safe || !normal {
        return CaptureRootGrant::Invalid(root.to_string());
    }
    if COVERED.iter().any(|c| path.starts_with(c)) {
        return CaptureRootGrant::Covered;
    }
    CaptureRootGrant::Grant(root.trim_end_matches('/').to_string())
}

/// The drop-in body for an explicit root. `-` keeps namespace setup from
/// failing should the directory be removed later.
pub fn capture_root_dropin_body(root: &str) -> String {
    format!("[Service]\nReadWritePaths=-{root}\n")
}

/// Bring the drop-in under `dir` in line with `grant`: written for
/// [`CaptureRootGrant::Grant`], removed otherwise. Returns whether the file
/// changed (so the caller knows to reload systemd).
pub fn apply_capture_root_dropin(dir: &Path, grant: &CaptureRootGrant) -> bool {
    let path = dir.join(CAPTURE_ROOT_DROPIN_NAME);
    match grant {
        CaptureRootGrant::Grant(root) => {
            let body = capture_root_dropin_body(root);
            if std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
                return false;
            }
            let written =
                std::fs::create_dir_all(dir).and_then(|()| write_atomic(&path, body.as_bytes()));
            if let Err(e) = written {
                tracing::warn!(path = %path.display(), error = %e, "capture-root drop-in write failed");
                return false;
            }
            true
        }
        _ => std::fs::remove_file(&path).is_ok(),
    }
}

/// Reconcile the capture-root drop-in against the config on a capture
/// profile (drone, visor); every other profile carries none. Creates an
/// explicit root as `2775 root:ados-capture` (an existing root's owner and
/// mode are corrected, the sessions under it left alone), flags an invalid
/// one, and reloads systemd when the drop-in changed. Idempotent. Runs after
/// the identity step has created the capture group.
pub(super) fn reconcile_capture_root_dropin(profile: &str) {
    let grant = if matches!(profile, "drone" | "visor") {
        let body = std::fs::read_to_string(crate::env::CONFIG_YAML).ok();
        capture_root_grant(body.as_deref())
    } else {
        CaptureRootGrant::Covered
    };
    match &grant {
        CaptureRootGrant::Grant(root) => {
            let prepared =
                ados_config::capture_storage::prepare_capture_root_on_host(Path::new(root));
            if let Err(e) = prepared {
                tracing::warn!(root, error = %e, "could not prepare the capture root");
            }
        }
        CaptureRootGrant::Invalid(root) => tracing::warn!(
            root,
            "storage.capture_root must be auto or an absolute path; the supervisor is not \
             granted write access to it and capture sessions will be refused"
        ),
        CaptureRootGrant::Covered => {}
    }
    if apply_capture_root_dropin(Path::new(SUPERVISOR_DROPIN_DIR), &grant) {
        let _ = exec::run("systemctl", &["daemon-reload"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(yaml: &str) -> CaptureRootGrant {
        capture_root_grant(Some(yaml))
    }

    #[test]
    fn auto_and_covered_roots_need_no_dropin() {
        assert_eq!(capture_root_grant(None), CaptureRootGrant::Covered);
        assert_eq!(grant("storage: {}\n"), CaptureRootGrant::Covered);
        assert_eq!(
            grant("storage:\n  capture_root: auto\n"),
            CaptureRootGrant::Covered
        );
        assert_eq!(grant("not: [valid\n"), CaptureRootGrant::Covered);
        for root in [
            "/mnt/nvme/cap",
            "/media/usb/cap",
            "/var/ados/capture",
            "/var/lib/ados/c",
        ] {
            assert_eq!(
                grant(&format!("storage:\n  capture_root: {root}\n")),
                CaptureRootGrant::Covered,
                "{root}"
            );
        }
    }

    #[test]
    fn an_explicit_root_elsewhere_is_granted() {
        assert_eq!(
            grant("storage:\n  capture_root: /data/capture/\n"),
            CaptureRootGrant::Grant("/data/capture".into())
        );
        assert_eq!(
            capture_root_dropin_body("/data/capture"),
            "[Service]\nReadWritePaths=-/data/capture\n"
        );
        // `/mntx` is not under `/mnt`.
        assert_eq!(
            grant("storage:\n  capture_root: /mntx/c\n"),
            CaptureRootGrant::Grant("/mntx/c".into())
        );
    }

    #[test]
    fn a_root_that_cannot_be_granted_is_flagged() {
        for root in [
            "data/capture",
            "/",
            "/data/../etc",
            "\"/data/my cap\"",
            "/data/100%",
        ] {
            assert!(
                matches!(
                    grant(&format!("storage:\n  capture_root: {root}\n")),
                    CaptureRootGrant::Invalid(_)
                ),
                "{root}"
            );
        }
    }

    #[test]
    fn the_dropin_follows_the_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ados-supervisor.service.d");
        let path = dir.join(CAPTURE_ROOT_DROPIN_NAME);
        let g = CaptureRootGrant::Grant("/data/capture".into());
        assert!(apply_capture_root_dropin(&dir, &g));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            capture_root_dropin_body("/data/capture")
        );
        assert!(!apply_capture_root_dropin(&dir, &g), "unchanged: no reload");
        let invalid = CaptureRootGrant::Invalid("rel".into());
        assert!(apply_capture_root_dropin(&dir, &invalid));
        assert!(!path.exists());
        assert!(!apply_capture_root_dropin(&dir, &CaptureRootGrant::Covered));
    }
}
