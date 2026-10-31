//! Size cap for a child's log file.

use std::path::Path;

/// Empty `path` when it is larger than `cap` bytes. Returns whether it did. A
/// missing file is not an error. Pair with [`crate::StderrMode::AppendLog`] so
/// the child keeps writing at the new end of the file.
pub async fn cap_log(path: &Path, cap: u64) -> std::io::Result<bool> {
    let len = match tokio::fs::metadata(path).await {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if len <= cap {
        return Ok(false);
    }
    let file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    file.set_len(0).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn caps_only_past_the_limit_and_skips_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("x.log");
        assert!(!cap_log(&log, 10).await.unwrap());
        std::fs::write(&log, [0u8; 20]).unwrap();
        assert!(!cap_log(&log, 20).await.unwrap());
        assert!(cap_log(&log, 19).await.unwrap());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
    }
}
