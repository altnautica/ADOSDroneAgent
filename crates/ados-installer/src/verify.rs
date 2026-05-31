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
//!
//! Implementation lands in the leaf-module phase. The signature below is the
//! frozen contract the `fetch_binaries` step calls against.

use std::path::Path;

/// Release channel — governs whether a missing signature is fatal.
pub enum Channel {
    /// Rolling `main` builds: SHA256-only is acceptable when no key is present.
    Edge,
    /// Pinned releases: a signature is mandatory.
    Stable,
}

/// Verify `artifact` against its `.sha256` (mandatory) and, when `pubkey` is
/// `Some`, its `.minisig`. `allow_unsigned` short-circuits the signature check.
pub fn verify_artifact(
    _artifact: &Path,
    _pubkey: Option<&str>,
    _channel: Channel,
    _allow_unsigned: bool,
) -> anyhow::Result<()> {
    anyhow::bail!("verify::verify_artifact is implemented in the leaf-module phase")
}
