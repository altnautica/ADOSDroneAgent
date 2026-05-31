//! IPv4-resilient artifact fetcher.
//!
//! Shells out to `curl` (the workspace `ureq` is HTTP-only / no-TLS and cannot
//! fetch GitHub release assets over HTTPS). The fetch must survive the
//! confirmed field failure where a host resolves an asset to IPv6-only with no
//! usable IPv6 default route and a naive `curl` stalls ~12 s before failing:
//! force `-4` when no IPv6 default route exists, and retry with `-4` on a
//! connect timeout. Bounded retries; never hangs (curl `--max-time`).
//!
//! Implementation lands in the leaf-module phase. The signature below is the
//! frozen contract the `fetch_binaries` step + the bootstrap-equivalent logic
//! call against.

use std::path::Path;

/// Fetch `url` into `dest` (atomically: download to a temp sibling then rename),
/// IPv4-resilient with bounded retries. Returns `Err` only after retries are
/// exhausted.
pub fn fetch(_url: &str, _dest: &Path) -> anyhow::Result<()> {
    anyhow::bail!("net::fetch is implemented in the leaf-module phase")
}
