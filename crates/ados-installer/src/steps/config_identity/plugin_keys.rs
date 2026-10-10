//! Plugin trust-key provisioning into `/etc/ados/plugin-keys/`.
//!
//! Three sources land keys there: the first-party keys that ship in the repo
//! (`<source>/scripts/plugin-keys/`), the out-of-band operator/OEM directory
//! [`TRUST_D_DIR`], and the explicit `--plugin-key <path>` flags.
//!
//! First-party status is the file stem being on `FIRST_PARTY_SIGNERS` and the
//! signature verifying against that file's key, so only the repo source may
//! write a first-party-named file. An operator or OEM key named like a
//! first-party signer is skipped with a warning; otherwise it would replace the
//! real first-party key.
//!
//! The names provisioned from the repo source are recorded in
//! [`PROVISIONED_MANIFEST`]. On the next run a name recorded there but no longer
//! in the source set is removed, so a key retired from the repo stops being
//! trusted on upgraded nodes. Keys from any other source are never recorded and
//! never pruned.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::set_mode;

/// Plugin trust keys dir under /etc/ados.
const PLUGIN_KEYS_DIR: &str = "/etc/ados/plugin-keys";

/// Out-of-band operator/OEM trust-provisioning directory: every `*.pem` here is
/// copied into `/etc/ados/plugin-keys/` on install, so an operator can trust an
/// OEM signer without committing a named key to a public repo (the key material
/// is cryptographically public, the NAME can be sensitive). The directory is
/// created (0700) if absent; a key dropped here lands on install.
const TRUST_D_DIR: &str = "/opt/ados/trust.d";

/// The manifest of repo-provisioned key file names, one per line, inside the
/// trust-key dir. The leading dot keeps it out of the `*.pem` key scan.
pub const PROVISIONED_MANIFEST: &str = ".provisioned";

/// True when `file_name`'s stem names a first-party signer.
pub fn is_first_party_name(file_name: &str) -> bool {
    let stem = file_name.strip_suffix(".pem").unwrap_or(file_name);
    ados_plugin_host::FIRST_PARTY_SIGNERS
        .iter()
        .any(|(id, _)| *id == stem)
}

/// Every `*.pem` file name in `dir`, sorted. Empty when `dir` is absent.
fn pem_names(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.ends_with(".pem"))
        .collect();
    names.sort();
    names
}

/// Copy one key into `dst_dir` (0600). `false` when the copy failed.
fn copy_key(src: &Path, dst_dir: &Path, name: &str) -> bool {
    let dst = dst_dir.join(name);
    match std::fs::copy(src, &dst) {
        Ok(_) => {
            set_mode(&dst, 0o600);
            true
        }
        Err(e) => {
            tracing::warn!(path = %src.display(), error = %e, "plugin-key copy failed");
            false
        }
    }
}

/// Copy every `*.pem` from an operator source into `dst_dir`, skipping any
/// file named like a first-party signer. Returns how many landed.
pub fn copy_operator_pems(src_dir: &Path, dst_dir: &Path) -> usize {
    let mut copied = 0usize;
    for name in pem_names(src_dir) {
        if is_first_party_name(&name) {
            tracing::warn!(
                key = %name,
                source = %src_dir.display(),
                "skipping an operator key named like a first-party signer"
            );
            continue;
        }
        if copy_key(&src_dir.join(&name), dst_dir, &name) {
            copied += 1;
        }
    }
    copied
}

/// The names recorded in the manifest under `dst_dir`.
fn read_manifest(dst_dir: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(dst_dir.join(PROVISIONED_MANIFEST))
        .map(|body| {
            body.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Sync the repo-shipped keys from `src_dir` into `dst_dir`: copy every one,
/// remove every previously provisioned name the source no longer carries, and
/// record the current set. Returns `(copied, pruned)`. A missing source dir
/// changes nothing, so an unreadable source never prunes a trusted key.
pub fn sync_repo_keys(src_dir: &Path, dst_dir: &Path) -> (usize, usize) {
    if !src_dir.is_dir() {
        return (0, 0);
    }
    let current = pem_names(src_dir);
    let mut copied = 0usize;
    for name in &current {
        if copy_key(&src_dir.join(name), dst_dir, name) {
            copied += 1;
        }
    }
    let current_set: BTreeSet<String> = current.iter().cloned().collect();
    let mut pruned = 0usize;
    for retired in read_manifest(dst_dir).difference(&current_set) {
        // A manifest entry is a bare file name; anything else is not ours.
        if retired.contains('/') || !retired.ends_with(".pem") {
            continue;
        }
        match std::fs::remove_file(dst_dir.join(retired)) {
            Ok(()) => {
                tracing::info!(key = %retired, "retired repo plugin key removed");
                pruned += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(key = %retired, error = %e, "retired key not removed"),
        }
    }
    let mut body = current.join("\n");
    body.push('\n');
    let manifest = dst_dir.join(PROVISIONED_MANIFEST);
    match std::fs::write(&manifest, body) {
        Ok(()) => set_mode(&manifest, 0o600),
        Err(e) => tracing::warn!(error = %e, "plugin-key manifest not written"),
    }
    (copied, pruned)
}

/// Provision the plugin trust keys at `/etc/ados/plugin-keys/` so the agent can
/// verify signed `.adosplug` archives: the repo's first-party keys (synced
/// against the manifest), then the operator/OEM keys from [`TRUST_D_DIR`].
pub fn provision_plugin_keys(source: Option<&Path>) {
    let dst = Path::new(PLUGIN_KEYS_DIR);
    let _ = std::fs::create_dir_all(dst);
    set_mode(dst, 0o700);

    let (copied, pruned) = match source.map(|s| s.join("scripts/plugin-keys")) {
        Some(src) if src.is_dir() => sync_repo_keys(&src, dst),
        _ => {
            tracing::warn!("plugin-keys source dir not found; skipping first-party trust keys");
            (0, 0)
        }
    };

    // Create trust.d so a pre-seeded directory survives and the repo path is
    // not the only one.
    let trust_d = Path::new(TRUST_D_DIR);
    let _ = std::fs::create_dir_all(trust_d);
    set_mode(trust_d, 0o700);
    let operator = copy_operator_pems(trust_d, dst);

    tracing::info!(
        count = copied + operator,
        pruned,
        "plugin trust keys provisioned"
    );
}

/// Copy the explicitly named plugin trust keys (`--plugin-key <path>`) into the
/// trust-key dir, returning how many landed.
pub fn provision_named_plugin_keys(paths: &[String]) -> usize {
    copy_named_keys(paths, Path::new(PLUGIN_KEYS_DIR))
}

/// [`provision_named_plugin_keys`] into an explicit directory.
fn copy_named_keys(paths: &[String], dst_dir: &Path) -> usize {
    let mut copied = 0usize;
    for p in paths {
        let src = PathBuf::from(p);
        let Some(name) = src.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            tracing::warn!(path = %p, "plugin-key has no filename");
            continue;
        };
        if is_first_party_name(&name) {
            tracing::warn!(path = %p, "skipping a --plugin-key named like a first-party signer");
            continue;
        }
        if copy_key(&src, dst_dir, &name) {
            copied += 1;
        }
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_party_file() -> String {
        format!("{}.pem", ados_plugin_host::FIRST_PARTY_SIGNERS[0].0)
    }

    #[test]
    fn first_party_named_trustd_key_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let trust_d = dir.path().join("trust.d");
        let keys = dir.path().join("plugin-keys");
        std::fs::create_dir_all(&trust_d).unwrap();
        std::fs::create_dir_all(&keys).unwrap();
        let fp = first_party_file();
        std::fs::write(keys.join(&fp), "real first-party key").unwrap();
        std::fs::write(trust_d.join(&fp), "impostor").unwrap();
        std::fs::write(trust_d.join("example-oem.pem"), "oem").unwrap();

        assert_eq!(copy_operator_pems(&trust_d, &keys), 1);
        assert_eq!(
            std::fs::read_to_string(keys.join(&fp)).unwrap(),
            "real first-party key"
        );
        assert_eq!(
            std::fs::read_to_string(keys.join("example-oem.pem")).unwrap(),
            "oem"
        );

        // The --plugin-key path applies the same rule.
        let named = vec![
            trust_d.join(&fp).to_string_lossy().into_owned(),
            trust_d
                .join("example-oem.pem")
                .to_string_lossy()
                .into_owned(),
        ];
        assert_eq!(copy_named_keys(&named, &keys), 1);
        assert_eq!(
            std::fs::read_to_string(keys.join(&fp)).unwrap(),
            "real first-party key"
        );
    }

    #[test]
    fn retired_repo_key_pruned_operator_key_kept() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source-keys");
        let keys = dir.path().join("plugin-keys");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(src.join("signer-a.pem"), "a").unwrap();
        std::fs::write(src.join("signer-b.pem"), "b").unwrap();
        std::fs::write(keys.join("example-oem.pem"), "operator").unwrap();

        assert_eq!(sync_repo_keys(&src, &keys), (2, 0));
        assert!(keys.join("signer-b.pem").exists());

        // signer-b is retired from the repo.
        std::fs::remove_file(src.join("signer-b.pem")).unwrap();
        assert_eq!(sync_repo_keys(&src, &keys), (1, 1));
        assert!(keys.join("signer-a.pem").exists());
        assert!(!keys.join("signer-b.pem").exists());
        assert_eq!(
            std::fs::read_to_string(keys.join("example-oem.pem")).unwrap(),
            "operator"
        );
        assert_eq!(
            std::fs::read_to_string(keys.join(PROVISIONED_MANIFEST)).unwrap(),
            "signer-a.pem\n"
        );

        // A missing source prunes nothing.
        assert_eq!(sync_repo_keys(&dir.path().join("absent"), &keys), (0, 0));
        assert!(keys.join("signer-a.pem").exists());
    }
}
