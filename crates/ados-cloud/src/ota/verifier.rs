//! SHA256 verification for a downloaded update wheel.
//!
//! Ports `verify_sha256` in `src/ados/services/ota/verifier.py`: streaming
//! SHA256 over the file, compared to the expected hex digest (case-insensitive
//! on the expected side, matching the Python `expected_hash.lower()`).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Read chunk size for the streaming hash. Matches the Python `HASH_CHUNK_SIZE`.
const HASH_CHUNK_SIZE: usize = 65536;

/// Compute the streaming SHA256 of `path` and compare to `expected_hash`.
///
/// Returns `false` when the file is missing/unreadable or the digest does not
/// match. The expected digest is lowercased before comparison, mirroring the
/// Python `actual == expected_hash.lower()`.
pub fn verify_sha256(path: impl AsRef<Path>, expected_hash: &str) -> bool {
    let path = path.as_ref();
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => {
            tracing::error!(path = %path.display(), "verify sha256: file missing");
            return false;
        }
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK_SIZE];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => {
                tracing::error!(path = %path.display(), "verify sha256: read failed");
                return false;
            }
        };
        hasher.update(&buf[..n]);
    }
    let actual = hex::encode(hasher.finalize());
    let matched = actual == expected_hash.to_ascii_lowercase();
    if matched {
        tracing::info!(path = %path.display(), "sha256 verified");
    } else {
        tracing::error!(
            path = %path.display(),
            expected = %expected_hash,
            actual = %actual,
            "sha256 mismatch"
        );
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ados-ota-verify-{}-{}", std::process::id(), name));
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn matching_digest_verifies_case_insensitively() {
        // SHA256("ados") is a known constant; compute it once and check both
        // cases of the expected hex compare equal.
        let path = temp_file("match", b"ados");
        let expect = hex::encode(Sha256::digest(b"ados"));
        assert!(verify_sha256(&path, &expect));
        assert!(verify_sha256(&path, &expect.to_ascii_uppercase()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wrong_digest_fails() {
        let path = temp_file("wrong", b"ados");
        assert!(!verify_sha256(&path, &"00".repeat(32)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_fails() {
        assert!(!verify_sha256(
            "/nonexistent/ados/wheel.whl",
            &"ab".repeat(32)
        ));
    }
}
