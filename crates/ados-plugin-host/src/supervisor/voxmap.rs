//! The voxel map's single writer (seam M, the `shm.voxmap` grant).
//!
//! The map file lives in [`ados_protocol::voxmap::VOXMAP_DIR`], under the
//! root-only run dir. The host makes that directory the property of the one
//! installed plugin holding the grant, and root's while none or several do.
//! Its owner is the record readers check the file against: only that uid can
//! create the file, and a reader refuses a file anyone else owns.

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::*;

/// The map directory's name under the run dir.
const VOXMAP_DIR_NAME: &str = "voxmap";

/// The one plugin of `holders` (the installs holding the grant), or `None`
/// when there is not exactly one: two writers of one map would each be
/// trusted, so neither is.
fn sole_writer<'a>(holders: &[&'a str]) -> Option<&'a str> {
    match holders {
        [one] => Some(*one),
        _ => None,
    }
}

impl PluginSupervisor {
    /// Hand the voxel-map directory to its single writer, or back to root.
    ///
    /// Run whenever a grant, an enable or a removal can change who holds
    /// `shm.voxmap`, and at daemon start (the run dir is a tmpfs, recreated
    /// root's at boot). A no-op unless this host runs plugins sandboxed as
    /// their own users and is root: elsewhere plugins run as this process's
    /// user, which owns the directory already.
    pub(super) fn assign_voxmap_writer(&self) {
        if !self.backend.enforces_sandbox() || !nix::unistd::geteuid().is_root() {
            return;
        }
        let holders: Vec<&str> = self
            .installs
            .iter()
            .filter(|i| state::granted_caps(i).contains(crate::sandbox::VOXMAP_WRITE_CAP))
            .map(|i| i.plugin_id.as_str())
            .collect();
        if holders.len() > 1 {
            tracing::error!(
                ?holders,
                "voxmap_writer_ambiguous: more than one plugin holds shm.voxmap; none may publish"
            );
        }
        let writer = sole_writer(&holders);
        let owner = match writer.map(|id| (id, crate::plugin_account::plugin_uid(id))) {
            Some((_, Some(uid))) => uid,
            Some((id, None)) => {
                tracing::warn!(plugin_id = id, "voxmap_writer_has_no_account");
                0
            }
            None => 0,
        };
        let dir = self.paths.run_dir.join(VOXMAP_DIR_NAME);
        if let Err(e) = hand_voxmap_dir(&dir, owner) {
            tracing::error!(dir = %dir.display(), owner, error = %e, "voxmap_dir_not_handed_over");
        }
    }
}

/// Make `dir` (created when missing) `owner`'s, group
/// [`ados_protocol::voxmap::VOXMAP_GROUP`] when it exists, mode `0750`, then
/// remove every entry in it someone else owns: a file the previous writer
/// left is not the new writer's, and would only block it or be refused.
/// The ownership changes first, so a previous writer can add nothing after
/// the sweep.
fn hand_voxmap_dir(dir: &Path, owner: u32) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(md) if md.is_dir() => {}
        Ok(_) => {
            return Err(std::io::Error::other(format!(
                "{} is not a directory",
                dir.display()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(dir)?,
        Err(e) => return Err(e),
    }
    if nix::unistd::geteuid().as_raw() != owner || std::fs::symlink_metadata(dir)?.uid() != owner {
        let gid = nix::unistd::Group::from_name(ados_protocol::voxmap::VOXMAP_GROUP)
            .ok()
            .flatten()
            .map(|g| g.gid.as_raw());
        std::os::unix::fs::lchown(dir, Some(owner), gid)?;
    }
    // The run dir is root's alone, so the entry checked above is still the
    // directory this follows.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750))?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let md = entry.metadata()?;
        if md.uid() == owner {
            continue;
        }
        if md.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_has_a_writer_only_while_one_plugin_holds_the_grant() {
        assert_eq!(sole_writer(&[]), None);
        assert_eq!(sole_writer(&["com.example.nav"]), Some("com.example.nav"));
        assert_eq!(sole_writer(&["com.example.nav", "com.example.other"]), None);
    }

    #[test]
    fn handing_over_creates_the_directory_owner_only_and_refuses_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let me = nix::unistd::geteuid().as_raw();
        let dir = tmp.path().join("voxmap");
        hand_voxmap_dir(&dir, me).unwrap();
        let md = std::fs::symlink_metadata(&dir).unwrap();
        assert!(md.is_dir());
        assert_eq!((md.uid(), md.mode() & 0o7777), (me, 0o750));
        // The writer's own file survives a second hand-over to the same uid.
        std::fs::write(dir.join("map-v1"), b"x").unwrap();
        hand_voxmap_dir(&dir, me).unwrap();
        assert!(dir.join("map-v1").exists());

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(hand_voxmap_dir(&link, me).is_err());
    }
}
