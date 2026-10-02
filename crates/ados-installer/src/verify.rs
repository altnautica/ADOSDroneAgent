//! Artifact verification — port of `scripts/lib/verify.sh`.
//!
//! Mandatory SHA256 (computed in-process with `sha2` against the `.sha256`
//! sidecar) plus an Ed25519/minisign signature (`.minisig`) checked against the
//! vendored trust anchor. Where the bytes came from sets the fatality matrix:
//!   - SHA256 mismatch / missing sidecar → always fatal.
//!   - a signature present and invalid    → always fatal (tamper).
//!   - [`SignaturePolicy::Required`] (anything downloaded from a release, on
//!     every channel): a missing `.minisig` or a host with no `minisign` is
//!     fatal. The `.sha256` comes from the same host as the artifact, so on its
//!     own it proves the transfer, not the origin; an attacker who controls the
//!     download would simply withhold the signature.
//!   - [`SignaturePolicy::LocalBuild`] (`--artifacts <dir>` only): a locally
//!     built binary cannot carry the CI signature, so a signature that cannot be
//!     obtained warns and the build host's SHA256 sidecar is the gate. A
//!     signature that IS present is still verified.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::exec;

/// The trust anchor for every artifact the installer fetches from a release:
/// the public half of the minisign keypair CI signs each asset's `.minisig`
/// with (the private half is the `ADOS_DRIVER_SIGNING_KEY` CI secret). One key
/// signs every artifact class — the service binaries, the bootstrap installer,
/// the kernel modules, and the stable-channel wheel and deploy bundle — and the
/// same public half is vendored in `scripts/install.sh` and
/// `scripts/drivers/lib-prebuilt.sh`. EMBEDDED, not fetched, so a MITM on the
/// release host cannot swap the key. Key id `8DEB4E827E9D083F` (rotated 2026-07).
pub const RELEASE_PUBKEY: &str = "RWQ/CJ1+gk7rjVfGSoy6MOL50e8TmO30KD/J+goaEj+WMI1uzEf92rHN";

/// How strictly one artifact's signature is judged. Chosen by where the bytes
/// came from, never by the release channel: every channel downloads from the
/// same host, so every channel needs the same proof of origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignaturePolicy {
    /// A downloaded release asset: the `.minisig` must exist, `minisign` must be
    /// installed, and the signature must verify against the trust anchor.
    Required,
    /// A locally-built artifact the operator handed over with `--artifacts`: a
    /// present signature is verified; one that cannot be obtained warns.
    LocalBuild,
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

/// Verify the minisign signature of `artifact` against `<artifact>.minisig`
/// using `pubkey`, collapsed into the install's fatality model:
///   - verified                         → Ok(())
///   - signature INVALID                → fatal under every policy (tamper)
///   - minisign missing / no .minisig   → unverifiable: fatal for a release
///     asset, a warning for a local build
fn verify_minisign(
    artifact: &Path,
    pubkey: &str,
    policy: SignaturePolicy,
    artifact_name: &str,
) -> anyhow::Result<()> {
    let sig_path = sidecar(artifact, "minisig");
    let sig_str = sig_path.to_string_lossy();
    let art_str = artifact.to_string_lossy();

    if !sig_path.exists() {
        // No signature file — unverifiable, not tampered.
        return unverifiable(policy, artifact_name, "missing .minisig");
    }

    let res = exec::run(
        "minisign",
        &["-V", "-P", pubkey, "-m", &art_str, "-x", &sig_str],
    );
    if !res.spawned {
        // minisign not installed — unverifiable, not tampered.
        return unverifiable(policy, artifact_name, "minisign not installed");
    }
    if res.success() {
        return Ok(());
    }
    // minisign ran and rejected the signature — tamper. Fatal everywhere.
    anyhow::bail!("tamper check failed for {artifact_name}; refusing to install");
}

/// The "unverifiable but not tampered" branch: fatal for a release asset, a
/// warning for a local build (whose SHA256 sidecar came from its build host).
fn unverifiable(policy: SignaturePolicy, artifact_name: &str, why: &str) -> anyhow::Result<()> {
    match policy {
        SignaturePolicy::Required => {
            anyhow::bail!(
                "{artifact_name} could not be signature-verified ({why}); a downloaded \
                 release asset is installed only with a valid signature"
            )
        }
        SignaturePolicy::LocalBuild => {
            tracing::warn!(
                artifact = artifact_name,
                why,
                "local build signature unverifiable; SHA256-checked against its build host"
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

/// Verify `artifact` against an explicit `.sha256` sidecar path: compute the
/// file's SHA256 in-process and compare it against the digest the sidecar
/// declares. A missing/malformed sidecar or a digest mismatch is a hard error.
///
/// This is the SHA256-only check (no signature policy), exposed for callers that
/// fetch a single artifact + its `.sha256` and need only the integrity gate
/// (e.g. the release wheel install). It reuses the same streaming hasher and
/// sidecar parser as [`verify_artifact`].
pub fn verify_sha256(artifact: &Path, sidecar_path: &Path) -> anyhow::Result<()> {
    let name = artifact
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| artifact.to_string_lossy().into_owned());

    let sidecar_contents = std::fs::read_to_string(sidecar_path).map_err(|_| {
        anyhow::anyhow!(
            "SHA256 verification failed for {name}: missing {}",
            sidecar_path.display()
        )
    })?;
    let declared = parse_sha256_sidecar(&sidecar_contents)
        .map_err(|e| anyhow::anyhow!("SHA256 verification failed for {name}: {e}"))?;
    let computed = sha256_hex(artifact)?;
    if sha_check(&computed, &declared) == ShaCheck::Mismatch {
        anyhow::bail!("SHA256 verification failed for {name}");
    }
    Ok(())
}

/// Verify `artifact` against its `.sha256` (mandatory) and its `.minisig`
/// against `pubkey`, under `policy`.
pub fn verify_artifact(
    artifact: &Path,
    pubkey: &str,
    policy: SignaturePolicy,
) -> anyhow::Result<()> {
    let name = artifact
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| artifact.to_string_lossy().into_owned());

    // ── SHA256 is always mandatory. ──
    let sha_path = sidecar(artifact, "sha256");
    let sidecar_contents = std::fs::read_to_string(&sha_path).map_err(|_| {
        anyhow::anyhow!("SHA256 verification failed for {name}: missing {name}.sha256")
    })?;
    let declared = parse_sha256_sidecar(&sidecar_contents)
        .map_err(|e| anyhow::anyhow!("SHA256 verification failed for {name}: {e}"))?;
    let computed = sha256_hex(artifact)?;
    if sha_check(&computed, &declared) == ShaCheck::Mismatch {
        anyhow::bail!("SHA256 verification failed for {name}");
    }

    verify_minisign(artifact, pubkey, policy, &name)
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
        std::fs::File::create(&art)
            .unwrap()
            .write_all(bytes)
            .unwrap();

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
    fn verify_sha256_matches_and_mismatches() {
        // A matching sidecar passes; a wrong digest is fatal; a missing sidecar
        // is fatal.
        let (dir_ok, art_ok) = artifact_with_sha(b"wheel bytes", true);
        let sidecar_ok = dir_ok.path().join("ados-video-aarch64.sha256");
        assert!(verify_sha256(&art_ok, &sidecar_ok).is_ok());

        let (dir_bad, art_bad) = artifact_with_sha(b"wheel bytes", false);
        let sidecar_bad = dir_bad.path().join("ados-video-aarch64.sha256");
        let e = verify_sha256(&art_bad, &sidecar_bad).unwrap_err();
        assert!(e.to_string().contains("SHA256 verification failed"));

        let missing = dir_ok.path().join("does-not-exist.sha256");
        assert!(verify_sha256(&art_ok, &missing).is_err());
    }

    #[test]
    fn bad_sha_is_always_fatal() {
        let (_d, art) = artifact_with_sha(b"hello world", false);
        // Mismatch is fatal under either policy, before any signature check.
        for policy in [SignaturePolicy::Required, SignaturePolicy::LocalBuild] {
            let e = verify_artifact(&art, "REALKEY", policy).unwrap_err();
            assert!(e.to_string().contains("SHA256 verification failed"));
        }
    }

    #[test]
    fn missing_sha_sidecar_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let art = dir.path().join("ados-video-aarch64");
        std::fs::write(&art, b"x").unwrap();
        let e = verify_artifact(&art, "REALKEY", SignaturePolicy::LocalBuild).unwrap_err();
        assert!(e.to_string().contains("SHA256 verification failed"));
    }

    #[test]
    fn a_release_asset_without_a_signature_is_refused() {
        // The `.sha256` comes from the same host as the binary, so it cannot
        // prove origin: whoever controls the download would withhold the
        // `.minisig` and ship a matching digest. A downloaded asset with no
        // signature is therefore refused, whatever channel the box is on.
        let (_d, art) = artifact_with_sha(b"payload", true);
        let e = verify_artifact(&art, "REALKEY", SignaturePolicy::Required).unwrap_err();
        assert!(e.to_string().contains("missing .minisig"), "{e}");
    }

    #[test]
    fn a_local_build_without_a_signature_installs_on_its_sha256() {
        let (_d, art) = artifact_with_sha(b"payload", true);
        assert!(verify_artifact(&art, "REALKEY", SignaturePolicy::LocalBuild).is_ok());
    }

    #[test]
    fn a_garbage_signature_is_refused_even_for_a_local_build() {
        // A present signature is checked under every policy. Whether minisign
        // reports it invalid (tamper) or is absent from this host, a release
        // asset is refused; a local build is refused only for the tamper case.
        let (_d, art) = artifact_with_sha(b"payload", true);
        std::fs::write(sidecar(&art, "minisig"), b"not a signature").unwrap();
        assert!(verify_artifact(&art, RELEASE_PUBKEY, SignaturePolicy::Required).is_err());
        if exec::run("minisign", &["-v"]).spawned {
            let e = verify_artifact(&art, RELEASE_PUBKEY, SignaturePolicy::LocalBuild).unwrap_err();
            assert!(e.to_string().contains("tamper"), "{e}");
        }
    }
}
