//! Append-only and plugin log trimming.

use super::*;

/// How much a trim of the plugin logs would free — the part of each log past its
/// cap, never the tail that is its floor.
pub fn plugin_log_trimmable(dir: &Path, max_bytes: u64, keep_bytes: u64) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".log")))
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .filter_map(|len| crate::janitor::plan::trim_from(len, max_bytes, keep_bytes))
        .fold(0u64, |a, b| a.saturating_add(b))
}

/// Trim an append-only file so only its last `keep_bytes` survive, in place.
///
/// In place, not by rename, because these files are written through a file
/// descriptor that is already open — systemd's `StandardOutput=append:` opens
/// the plugin log once when the unit starts, and the Python audit writer holds
/// its own handle. Renaming the file would leave every subsequent write going to
/// the renamed inode, so the "rotated" log would keep growing under its new name
/// and the new one would stay empty forever. Rewriting the same inode keeps the
/// writer's descriptor pointing at the file it is meant to be writing.
///
/// Both writers open with `O_APPEND`, which recomputes the offset from the file
/// length on every write, so shortening the file underneath them produces a
/// shorter file rather than a sparse one. A write landing in the moment between
/// the read and the truncate is lost; these are low-rate log writers and one
/// line is the worst case, which is the price of bounding a file nothing else
/// bounds.
///
/// Returns the bytes freed, or zero when the file is under its cap — which is
/// what makes a second pass immediately after a first a no-op.
pub fn trim_append_only(path: &Path, max_bytes: u64, keep_bytes: u64) -> u64 {
    if is_protected(path) {
        tracing::warn!(path = %path.display(), "janitor refused a protected path");
        return 0;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if !meta.is_file() {
        return 0;
    }
    let len = meta.len();
    let Some(drop_bytes) = crate::janitor::plan::trim_from(len, max_bytes, keep_bytes) else {
        return 0;
    };

    let tail = match read_tail(path, len.saturating_sub(drop_bytes)) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "janitor could not read the tail");
            return 0;
        }
    };
    // Start at the first line boundary so the surviving file does not open with
    // half a record — the audit trail is newline-delimited JSON and a torn first
    // line would make the whole file unparseable to a reader that is strict.
    let tail = match tail.iter().position(|b| *b == b'\n') {
        Some(i) if i + 1 < tail.len() => &tail[i + 1..],
        Some(_) => &[][..],
        None => &tail[..],
    };
    if let Err(e) = std::fs::write(path, tail) {
        tracing::debug!(path = %path.display(), error = %e, "janitor could not rewrite the file");
        return 0;
    }
    len.saturating_sub(tail.len() as u64)
}

/// Read the last `n` bytes of a file.
pub(super) fn read_tail(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(n)))?;
    let mut buf = Vec::with_capacity(n as usize);
    f.take(n).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Trim every `*.log` directly under the plugin log directory.
pub fn trim_plugin_logs(dir: &Path, max_bytes: u64, keep_bytes: u64) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut freed = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".log") {
            continue;
        }
        freed = freed.saturating_add(trim_append_only(&entry.path(), max_bytes, keep_bytes));
    }
    freed
}
