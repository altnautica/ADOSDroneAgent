//! Network stack: hand the management Wi-Fi to NetworkManager on hosts that
//! run another stack, so the Wi-Fi join, the dashboard and the terminal UI can
//! all drive it with `nmcli`.
//!
//! The work lives in [`crate::net_stack`]; this step decides when it applies.
//! The wired link is never reconfigured live: on a netplan host the renderer
//! switch is staged for the installer's closing reboot.
//!
//! Optional: a host that cannot adopt NetworkManager keeps its wired link and
//! its current stack; only the Wi-Fi setup is unavailable.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// The profiles that run a management Wi-Fi the agent drives.
const PROFILES: &[&str] = &["drone", "ground_station", "visor"];

/// Adopt NetworkManager for the management Wi-Fi.
pub struct NetworkStack;

impl Step for NetworkStack {
    fn id(&self) -> &str {
        "network_stack"
    }
    fn requires(&self) -> &[&str] {
        // After deps (apt is ready) and after the WFB rule, so the long-range
        // radios are unmanaged before NetworkManager ever starts.
        &["deps", "wfb_unmanaged"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: the probe makes a re-run a no-op on an adopted host,
        // and re-signals a reboot that is still owed.
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        if let Some(skip) = crate::host::personal_skip(ctx, "network_stack") {
            return skip;
        }
        if !PROFILES.contains(&ctx.profile.as_str()) {
            return StepOutcome::Skipped;
        }
        // A host online over Wi-Fi keeps its stack: releasing the radio would
        // cut the link the install (or the operator) is using.
        if crate::net_stack::wifi_uplink_in_use()
            && !crate::net_stack::plan(&crate::net_stack::probe()).noop()
        {
            tracing::warn!("network stack left as it is: this device is online over Wi-Fi");
            return StepOutcome::Skipped;
        }
        match crate::net_stack::ensure_networkmanager(&ctx.progress) {
            Ok(a) => {
                tracing::info!(
                    stack = a.stack,
                    reboot_reason = a.reboot_reason.as_deref().unwrap_or("none"),
                    "network stack ready for NetworkManager Wi-Fi"
                );
                StepOutcome::Ok
            }
            Err(e) => StepOutcome::Failed(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::Checkpoint;

    #[test]
    fn step_metadata_is_optional_after_deps_and_the_wfb_rule() {
        assert_eq!(NetworkStack.id(), "network_stack");
        assert_eq!(NetworkStack.requires(), &["deps", "wfb_unmanaged"]);
        assert!(matches!(NetworkStack.kind(), StepKind::Optional));
        assert!(NetworkStack.checkpoint().is_none());
    }

    #[test]
    fn a_profile_without_a_managed_wifi_skips() {
        let mut ctx = Ctx::for_test(Checkpoint::new());
        ctx.profile = "workstation".into();
        assert_eq!(NetworkStack.run(&mut ctx), StepOutcome::Skipped);
    }
}
