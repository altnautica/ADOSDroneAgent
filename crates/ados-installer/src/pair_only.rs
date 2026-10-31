//! Fast re-pair of an already-installed, running agent (`--pair CODE` on a
//! box the install-presence probe found on disk).

use std::path::Path;

use anyhow::Result;

use ados_term::exec;

use crate::cli::Args;
use crate::env;
use crate::steps::config_identity::{mint_pending_api_key, pairing_json};

/// Fast re-pair against an already-installed, running agent: write fresh
/// pairing material and nudge the agent to pick it up, without the full install
/// chain (no re-fetch, no re-provision). Reached only when the install-presence
/// probe confirmed the agent is on disk and a bare `--pair CODE` was given.
///
/// The agent auto-reloads `pairing.json` when its on-disk mtime is newer than
/// the in-memory copy, so writing a fresh file is sufficient for correctness;
/// restarting the cloud-relay unit forces an immediate re-read and re-beacon so
/// the operator sees the new code take effect at once rather than on the next
/// reload tick. The restart is best-effort: a write that lands but a restart
/// that fails still re-pairs on the agent's own reload.
pub fn run(args: &Args) -> Result<()> {
    let code = validate_pair_code(args.pair.as_deref())?;

    write_pairing_material(&code)?;
    nudge_cloud_relay();

    println!(
        "pair: re-paired the running agent with code {}",
        code.to_ascii_uppercase()
    );
    Ok(())
}

/// Validate the supplied pairing code (pure): trim surrounding whitespace and
/// reject an absent or blank code. Returns the trimmed code on success.
fn validate_pair_code(pair: Option<&str>) -> Result<String> {
    match pair.map(str::trim) {
        Some(c) if !c.is_empty() => Ok(c.to_string()),
        _ => anyhow::bail!("no pairing code supplied"),
    }
}

/// Write `/etc/ados/pairing.json` with the supplied code (uppercased, stamped),
/// reusing the same body builder the install's config step uses so the on-disk
/// shape never drifts, and the same durable atomic writer so a power cut can
/// never leave a truncated file and the 0600 mode is set before the file is
/// visible at its final path — it carries pairing identity.
fn write_pairing_material(code: &str) -> Result<()> {
    let pending_api_key = mint_pending_api_key();
    let body = pairing_json(code, now_epoch(), pending_api_key.as_deref());
    env::write_atomic_durable(Path::new(env::PAIRING_JSON), body.as_bytes(), Some(0o600))
        .map_err(|e| anyhow::anyhow!("write {} failed: {e}", env::PAIRING_JSON))?;
    tracing::info!(code = %code.to_ascii_uppercase(), "pairing material rewritten for re-pair");
    Ok(())
}

/// Restart the cloud unit that beacons the pair code so the new code is re-read
/// immediately. The single `ados-cloud` unit serves both profiles — it spawns
/// the ground-station bridge when the role resolves to a ground station — so one
/// unit covers every box. Only an installed-and-active unit restarts; otherwise
/// this is a harmless no-op. Best-effort by design — see [`run`].
fn nudge_cloud_relay() {
    const UNIT: &str = "ados-cloud";
    // Only restart a unit that is actually active so we never spuriously start a
    // unit that has not come up yet on this box.
    if exec::run_ok("systemctl", &["is-active", "--quiet", UNIT]) {
        if exec::run_ok("systemctl", &["restart", UNIT]) {
            tracing::info!(
                unit = UNIT,
                "restarted cloud relay to re-beacon the new pair code"
            );
        } else {
            tracing::warn!(
                unit = UNIT,
                "cloud relay restart failed; the agent will re-read pairing.json on its next reload"
            );
        }
    }
}

/// Seconds since the Unix epoch (for the pairing stamp).
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
