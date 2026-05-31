//! Purge residue: clear leftovers from a prior failed/partial install or a
//! profile flip (the GS uplink-router's orphan default route + the SPI-LCD boot
//! config residue). Optional — best-effort cleanup that never blocks the
//! install.
//!
//! Two pure transforms carry the logic:
//!   - [`orphan_default_route`] parses `ip route show default` and returns the
//!     `ip route del ...` args for a gateway-less `default dev <if> scope link`
//!     route the ground-station uplink router leaves behind.
//!   - [`revert_lcd_config`] rewrites a `/boot/firmware/config.txt` that the
//!     SPI-LCD installer edited (added `dtoverlay=waveshare35a`, commented out
//!     `dtoverlay=vc4-kms-v3d`) back to a clean drone-profile state.
//!
//! Both are idempotent: re-running on an already-clean box is a no-op.

use std::path::Path;

use crate::ctx::Ctx;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// Parse `ip route show default` output and return the `ip route del ...`
/// argument vector for an orphan gateway-less default route, or `None` when no
/// such route exists. Pure.
///
/// The GS uplink-router can leave a `default dev <if> scope link` route with NO
/// `via <gw>` gateway when a failover tears down mid-flight. That route hijacks
/// the whole default path with no usable next hop. A legitimate default route
/// is `default via <gw> dev <if> ...`; we only delete the gateway-less form.
pub fn orphan_default_route(route_show_output: &str) -> Option<Vec<String>> {
    for line in route_show_output.lines() {
        let line = line.trim();
        if !line.starts_with("default") {
            continue;
        }
        let has_gateway = line.contains(" via ");
        let is_scope_link = line.contains("scope link");
        if has_gateway || !is_scope_link {
            // A normal `default via <gw> ...` route, or a default route that is
            // not the scope-link orphan shape — leave it alone.
            continue;
        }
        // Extract the interface: `default dev <if> scope link ...`.
        let mut toks = line.split_whitespace();
        // toks: ["default", "dev", "<if>", "scope", "link", ...]
        if toks.next() != Some("default") {
            continue;
        }
        if toks.next() != Some("dev") {
            continue;
        }
        if let Some(iface) = toks.next() {
            return Some(vec![
                "route".to_string(),
                "del".to_string(),
                "default".to_string(),
                "dev".to_string(),
                iface.to_string(),
                "scope".to_string(),
                "link".to_string(),
            ]);
        }
    }
    None
}

/// Rewrite a `/boot/firmware/config.txt` body, undoing the SPI-LCD installer's
/// edits so a drone-profile box boots with the GPU KMS overlay restored. Pure.
///
/// Forward edit (from the LCD installer) was: append `dtoverlay=waveshare35a`
/// and rewrite `dtoverlay=vc4-kms-v3d[,args]` to
/// `# dtoverlay=vc4-kms-v3d  # disabled by ADOS LCD installer (claims fb0)`.
/// The revert:
///   - drops every `dtoverlay=waveshare35a` line (commented or not),
///   - un-comments any `# dtoverlay=vc4-kms-v3d...` (and KMS variants like
///     `vc4-fkms-v3d`) back to an active `dtoverlay=vc4-kms-v3d` line, trimming
///     the ADOS "disabled by" trailer.
///
/// Idempotent: a config with no waveshare line and an already-active KMS
/// overlay is returned unchanged.
pub fn revert_lcd_config(cfg: &str) -> String {
    let had_trailing_newline = cfg.ends_with('\n');
    let mut out: Vec<String> = Vec::new();

    for raw in cfg.lines() {
        let trimmed = raw.trim_start();
        let stripped_hash = trimmed.trim_start_matches('#').trim_start();

        // Drop any waveshare35a overlay line (active or commented) — it is the
        // SPI panel overlay the drone profile must not carry.
        if stripped_hash.starts_with("dtoverlay=waveshare35a") {
            continue;
        }

        // Un-comment a commented-out KMS overlay back to active.
        if trimmed.starts_with('#') && is_kms_overlay(stripped_hash) {
            // Preserve leading whitespace before the original '#'.
            let indent: String = raw.chars().take_while(|c| c.is_whitespace()).collect();
            out.push(format!("{indent}{}", kms_overlay_token(stripped_hash)));
            continue;
        }

        out.push(raw.to_string());
    }

    let mut joined = out.join("\n");
    if had_trailing_newline {
        joined.push('\n');
    }
    joined
}

/// True when the (hash-stripped) token is a VideoCore KMS/FKMS overlay line.
fn is_kms_overlay(token: &str) -> bool {
    token.starts_with("dtoverlay=vc4-kms-v3d") || token.starts_with("dtoverlay=vc4-fkms-v3d")
}

/// Extract the bare `dtoverlay=vc4-...-v3d[,args]` token, dropping any trailing
/// ` # disabled by ...` comment the LCD installer appended. Preserves overlay
/// arguments (the part after a comma) up to the comment.
fn kms_overlay_token(token: &str) -> String {
    // Cut at the first ' #' (the appended comment), keep what precedes it.
    let cut = token.find(" #").map(|i| &token[..i]).unwrap_or(token);
    cut.trim_end().to_string()
}

/// Best-effort cleanup of prior-install residue.
pub struct PurgeResidue;

impl Step for PurgeResidue {
    fn id(&self) -> &str {
        "purge_residue"
    }
    fn requires(&self) -> &[&str] {
        &[]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        revert_residue();
        StepOutcome::Ok
    }
}

/// Run both residue reversions (orphan default route + SPI-LCD boot config).
/// Shared by the install step and the uninstall path so a GS→drone flip and a
/// full uninstall leave an identically clean box. Idempotent + best-effort.
pub fn revert_residue() {
    purge_orphan_route();
    revert_lcd_boot_config();
}

/// (a) Detect + delete the orphan default route via `ip`.
fn purge_orphan_route() {
    let show = exec::run("ip", &["route", "show", "default"]);
    if !show.success() {
        // No `ip`, or no default route — nothing to do.
        return;
    }
    if let Some(args) = orphan_default_route(&show.stdout) {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        if exec::run_ok("ip", &argv) {
            tracing::info!("removed orphan gateway-less default route");
        } else {
            tracing::warn!("orphan default route delete failed (best-effort)");
        }
    }
}

/// (b) Revert the SPI-LCD residue in the Pi boot config, snapshotting the file
/// to `<cfg>.ados-bak` before writing. No-op when the config is absent or the
/// revert produces no change (idempotent).
fn revert_lcd_boot_config() {
    // The Pi-family config lives at one of these two paths.
    for cfg_path in ["/boot/firmware/config.txt", "/boot/config.txt"] {
        let path = Path::new(cfg_path);
        let current = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let reverted = revert_lcd_config(&current);
        if reverted == current {
            // Already clean — nothing to do (idempotent).
            return;
        }
        // Snapshot before writing.
        let bak = format!("{cfg_path}.ados-bak");
        if let Err(e) = std::fs::write(&bak, &current) {
            tracing::warn!(error = %e, "could not snapshot boot config before LCD revert; skipping");
            return;
        }
        if let Err(e) = std::fs::write(path, &reverted) {
            tracing::warn!(error = %e, "writing reverted boot config failed");
        } else {
            tracing::info!(cfg = cfg_path, "reverted SPI-LCD boot config residue");
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── orphan default route fixtures ──

    #[test]
    fn orphan_scope_link_route_is_detected() {
        let fixture = "default dev eth0 scope link\n\
                       10.0.0.0/24 dev eth0 proto kernel scope link src 10.0.0.5\n";
        let args = orphan_default_route(fixture).expect("orphan route must be detected");
        assert_eq!(
            args,
            vec!["route", "del", "default", "dev", "eth0", "scope", "link"]
        );
    }

    #[test]
    fn legit_default_via_gateway_is_left_alone() {
        let fixture = "default via 192.168.1.1 dev wlan0 proto dhcp metric 600\n";
        assert!(orphan_default_route(fixture).is_none());
    }

    #[test]
    fn no_default_route_returns_none() {
        let fixture = "10.0.0.0/24 dev eth0 proto kernel scope link src 10.0.0.5\n";
        assert!(orphan_default_route(fixture).is_none());
    }

    #[test]
    fn gatewayless_default_without_scope_link_is_left_alone() {
        // A gateway-less default that is NOT the scope-link orphan shape: don't
        // touch it (out of scope for this cleanup).
        let fixture = "default dev tun0 metric 50\n";
        assert!(orphan_default_route(fixture).is_none());
    }

    // ── LCD config revert fixtures ──

    #[test]
    fn lcd_revert_drops_waveshare_and_restores_kms() {
        let fixture = "dtparam=audio=on\n\
                       # dtoverlay=vc4-kms-v3d  # disabled by ADOS LCD installer (claims fb0)\n\
                       dtparam=spi=on\n\
                       dtoverlay=waveshare35a\n\
                       max_framebuffers=2\n";
        let out = revert_lcd_config(fixture);
        // waveshare overlay gone.
        assert!(!out.contains("waveshare35a"), "waveshare line must be removed:\n{out}");
        // KMS overlay restored, active, trailer stripped.
        assert!(
            out.contains("dtoverlay=vc4-kms-v3d"),
            "KMS overlay must be restored:\n{out}"
        );
        assert!(
            !out.contains("disabled by ADOS"),
            "the ADOS trailer must be stripped:\n{out}"
        );
        // The restored KMS line is not commented.
        assert!(
            out.lines().any(|l| l.trim() == "dtoverlay=vc4-kms-v3d"),
            "restored KMS overlay must be an active line:\n{out}"
        );
        // Untouched lines survive.
        assert!(out.contains("dtparam=audio=on"));
        assert!(out.contains("max_framebuffers=2"));
    }

    #[test]
    fn lcd_revert_preserves_kms_overlay_args() {
        let fixture =
            "# dtoverlay=vc4-kms-v3d,cma-256  # disabled by ADOS LCD installer (claims fb0)\n";
        let out = revert_lcd_config(fixture);
        assert!(
            out.lines().any(|l| l.trim() == "dtoverlay=vc4-kms-v3d,cma-256"),
            "overlay args must be preserved:\n{out}"
        );
    }

    #[test]
    fn lcd_revert_is_idempotent_on_clean_config() {
        let clean = "dtparam=audio=on\ndtoverlay=vc4-kms-v3d\nmax_framebuffers=2\n";
        let out = revert_lcd_config(clean);
        assert_eq!(out, clean, "a clean config must be returned unchanged");
        // Running again is still a no-op.
        assert_eq!(revert_lcd_config(&out), clean);
    }

    #[test]
    fn lcd_revert_drops_commented_waveshare_line_too() {
        let fixture = "# dtoverlay=waveshare35a\ndtparam=spi=on\n";
        let out = revert_lcd_config(fixture);
        assert!(!out.contains("waveshare35a"));
        assert!(out.contains("dtparam=spi=on"));
    }
}
