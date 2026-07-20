//! Non-interactive Wi-Fi join for the silent, flag-driven install.
//!
//! When `--wifi-ssid` is passed, join that network on the management Wi-Fi
//! interface so the operator can later unplug the wired cable -- WITHOUT
//! running the interactive onboarding wizard. This is the headless equivalent
//! of the wizard's Wi-Fi stage: it reuses the exact same nmcli
//! join/verify/persist logic (`crate::wizard::wifi`) and the same two safety
//! rules -- it refuses if the operator's session rides Wi-Fi (never sever the
//! link the install is running over) and never touches the long-range radio
//! adapter (the join is pinned to a resolved management-Wi-Fi interface).
//!
//! Optional by design: a Wi-Fi problem degrades but never aborts the install --
//! the box still comes up on its wired link. A connected-but-unreachable link
//! is torn down rather than persisted, so a dead network is never saved for the
//! next boot.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::wizard::wifi;

/// Headless Wi-Fi join (driven by `--wifi-ssid` / `--wifi-pass`).
pub struct WifiJoin;

impl Step for WifiJoin {
    fn id(&self) -> &str {
        "wifi_join"
    }
    fn requires(&self) -> &[&str] {
        // After config_identity so the hostname is set before the box appears
        // on the new network.
        &["config_identity"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: re-running an upgrade re-affirms the join + persist
        // idempotently (NetworkManager owns the saved profile).
        None
    }
    fn kind(&self) -> StepKind {
        // Optional: Wi-Fi is a convenience uplink; the wired link still works.
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        let ssid = match ctx.args.wifi_ssid.clone() {
            Some(s) if !s.trim().is_empty() => s,
            // No --wifi-ssid: nothing to do (the wizard owns the interactive path).
            _ => return StepOutcome::Skipped,
        };

        // Never reconfigure the radio the operator's SSH session rides on --
        // that would drop the install mid-flight.
        if wifi::session_rides_wifi() {
            tracing::warn!(
                "skipping --wifi-ssid join: the session rides Wi-Fi (would sever the link)"
            );
            return StepOutcome::Skipped;
        }

        let iface = match wifi::management_wifi_iface() {
            Some(i) => i,
            None => {
                return StepOutcome::Failed(
                    "no management Wi-Fi interface found to join --wifi-ssid".into(),
                )
            }
        };

        let password = ctx.args.wifi_pass.as_deref().filter(|p| !p.is_empty());

        if let Err(e) = wifi::connect(&iface, &ssid, password, false) {
            return StepOutcome::Failed(format!("Wi-Fi join failed: {e}"));
        }

        // Verify the joined link actually reaches the LAN. A connected-but-dead
        // link must not be persisted -- it would auto-rejoin a dead network on
        // the next boot.
        let reach = wifi::verify_lan_reachable(&iface);
        if !reach.reachable {
            wifi::forget(&ssid);
            return StepOutcome::Failed(
                "Wi-Fi joined but the LAN gateway did not answer; not persisted".into(),
            );
        }

        // Persist: autoconnect on, a route metric higher than wired (so a
        // plugged cable stays primary), power-save off.
        wifi::persist(&ssid);
        tracing::info!(iface = %iface, ssid = %ssid, "headless Wi-Fi join complete");
        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::Checkpoint;

    #[test]
    fn skips_when_no_wifi_ssid_flag() {
        // With no --wifi-ssid, the step is a no-op (Skipped), never a failure.
        let mut ctx = Ctx::for_test(Checkpoint::new());
        assert_eq!(WifiJoin.run(&mut ctx), StepOutcome::Skipped);
    }

    #[test]
    fn skips_on_blank_wifi_ssid() {
        let mut ctx = Ctx::for_test(Checkpoint::new());
        ctx.args.wifi_ssid = Some("   ".to_string());
        assert_eq!(WifiJoin.run(&mut ctx), StepOutcome::Skipped);
    }

    #[test]
    fn step_metadata_is_optional_after_config() {
        assert_eq!(WifiJoin.id(), "wifi_join");
        assert_eq!(WifiJoin.requires(), &["config_identity"]);
        assert!(matches!(WifiJoin.kind(), StepKind::Optional));
        assert!(WifiJoin.checkpoint().is_none());
    }
}
