//! Artifact verification — port of `scripts/lib/verify.sh`.
//!
//! Mandatory SHA256 (computed in-process with `sha2` against the `.sha256`
//! sidecar) plus an optional Ed25519/minisign signature (`.minisig`). The
//! channel + `allow_unsigned` flag set the fatality matrix exactly as the bash
//! `ados_verify_artifact`:
//!   - SHA256 mismatch                → always fatal.
//!   - `allow_unsigned == true`       → signature skipped (dev default).
//!   - pubkey empty, channel == Edge  → SHA256-only, warn, OK.
//!   - pubkey empty, channel == Stable→ fatal (refuse unsigned on stable).
//!   - pubkey present                 → minisign signature mandatory.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::exec;

/// Release channel — governs whether a missing signature is fatal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Rolling `main` builds: SHA256-only is acceptable when no key is present.
    Edge,
    /// Pinned releases: a signature is mandatory.
    Stable,
}

/// The outcome of the in-process SHA256 check against the `.sha256` sidecar.
/// Pure: takes the digest the sidecar declares and the digest we computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShaCheck {
    /// The computed digest matches the sidecar's declared digest.
    Match,
    /// They differ (tamper / truncation) — always fatal.
    Mismatch,
}

/// Compute the lowercase-hex SHA256 of a file, streaming it (the binaries are a
/// few MB; an 8 KiB buffer keeps memory flat regardless of size).
fn sha256_hex(path: &Path) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("cannot open {} for hashing: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| anyhow::anyhow!("read error hashing {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Parse the leading hex digest token out of a `sha256sum`-format sidecar line
/// (`<hex>␠␠<name>`). Pure — unit-testable without a file. Returns the lowercase
/// hex digest, or an error when the sidecar is empty / malformed.
fn parse_sha256_sidecar(contents: &str) -> anyhow::Result<String> {
    let first = contents
        .lines()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("empty .sha256 sidecar"))?;
    let token = first
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("malformed .sha256 sidecar"))?;
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("malformed sha256 digest in sidecar: {token:?}");
    }
    Ok(token.to_ascii_lowercase())
}

/// Compare a computed digest against the sidecar's declared digest (pure).
fn sha_check(computed: &str, declared: &str) -> ShaCheck {
    if computed.eq_ignore_ascii_case(declared) {
        ShaCheck::Match
    } else {
        ShaCheck::Mismatch
    }
}

/// Decide whether an unverifiable-but-untampered signature situation is fatal,
/// given the channel + whether a pubkey was supplied + whether `allow_unsigned`
/// is set. Pure — this is the heart of the bash fatality matrix, isolated so the
/// branches are unit-testable without any files.
///
/// Returns `Ok(())` to proceed (possibly with a warning), `Err(msg)` to fail.
/// Only called once the mandatory SHA256 has already passed.
fn signature_policy(
    pubkey: Option<&str>,
    channel: Channel,
    allow_unsigned: bool,
    artifact_name: &str,
) -> Result<(), String> {
    // allow_unsigned force-skips the signature on any channel.
    if allow_unsigned {
        tracing::warn!(artifact = artifact_name, "allow-unsigned set; skipping signature check");
        return Ok(());
    }

    // Treat an empty pubkey string the same as None (CI has not substituted a
    // real key yet) — matches the bash `[ -z "$pubkey" ]`.
    let key = pubkey.filter(|k| !k.is_empty());

    match key {
        None => match channel {
            Channel::Stable => Err(format!(
                "no signing key available; refusing unsigned {artifact_name} on stable channel"
            )),
            Channel::Edge => {
                tracing::warn!(
                    artifact = artifact_name,
                    "no signing key; SHA256-checked only (edge channel)"
                );
                Ok(())
            }
        },
        // A key IS present → the signature is mandatory and is verified by the
        // caller below. This branch only signals "go run minisign".
        Some(_) => Ok(()),
    }
}

/// Verify the minisign signature of `artifact` against `<artifact>.minisig`
/// using the provided pubkey. Mirrors bash `ados_verify_minisign` return codes,
/// collapsed into the install's fatality model:
///   - verified              → Ok(())
///   - signature INVALID     → fatal everywhere (tamper)
///   - minisign missing / no .minisig → unverifiable: fatal on stable, warn+OK on edge
fn verify_minisign(
    artifact: &Path,
    pubkey: &str,
    channel: Channel,
    artifact_name: &str,
) -> anyhow::Result<()> {
    let sig_path = sidecar(artifact, "minisig");
    let sig_str = sig_path.to_string_lossy();
    let art_str = artifact.to_string_lossy();

    if !sig_path.exists() {
        // No signature file — unverifiable, not tampered.
        return unverifiable(channel, artifact_name, "missing .minisig");
    }

    let res = exec::run(
        "minisign",
        &["-V", "-P", pubkey, "-m", &art_str, "-x", &sig_str],
    );
    if !res.spawned {
        // minisign not installed — unverifiable, not tampered.
        return unverifiable(channel, artifact_name, "minisign not installed");
    }
    if res.success() {
        return Ok(());
    }
    // minisign ran and rejected the signature — tamper. Fatal on every channel.
    anyhow::bail!("tamper check failed for {artifact_name}; refusing to install");
}

/// The "unverifiable but not tampered" branch: fatal on stable, warn+OK on edge.
fn unverifiable(channel: Channel, artifact_name: &str, why: &str) -> anyhow::Result<()> {
    match channel {
        Channel::Stable => {
            anyhow::bail!("{artifact_name} could not be signature-verified on stable channel ({why})")
        }
        Channel::Edge => {
            tracing::warn!(
                artifact = artifact_name,
                why,
                "signature unverifiable; SHA256-checked only (edge channel)"
            );
            Ok(())
        }
    }
}

/// The `<artifact>.<ext>` sidecar path.
fn sidecar(artifact: &Path, ext: &str) -> std::path::PathBuf {
    let mut s = artifact.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    std::path::PathBuf::from(s)
}

/// Verify `artifact` against its `.sha256` (mandatory) and, when `pubkey` is
/// `Some`, its `.minisig`. `allow_unsigned` short-circuits the signature check.
pub fn verify_artifact(
    artifact: &Path,
    pubkey: Option<&str>,
    channel: Channel,
    allow_unsigned: bool,
) -> anyhow::Result<()> {
    let name = artifact
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| artifact.to_string_lossy().into_owned());

    // ── SHA256 is always mandatory, on every channel. ──
    let sha_path = sidecar(artifact, "sha256");
    let sidecar_contents = std::fs::read_to_string(&sha_path)
        .map_err(|_| anyhow::anyhow!("SHA256 verification failed for {name}: missing {name}.sha256"))?;
    let declared = parse_sha256_sidecar(&sidecar_contents)
        .map_err(|e| anyhow::anyhow!("SHA256 verification failed for {name}: {e}"))?;
    let computed = sha256_hex(artifact)?;
    if sha_check(&computed, &declared) == ShaCheck::Mismatch {
        anyhow::bail!("SHA256 verification failed for {name}");
    }

    // ── Signature policy (channel + key + allow_unsigned). ──
    signature_policy(pubkey, channel, allow_unsigned, &name).map_err(|m| anyhow::anyhow!(m))?;

    // allow_unsigned already short-circuited inside signature_policy; if a real
    // key is present (and we are not skipping), the signature is mandatory.
    if !allow_unsigned {
        if let Some(key) = pubkey.filter(|k| !k.is_empty()) {
            verify_minisign(artifact, key, channel, &name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `bytes` to a tempfile + its `.sha256` sidecar (good or bad digest),
    /// returning the tempdir guard + the artifact path.
    fn artifact_with_sha(bytes: &[u8], good: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let art = dir.path().join("ados-video-aarch64");
        std::fs::File::create(&art).unwrap().write_all(bytes).unwrap();

        let digest = if good {
            sha256_hex(&art).unwrap()
        } else {
            // A valid-shape but wrong digest.
            "00".repeat(32)
        };
        let sha = dir.path().join("ados-video-aarch64.sha256");
        // sha256sum format: "<hex>␠␠<name>".
        std::fs::write(&sha, format!("{digest}  ados-video-aarch64\n")).unwrap();
        (dir, art)
    }

    #[test]
    fn sha256_of_known_bytes_matches_known_digest() {
        // SHA256("abc") is the canonical NIST test vector.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc.bin");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_hex(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha_compare_matches_and_mismatches() {
        assert_eq!(sha_check("abcDEF", "ABCdef"), ShaCheck::Match);
        assert_eq!(sha_check("aa", "bb"), ShaCheck::Mismatch);
    }

    #[test]
    fn parse_sidecar_extracts_first_hex_token() {
        let d = parse_sha256_sidecar("deadBEEF  some-file\nignored line\n").unwrap();
        assert_eq!(d, "deadbeef");
    }

    #[test]
    fn parse_sidecar_rejects_garbage() {
        assert!(parse_sha256_sidecar("not-hex  file").is_err());
        assert!(parse_sha256_sidecar("   \n").is_err());
    }

    #[test]
    fn good_sha_edge_no_key_passes() {
        let (_d, art) = artifact_with_sha(b"hello world", true);
        // Edge channel, no key, default allow_unsigned -> SHA-only, OK.
        assert!(verify_artifact(&art, None, Channel::Edge, true).is_ok());
        // Even with allow_unsigned off, edge + no key is OK (SHA256-only warn).
        assert!(verify_artifact(&art, None, Channel::Edge, false).is_ok());
    }

    #[test]
    fn bad_sha_is_always_fatal() {
        let (_d, art) = artifact_with_sha(b"hello world", false);
        // Mismatch is fatal regardless of channel / allow_unsigned.
        let e = verify_artifact(&art, None, Channel::Edge, true).unwrap_err();
        assert!(e.to_string().contains("SHA256 verification failed"));
        assert!(verify_artifact(&art, None, Channel::Stable, false).is_err());
    }

    #[test]
    fn missing_sha_sidecar_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let art = dir.path().join("ados-video-aarch64");
        std::fs::write(&art, b"x").unwrap();
        let e = verify_artifact(&art, None, Channel::Edge, true).unwrap_err();
        assert!(e.to_string().contains("SHA256 verification failed"));
    }

    // ── The fatality matrix, exercised on the pure policy fn. ──

    #[test]
    fn policy_allow_unsigned_skips_everywhere() {
        assert!(signature_policy(None, Channel::Stable, true, "x").is_ok());
        assert!(signature_policy(Some("KEY"), Channel::Stable, true, "x").is_ok());
    }

    #[test]
    fn policy_empty_key_edge_ok_stable_fatal() {
        // Edge + no key → OK (SHA-only).
        assert!(signature_policy(None, Channel::Edge, false, "x").is_ok());
        assert!(signature_policy(Some(""), Channel::Edge, false, "x").is_ok());
        // Stable + no key → fatal.
        assert!(signature_policy(None, Channel::Stable, false, "x").is_err());
        assert!(signature_policy(Some(""), Channel::Stable, false, "x").is_err());
    }

    #[test]
    fn policy_present_key_requires_signature_step() {
        // A present key passes the policy gate (the actual minisign run happens
        // after); the policy itself does not reject.
        assert!(signature_policy(Some("REALKEY"), Channel::Edge, false, "x").is_ok());
        assert!(signature_policy(Some("REALKEY"), Channel::Stable, false, "x").is_ok());
    }

    #[test]
    fn good_sha_present_key_no_minisig_edge_warns_stable_fatal() {
        let (_d, art) = artifact_with_sha(b"payload", true);
        // Key present, no .minisig sidecar present: edge tolerates (unverifiable
        // not tampered), stable refuses.
        assert!(verify_artifact(&art, Some("REALKEY"), Channel::Edge, false).is_ok());
        let e = verify_artifact(&art, Some("REALKEY"), Channel::Stable, false).unwrap_err();
        assert!(e.to_string().contains("stable"));
    }
}
