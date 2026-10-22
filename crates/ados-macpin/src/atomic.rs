//! Atomic file replacement for the state file and the `.link` drop-ins.

use std::path::Path;

/// The sibling temp path `<name>.tmp-<8 random hex>` for `path`, after
/// creating the parent directory. Returns `(parent, tmp)`.
fn temp_sibling(path: &Path) -> std::io::Result<(&Path, std::path::PathBuf)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut suffix = [0u8; 4];
    getrandom::fill(&mut suffix).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp-{}", hex::encode(suffix)));
    Ok((parent, parent.join(tmp_name)))
}

/// Atomically replace `path` with `data` at file mode `mode`: a sibling temp
/// file opened `create_new` (so concurrent writers never share one), the exact
/// mode set regardless of umask, fsynced, renamed over `path`, then the
/// directory fsynced so the rename survives a power cut. On any error the temp
/// file is removed.
#[cfg(unix)]
pub(crate) fn write_atomic(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let (parent, tmp) = temp_sibling(path)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    let result = (|| {
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    drop(f);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Non-unix fallback: temp file plus rename, without modes or directory fsync.
#[cfg(not(unix))]
pub(crate) fn write_atomic(path: &Path, data: &[u8], _mode: u32) -> std::io::Result<()> {
    let (_, tmp) = temp_sibling(path)?;
    let result = std::fs::write(&tmp, data).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
