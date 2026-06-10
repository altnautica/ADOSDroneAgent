//! The default locked-channel hint sink (`FileLockedChannelHint`): an atomic
//! single-int + newline write to the Contract-E hint file.
//!
//! INVARIANT: it NEVER writes the operator's immutable config home channel — a
//! locked channel is recorded ONLY as a tmpfs runtime hint so a restart can try
//! it first; the home channel where both sides deterministically meet is never
//! auto-overwritten.

use super::seams::LockedChannelHint;

/// Default `LockedChannelHint`: atomic tmp-write + rename of a single integer
/// channel followed by a newline to the Contract-E hint file. A failure is not
/// fatal to the live link (a restart just sweeps from the home channel again).
/// NEVER writes the operator's config home channel (see the module invariant).
pub struct FileLockedChannelHint;

impl LockedChannelHint for FileLockedChannelHint {
    fn persist(&self, channel: u8) {
        let path = std::path::Path::new(crate::paths::WFB_LOCKED_CHANNEL_HINT);
        if let Err(e) = persist_hint(path, channel) {
            tracing::warn!(channel, error = %e, "ground_wfb_channel_hint_persist_failed");
        } else {
            tracing::info!(channel, "ground_wfb_channel_hint_persisted");
        }
    }
}

/// Atomic single-int + newline write to `path` (tmp sibling + rename). Mirrors
/// the Python `tmp.write_text(f"{int(channel)}\n"); tmp.replace(path)`.
fn persist_hint(path: &std::path::Path, channel: u8) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Python uses `with_suffix(suffix + ".tmp")` → `wfb-locked-channel.tmp`.
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        // Single integer + trailing newline (mirrors the Python `f"{int}\n"`).
        writeln!(f, "{channel}")?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_hint_writes_single_int_newline_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-locked-channel");
        persist_hint(&path, 157).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "157\n");
        // No leftover tmp sibling.
        assert!(!dir.path().join("wfb-locked-channel.tmp").exists());
    }
}
