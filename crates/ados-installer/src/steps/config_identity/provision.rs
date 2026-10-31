//! The side-effecting provisioning steps: device id, config files, hostname, pairing and overlays.

use std::path::Path;

use ados_term::exec;

use crate::ctx::Ctx;
use crate::env::{CONFIG_YAML, DEVICE_ID_FILE, PAIRING_JSON, PROFILE_CONF};

use super::render::{
    cutover_note_body, default_config_yaml, default_display_arg, mint_pending_api_key,
    pairing_json, profile_conf_body, rewrite_hosts_body, slugify_hostname, CUTOVER_NOTE_PATH,
};
use super::{now_epoch, set_mode};

/// Mint or read the stable 12-hex device id. Never overwrites an existing id
/// (the running agent's identity is derived from it). Prefers the kernel UUID
/// source, falling back to `getrandom` so the mint works on any host.
pub(super) fn ensure_device_id() -> anyhow::Result<String> {
    let path = Path::new(DEVICE_ID_FILE);
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            tracing::info!(device_id = %trimmed, "device identity exists");
            return Ok(trimmed.to_string());
        }
    }
    let id = mint_device_id();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::env::write_atomic_durable(path, format!("{id}\n").as_bytes(), Some(0o644))?;
    tracing::info!(device_id = %id, "device identity generated");
    Ok(id)
}

/// Generate a fresh 12-hex device id. Reads `/proc/sys/kernel/random/uuid`
/// when present (matching the bash), else fills 6 bytes from `getrandom`.
fn mint_device_id() -> String {
    if let Ok(uuid) = std::fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let hex: String = uuid
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(12)
            .collect();
        if hex.len() == 12 {
            return hex.to_ascii_lowercase();
        }
    }
    let mut bytes = [0u8; 6];
    if getrandom::fill(&mut bytes).is_ok() {
        return hex::encode(bytes);
    }
    // Last-ditch deterministic-ish fallback (should never be reached): derive
    // from the process id + a fixed salt so we still produce 12 hex chars.
    let pid = std::process::id();
    format!("{pid:012x}").chars().take(12).collect()
}

/// Auto-detect the FC serial port. Mirrors the bash glob order
/// (`/dev/ttyACM* /dev/ttyAMA* /dev/ttyUSB*`), returning the first node found
/// or an empty string. The agent re-detects at runtime; this is only the seed.
fn detect_fc_port() -> String {
    for prefix in ["/dev/ttyACM", "/dev/ttyAMA", "/dev/ttyUSB"] {
        let dir = Path::new("/dev");
        if let Ok(read) = std::fs::read_dir(dir) {
            let base = prefix.trim_start_matches("/dev/");
            let mut matches: Vec<String> = read
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with(base) && n.len() > base.len())
                .collect();
            matches.sort();
            if let Some(first) = matches.first() {
                return format!("/dev/{first}");
            }
        }
    }
    String::new()
}

/// The default human-readable node name: `ados-` plus the first 8 hex of the
/// device id. A label only; the identity is always the full id.
pub(super) fn default_agent_name(device_id: &str) -> String {
    let short: String = device_id.chars().take(8).collect();
    format!("ados-{short}")
}

/// Write the default config (skip-if-exists, matching `generate_default_config`).
/// A pre-existing config is left intact (an operator may have edited it / an
/// `--upgrade` preserves it); we only mint a fresh one on a clean box. A short
/// `agent.device_id` an older install left there is harmless: every reader
/// resolves the identity file first.
///
/// `server_mode` (`local` local-first / `cloud` reach-from-anywhere) and
/// `region` (an ISO country code, or `None` for the unrestricted default) come
/// from the operator's onboarding choices.
pub(super) fn write_default_config(
    device_id: &str,
    name: Option<&str>,
    profile: &str,
    server_mode: &str,
    region: Option<&str>,
) {
    let path = Path::new(CONFIG_YAML);
    if path.exists() {
        tracing::info!(
            path = CONFIG_YAML,
            "config already exists; leaving it intact"
        );
        return;
    }
    let agent_name = name
        .map(|n| n.to_string())
        .unwrap_or_else(|| default_agent_name(device_id));
    let fc_port = detect_fc_port();
    if !fc_port.is_empty() {
        tracing::info!(fc_port = %fc_port, "detected flight controller serial port");
    }
    // Stamp the region acknowledgement only when a region is actually pinned.
    let ack_at = if region.is_some() {
        crate::result::now_iso8601_utc()
    } else {
        String::new()
    };
    let body = default_config_yaml(
        device_id,
        &agent_name,
        profile,
        &fc_port,
        server_mode,
        region,
        &ack_at,
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = crate::env::write_atomic_durable(path, body.as_bytes(), Some(0o600)) {
        tracing::warn!(error = %e, "writing default config failed");
        return;
    }
    tracing::info!("default config written");
}

/// Persist the resolved profile to `/etc/ados/profile.conf` so it survives
/// reboots and a later `--upgrade`. Idempotent overwrite of the one-line file.
pub(super) fn write_profile_conf(profile: &str, channel: &str, version: Option<&str>) {
    let path = Path::new(PROFILE_CONF);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = crate::env::write_atomic_durable(
        path,
        profile_conf_body(profile, channel, version).as_bytes(),
        Some(0o644),
    ) {
        tracing::warn!(error = %e, "writing profile.conf failed");
    }
}

/// Set the system hostname from `--name` (slugified), rewrite the `/etc/hosts`
/// `127.0.1.1` line, and restart avahi so `<slug>.local` resolves. Best-effort
/// — a hostname failure must not abort the install.
pub(super) fn set_hostname(name: &str) {
    let slug = slugify_hostname(name);
    if slug.is_empty() {
        tracing::warn!(
            name,
            "--name slugified to empty; leaving hostname unchanged"
        );
        return;
    }
    // Idempotent: if the box is already this hostname, do nothing. A wizard
    // "Keep the current name" choice re-passes the existing hostname, and it
    // must not churn hostnamectl or restart avahi for a no-op rename.
    if crate::env::read_hostname() == slug {
        tracing::info!(slug = %slug, "hostname already set; leaving it unchanged");
        return;
    }
    // hostnamectl is the canonical setter; fall back to /etc/hostname.
    if !exec::run_ok("hostnamectl", &["set-hostname", &slug]) {
        tracing::warn!(slug = %slug, "hostnamectl failed; writing /etc/hostname directly");
        let _ = crate::env::write_atomic_durable(
            Path::new("/etc/hostname"),
            format!("{slug}\n").as_bytes(),
            Some(0o644),
        );
    } else {
        tracing::info!(slug = %slug, "hostname set");
    }
    rewrite_hosts_127_0_1_1(&slug);
    // Reconcile mDNS so <slug>.local resolves (best-effort).
    let _ = exec::run("systemctl", &["restart", "avahi-daemon"]);
}

/// Keep the `/etc/hosts` `127.0.1.1` line pointing at the new hostname so name
/// resolution does not stall. Rewrites that one line, or appends it.
fn rewrite_hosts_127_0_1_1(slug: &str) {
    let path = Path::new("/etc/hosts");
    let current = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let new = rewrite_hosts_body(&current, slug);
    if new != current {
        let _ = std::fs::write(path, new);
    }
}

/// Persist the pairing material from `--pair CODE`.
pub(super) fn write_pairing(code: &str) {
    let path = Path::new(PAIRING_JSON);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let pending_api_key = mint_pending_api_key();
    let body = pairing_json(code, now_epoch(), pending_api_key.as_deref());
    if let Err(e) = crate::env::write_atomic_durable(path, body.as_bytes(), Some(0o600)) {
        tracing::warn!(error = %e, "writing pairing.json failed");
        return;
    }
    tracing::info!(code = %code.to_ascii_uppercase(), "pairing code written");
}

/// Delegate the display + camera overlay provisioning to the brick-safe driver
/// scripts under the resolved source dir. Best-effort: a missing script or a
/// non-zero exit only degrades (the bash treats both as non-fatal).
///
/// Display default is profile-aware (the bash inverts the two): a ground
/// station auto-detects its panel; a drone defaults to `none`. Camera is the
/// inverse — a drone defaults to `auto`, a ground station to `none` — but the
/// camera auto-enable is still operator-gated, so without an explicit
/// `--camera` we pass `none` to match the bash safety gate.
pub(super) fn provision_overlays(ctx: &Ctx, source: Option<&Path>) {
    let source = match source {
        Some(s) => s,
        None => {
            tracing::warn!("no source dir resolved; skipping overlay provisioning");
            return;
        }
    };

    // ── Display ──
    let display_script = source.join("scripts/drivers/install-display-overlay.sh");
    if display_script.is_file() {
        let display_arg = ctx
            .args
            .display
            .clone()
            .unwrap_or_else(|| default_display_arg(&ctx.profile).to_string());
        let script_s = display_script.to_string_lossy();
        tracing::info!(display = %display_arg, "running display overlay installer");
        if !exec::run_ok(&script_s, &["--display", &display_arg]) {
            tracing::warn!("display overlay install returned non-zero (best-effort)");
        }
    } else {
        tracing::warn!("display overlay installer not found; skipping display provisioning");
    }

    // ── Camera ── operator-gated auto-enable: only auto when --camera given.
    let camera_script = source.join("scripts/drivers/install-camera-overlay.sh");
    if camera_script.is_file() {
        let camera_arg = ctx
            .args
            .camera
            .clone()
            .unwrap_or_else(|| "none".to_string());
        let script_s = camera_script.to_string_lossy();
        tracing::info!(camera = %camera_arg, "running camera overlay installer");
        if !exec::run_ok(&script_s, &["--camera", &camera_arg]) {
            tracing::warn!("camera overlay install returned non-zero (best-effort)");
        }
    } else {
        tracing::warn!("camera overlay installer not found; skipping camera provisioning");
    }
}

/// Deploy the operator CUTOVER note at `/etc/ados/CUTOVER`. Idempotent overwrite
/// — the text is stable across reinstalls. Best-effort: a write failure only
/// loses the note, it never aborts the install.
pub(super) fn write_cutover_note() {
    let path = Path::new(CUTOVER_NOTE_PATH);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, cutover_note_body()) {
        tracing::warn!(error = %e, "writing CUTOVER note failed");
        return;
    }
    set_mode(path, 0o644);
    tracing::info!("radio cutover note written");
}
