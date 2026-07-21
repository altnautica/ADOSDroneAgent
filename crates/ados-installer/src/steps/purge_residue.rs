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

/// Rewrite a `/boot/firmware/config.txt` body to PROVISION the SPI-LCD — the
/// inverse of [`revert_lcd_config`], for a ground-station rig whose UI is the
/// panel. Comments out an ACTIVE `dtoverlay=vc4-kms-v3d[,args]` (so the SPI
/// panel claims fb0) and appends `dtoverlay=waveshare35a` when no active one is
/// present. Pure + idempotent: a config that already carries the waveshare
/// overlay and a disabled KMS overlay is returned unchanged.
pub fn provision_lcd_config(cfg: &str) -> String {
    let had_trailing_newline = cfg.ends_with('\n');
    let mut out: Vec<String> = Vec::new();
    let mut has_waveshare = false;

    for raw in cfg.lines() {
        let trimmed = raw.trim_start();
        let is_comment = trimmed.starts_with('#');

        // An already-active waveshare overlay means the LCD is provisioned.
        if !is_comment && trimmed.starts_with("dtoverlay=waveshare35a") {
            has_waveshare = true;
            out.push(raw.to_string());
            continue;
        }

        // Comment out an ACTIVE KMS overlay so the SPI panel can own fb0.
        if !is_comment && is_kms_overlay(trimmed) {
            let indent: String = raw.chars().take_while(|c| c.is_whitespace()).collect();
            out.push(format!(
                "{indent}# {trimmed}  # disabled by ADOS LCD installer (claims fb0)"
            ));
            continue;
        }

        out.push(raw.to_string());
    }

    if !has_waveshare {
        out.push("dtoverlay=waveshare35a".to_string());
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
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        purge_orphan_route();
        // The SPI-LCD framebuffer overlay (`dtoverlay=waveshare35a`, which
        // comments out the KMS GPU overlay so the panel claims fb0) is
        // provisioned ONLY when the operator actually selected an SPI-LCD
        // panel. Keying off the profile alone force-added it to EVERY
        // ground-station box — including an HDMI-cockpit GS (`--display none`),
        // which killed the HDMI output (no `vc4-kms-v3d` -> no `/dev/dri` -> no
        // kiosk). Respect the resolved `--display` choice instead.
        match lcd_provision_action(ctx) {
            LcdAction::Provision => provision_lcd_boot_config(),
            LcdAction::Revert => revert_lcd_boot_config(),
            // Auto: leave the boot config to the brick-safe
            // install-display-overlay.sh auto-detector (bound-SPI keep / HDMI
            // clean / probation) so we neither strip a real panel nor break HDMI.
            LcdAction::Defer => {}
        }
        StepOutcome::Ok
    }
}

/// What `PurgeResidue` should do with the Pi SPI-LCD framebuffer overlay.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum LcdAction {
    /// Add `dtoverlay=waveshare35a` + disable the KMS overlay (panel is the UI).
    Provision,
    /// Strip any stale SPI-LCD overlay + re-enable KMS so HDMI boots clean.
    Revert,
    /// Change nothing — the brick-safe shell auto-detector owns the boot config.
    Defer,
}

/// Decide the SPI-LCD boot-config action from the operator's `--display`
/// choice, mirroring `config_identity::provision_overlays`' resolution: an
/// explicit `--display` wins; otherwise a ground station defaults to `auto`
/// (panel auto-detected) and every other profile to `none`. Pure + unit-tested.
///
///   * `none`  -> Revert  (explicit opt-out, or a drone: ensure a clean HDMI /
///     GPU boot by stripping any stale SPI-LCD overlay).
///   * `auto`  -> Defer   (the GS default: the brick-safe shell auto-detector
///     keeps a bound panel or resolves HDMI/OLED — never force a change here,
///     so a real SPI-LCD GS is never stripped to a white screen).
///   * an explicit panel id -> Provision on a ground station (the panel is its
///     UI), Revert on any other profile (a drone must never let the SPI
///     framebuffer overlay steal fb0 from the GPU).
fn lcd_provision_action(ctx: &Ctx) -> LcdAction {
    let resolved = ctx.args.display.clone().unwrap_or_else(|| {
        if ctx.profile == "ground_station" {
            "auto".to_string()
        } else {
            "none".to_string()
        }
    });
    match resolved.as_str() {
        "none" => LcdAction::Revert,
        "auto" => LcdAction::Defer,
        _ if ctx.profile == "ground_station" => LcdAction::Provision,
        _ => LcdAction::Revert,
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

/// Provision the SPI-LCD in the Pi boot config (the ground-station inverse of
/// [`revert_lcd_boot_config`]), snapshotting to `<cfg>.ados-bak` before writing.
/// No-op when the config is absent or already provisioned (idempotent). Takes
/// effect on the next reboot.
fn provision_lcd_boot_config() {
    for cfg_path in ["/boot/firmware/config.txt", "/boot/config.txt"] {
        let path = Path::new(cfg_path);
        let current = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let provisioned = provision_lcd_config(&current);
        if provisioned == current {
            // Already provisioned — nothing to do (idempotent).
            return;
        }
        let bak = format!("{cfg_path}.ados-bak");
        if let Err(e) = std::fs::write(&bak, &current) {
            tracing::warn!(error = %e, "could not snapshot boot config before LCD provision; skipping");
            return;
        }
        if let Err(e) = std::fs::write(path, &provisioned) {
            tracing::warn!(error = %e, "writing provisioned boot config failed");
        } else {
            tracing::info!(
                cfg = cfg_path,
                "provisioned SPI-LCD boot config (reboot to apply)"
            );
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::Checkpoint;

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
        assert!(
            !out.contains("waveshare35a"),
            "waveshare line must be removed:\n{out}"
        );
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
            out.lines()
                .any(|l| l.trim() == "dtoverlay=vc4-kms-v3d,cma-256"),
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

    #[test]
    fn lcd_provision_comments_kms_and_adds_waveshare() {
        // The exact broken ground-station state: active KMS overlay, no panel
        // overlay → no framebuffer → white LCD.
        let fixture = "dtparam=spi=on\ndtoverlay=vc4-kms-v3d\nmax_framebuffers=2\n";
        let out = provision_lcd_config(fixture);
        // KMS overlay commented out so the SPI panel can own fb0.
        assert!(
            out.lines()
                .any(|l| l.trim_start().starts_with("# dtoverlay=vc4-kms-v3d")),
            "KMS overlay must be commented out:\n{out}"
        );
        assert!(
            !out.lines().any(|l| l.trim() == "dtoverlay=vc4-kms-v3d"),
            "no active KMS overlay may remain:\n{out}"
        );
        // The SPI panel overlay is added.
        assert!(
            out.lines().any(|l| l.trim() == "dtoverlay=waveshare35a"),
            "waveshare overlay must be provisioned:\n{out}"
        );
        // Untouched lines survive.
        assert!(out.contains("dtparam=spi=on") && out.contains("max_framebuffers=2"));
    }

    #[test]
    fn lcd_provision_is_idempotent() {
        let provisioned = provision_lcd_config("dtparam=spi=on\ndtoverlay=vc4-kms-v3d\n");
        assert_eq!(
            provision_lcd_config(&provisioned),
            provisioned,
            "re-provisioning an already-provisioned config must be a no-op"
        );
        // Does not add a second waveshare line.
        assert_eq!(
            provisioned.matches("dtoverlay=waveshare35a").count(),
            1,
            "exactly one waveshare overlay line:\n{provisioned}"
        );
    }

    #[test]
    fn lcd_provision_then_revert_round_trips_to_clean() {
        let clean = "dtparam=spi=on\ndtoverlay=vc4-kms-v3d\nmax_framebuffers=2\n";
        let provisioned = provision_lcd_config(clean);
        assert_eq!(
            revert_lcd_config(&provisioned),
            clean,
            "revert(provision(clean)) must return the original clean config"
        );
    }

    #[test]
    fn lcd_revert_reenables_kms_disabled_by_the_lcd_installer() {
        // The exact shape a Waveshare SPI-LCD install leaves on a Pi: KMS
        // commented out + the panel overlay appended. Revert must restore HDMI.
        let broken = "dtparam=audio=on\n\
                      # dtoverlay=vc4-kms-v3d  # disabled by ADOS LCD installer (claims fb0)\n\
                      max_framebuffers=2\n\
                      dtoverlay=waveshare35a\n";
        let out = revert_lcd_config(broken);
        assert!(
            out.contains("\ndtoverlay=vc4-kms-v3d\n") || out.starts_with("dtoverlay=vc4-kms-v3d\n"),
            "KMS overlay must be re-enabled (active): {out:?}"
        );
        assert!(
            !out.contains("dtoverlay=waveshare35a"),
            "the SPI-LCD overlay must be stripped: {out:?}"
        );
    }

    // ── lcd_provision_action: the display-choice-aware decision ──

    fn ctx_with(display: Option<&str>, profile: &str) -> Ctx {
        let mut ctx = Ctx::for_test(Checkpoint::new());
        ctx.args.display = display.map(str::to_string);
        ctx.profile = profile.to_string();
        ctx
    }

    #[test]
    fn action_gs_display_none_reverts_so_hdmi_boots() {
        // The regression fix: a ground station installed with --display none
        // (HDMI cockpit) must NOT get the SPI-LCD overlay force-added.
        assert_eq!(
            lcd_provision_action(&ctx_with(Some("none"), "ground_station")),
            LcdAction::Revert
        );
    }

    #[test]
    fn action_gs_display_auto_defers_to_shell_autodetect() {
        // The GS default: never force a boot-config change; the brick-safe
        // shell auto-detector keeps a bound panel or resolves HDMI.
        assert_eq!(
            lcd_provision_action(&ctx_with(Some("auto"), "ground_station")),
            LcdAction::Defer
        );
        // Absent --display on a GS resolves to "auto" -> Defer (never strips a
        // real SPI-LCD GS to a white screen).
        assert_eq!(
            lcd_provision_action(&ctx_with(None, "ground_station")),
            LcdAction::Defer
        );
    }

    #[test]
    fn action_gs_explicit_panel_provisions() {
        assert_eq!(
            lcd_provision_action(&ctx_with(Some("waveshare35a"), "ground_station")),
            LcdAction::Provision
        );
    }

    #[test]
    fn action_drone_reverts_to_keep_the_gpu() {
        // A drone (absent --display resolves to "none") must never carry the SPI
        // framebuffer overlay stealing fb0 from the GPU.
        assert_eq!(
            lcd_provision_action(&ctx_with(None, "drone")),
            LcdAction::Revert
        );
        assert_eq!(
            lcd_provision_action(&ctx_with(Some("none"), "drone")),
            LcdAction::Revert
        );
        // Even an explicit panel id on a drone reverts (a drone is not a panel host).
        assert_eq!(
            lcd_provision_action(&ctx_with(Some("waveshare35a"), "drone")),
            LcdAction::Revert
        );
    }
}
