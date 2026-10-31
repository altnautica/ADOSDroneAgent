//! Config + identity: mint the device id, write `/etc/ados/profile.conf` +
//! `/etc/ados/config.yaml`, set the hostname (`--name`), persist the pairing
//! material (`--pair`), and delegate the display + camera overlay provisioning
//! to the battle-tested driver scripts. Required — the agent will not resolve
//! its profile or identity without it.
//!
//! Ports the pure file-write logic of `scripts/install.d/09-config.sh`
//! (`generate_device_id`, `generate_default_config`, `set_hostname`,
//! `write_pairing`) and delegates the two boot-config-editing overlay steps
//! (`install-display-overlay.sh` / `install-camera-overlay.sh`) via `exec` —
//! those carry brick-safe snapshot/restore logic that must not be re-ported.
//!
//! The slugify + the config-YAML builder are pure so a unit test exercises them
//! without touching the filesystem.

use std::path::Path;

use crate::ctx::Ctx;
use crate::env;
use crate::graph::{Step, StepKind, StepOutcome};

mod provision;
mod render;

use provision::{
    ensure_device_id, provision_overlays, set_hostname, write_cutover_note, write_default_config,
    write_pairing, write_profile_conf,
};
pub(crate) use render::default_display_arg;
pub use render::{
    cutover_note_body, default_config_yaml, mint_pending_api_key, pairing_json, profile_conf_body,
    regulatory_block, rewrite_hosts_body, slugify_hostname,
};

mod peripherals;

mod plugin_keys;

/// Operator config + agent identity provisioning.
pub struct ConfigIdentity;

impl Step for ConfigIdentity {
    fn id(&self) -> &str {
        "config_identity"
    }
    fn requires(&self) -> &[&str] {
        &["venv_agent"]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        // 1. Device id (required: identity is the install's keystone).
        let device_id = match ensure_device_id() {
            Ok(id) => id,
            Err(e) => return StepOutcome::Failed(format!("could not mint device id: {e}")),
        };

        // 2. Persist the resolved profile + the default config. The server mode
        //    and operating region come from the operator's onboarding choices
        //    (local-first + unrestricted by default).
        write_profile_conf(&ctx.profile, &ctx.channel, ctx.args.version.as_deref());
        let server_mode = if ctx.cloud_from_anywhere {
            "cloud"
        } else {
            "local"
        };
        write_default_config(
            &device_id,
            ctx.args.name.as_deref(),
            &ctx.profile,
            server_mode,
            ctx.region_pinned.as_deref(),
        );

        // 3. Hostname (only when --name given).
        if let Some(name) = ctx.args.name.clone() {
            set_hostname(&name);
        }

        // 4. Pairing material (only when --pair given).
        if let Some(code) = ctx.args.pair.clone() {
            write_pairing(&code);
        }

        // 5. Delegate the brick-safe overlay provisioning to the driver scripts.
        let source = env::resolve_source_dir(ctx.source_dir.as_deref());
        provision_overlays(ctx, source.as_deref());

        // 6. Provision the first-party plugin trust keys (so signed plugins can
        //    be verified) and seed the default peripheral manifests (so the
        //    Peripherals page renders on a fresh board). Both copy from the
        //    cloned source tree; idempotent + operator-state-preserving.
        plugin_keys::provision_plugin_keys(source.as_deref());
        plugin_keys::provision_named_plugin_keys(&ctx.args.plugin_key);
        peripherals::seed_default_peripherals(source.as_deref());

        // 7. Deploy the operator note describing how to revert the radio
        //    services to the packaged Python fallback.
        write_cutover_note();

        StepOutcome::Ok
    }
}

/// Seconds since the Unix epoch.
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// chmod (Unix); a no-op on a non-Unix dev host.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests;
