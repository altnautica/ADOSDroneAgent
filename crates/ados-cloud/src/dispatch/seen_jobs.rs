//! Idempotency ring for cloud-relay command jobs.
//!
//! A replayed command-queue row must be a no-op. The ring records each
//! processed `jobId` with a millisecond timestamp at
//! `/var/lib/ados/plugins/.jobs/_seen_jobs.json`, capped at 10k entries and
//! rotated weekly so the file never grows unbounded. Ports
//! `_load_seen_jobs` / `_save_seen_jobs` / `already_seen` / `mark_seen` from
//! `src/ados/plugins/remote_install.py`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default ring directory. Lives under `/var/lib` so it survives reboots.
pub const SEEN_JOBS_DIR: &str = "/var/lib/ados/plugins/.jobs";

/// Maximum tracked job ids before the oldest ~10% are dropped.
pub const SEEN_JOBS_MAX: usize = 10_000;

/// Rotate (treat as empty) when the file is older than a week.
pub const SEEN_JOBS_ROTATE_SECONDS: u64 = 7 * 24 * 3600;

/// The default ring file path (`<SEEN_JOBS_DIR>/_seen_jobs.json`).
pub fn default_path() -> PathBuf {
    Path::new(SEEN_JOBS_DIR).join("_seen_jobs.json")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Load + rotate the seen-jobs map. Returns `{jobId: ts_ms}`. A missing file, a
/// file older than the rotate window, or any parse error yields an empty map
/// (never raises), matching the Python loader.
pub fn load(path: &Path) -> BTreeMap<String, i64> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return BTreeMap::new(),
    };
    // Rotate when older than a week to bound disk use.
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = SystemTime::now().duration_since(modified) {
            if age.as_secs() > SEEN_JOBS_ROTATE_SECONDS {
                return BTreeMap::new();
            }
        }
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return BTreeMap::new(),
    };
    // Defensive: tolerate float values (the Python defensive int() coercion).
    let raw: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return BTreeMap::new(),
    };
    let obj = match raw.as_object() {
        Some(o) => o,
        None => return BTreeMap::new(),
    };
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        if let Some(n) = v.as_i64() {
            out.insert(k.clone(), n);
        } else if let Some(f) = v.as_f64() {
            out.insert(k.clone(), f as i64);
        }
    }
    out
}

/// Atomic write of the seen-jobs map with a size cap. When over cap, drop the
/// oldest ~10% of entries (by timestamp) to keep churn low. Mirrors
/// `_save_seen_jobs`.
pub fn save(seen: &BTreeMap<String, i64>, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let to_write: BTreeMap<String, i64> = if seen.len() > SEEN_JOBS_MAX {
        // Sort by ts ascending; drop the oldest ~10%.
        let mut ordered: Vec<(&String, &i64)> = seen.iter().collect();
        ordered.sort_by_key(|(_, ts)| **ts);
        let drop_n = ordered.len() / 10;
        ordered
            .into_iter()
            .skip(drop_n)
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    } else {
        seen.clone()
    };
    // Atomic write: write a sibling temp file, then rename over the target.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!("._seen_jobs.{}.tmp", std::process::id()));
    let body = serde_json::to_vec(&to_write).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Whether a job id was already processed.
pub fn already_seen(job_id: &str, path: &Path) -> bool {
    load(path).contains_key(job_id)
}

/// Record a job id as processed (load-modify-save).
pub fn mark_seen(job_id: &str, path: &Path) -> std::io::Result<()> {
    let mut seen = load(path);
    seen.insert(job_id.to_string(), now_ms());
    save(&seen, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ados-seen-jobs-{}-{}/_seen_jobs.json",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        p
    }

    #[test]
    fn mark_and_already_seen_round_trip() {
        let path = temp_path("roundtrip");
        assert!(!already_seen("job-1", &path));
        mark_seen("job-1", &path).unwrap();
        assert!(already_seen("job-1", &path));
        // A different job is still unseen.
        assert!(!already_seen("job-2", &path));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_is_empty_not_an_error() {
        let path = temp_path("missing");
        assert!(load(&path).is_empty());
        assert!(!already_seen("any", &path));
    }

    #[test]
    fn over_cap_drops_oldest_ten_percent() {
        // Build a map over the cap and confirm save drops the oldest ~10%.
        let path = temp_path("cap");
        let mut seen = BTreeMap::new();
        for i in 0..(SEEN_JOBS_MAX + 100) {
            // ts ascending with index so "oldest" is deterministic.
            seen.insert(format!("job-{i:06}"), i as i64);
        }
        save(&seen, &path).unwrap();
        let reloaded = load(&path);
        let total = SEEN_JOBS_MAX + 100;
        let expected_kept = total - total / 10;
        assert_eq!(reloaded.len(), expected_kept);
        // The very oldest (smallest ts) was dropped; a recent one is kept.
        assert!(!reloaded.contains_key("job-000000"));
        assert!(reloaded.contains_key(&format!("job-{:06}", total - 1)));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn malformed_file_loads_empty() {
        let path = temp_path("malformed");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert!(load(&path).is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
