//! Sidecar freshness: one rule for every reader in the front.
//!
//! The sibling daemons publish their state as small JSON files on the `/run`
//! tmpfs, rewritten whole on each tick, so a file's mtime IS the age of the
//! reading inside it. A daemon that died leaves its last file behind until
//! reboot, so every reader that serves a sidecar as a current measurement must
//! gate on that age.
//!
//! A future mtime fails closed: a clock that stepped backwards (an RTC-less SBC
//! correcting after boot) makes the age unprovable, and an unprovable age must
//! not read as a fresh measurement.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// A file's mtime age relative to `now`, or `None` when the file is absent, its
/// mtime is unreadable, or its mtime is after `now`.
pub fn file_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    now.duration_since(modified).ok()
}

/// Whether the file exists and was written within `max_age` of `now`.
pub fn is_fresh(path: &Path, now: SystemTime, max_age: Duration) -> bool {
    file_age(path, now).is_some_and(|age| age <= max_age)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_future_mtime_is_not_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, "{}").unwrap();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        // "now" before the write: the clock stepped backwards.
        let earlier = written - Duration::from_secs(60);
        assert!(file_age(&path, earlier).is_none());
        assert!(!is_fresh(&path, earlier, Duration::from_secs(10)));
        assert!(is_fresh(
            &path,
            written + Duration::from_secs(1),
            Duration::from_secs(10)
        ));
        assert!(!is_fresh(
            &path,
            written + Duration::from_secs(11),
            Duration::from_secs(10)
        ));
        assert!(!is_fresh(
            &dir.path().join("absent"),
            written,
            Duration::from_secs(10)
        ));
    }
}
