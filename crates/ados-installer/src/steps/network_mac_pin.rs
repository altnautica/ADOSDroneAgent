//! Stable-MAC pinning at install time.
//!
//! Some onboard USB WiFi chipsets have no efuse MAC, so their driver generates a
//! fresh random address every boot and the DHCP lease (and the box's IP) churns.
//! This step detects such an adapter from the chipset quirk table and writes a
//! next-boot `systemd-networkd` `.link` pinning a deterministic, locally-
//! administered MAC derived from the machine-id.
//!
//! Optional by design: a pin failure degrades but never aborts the install (the
//! box still comes up). It only ever writes a `.link` (effective on the next
//! boot) and never touches the live interface, so it cannot drop the operator's
//! management link. The cross-boot learner for unknown chipsets is the always-on
//! supervisor's job, not the installer's — so first-boot behavior stays
//! deterministic (quirk-table adapters only).

use std::collections::HashMap;

use serde::Deserialize;

use ados_macpin::engine::{self, ReconcileConfig};
use ados_macpin::AdapterState;

use crate::ctx::Ctx;
use crate::env::CONFIG_YAML;
use crate::graph::{Step, StepKind, StepOutcome};

/// The slice of `config.yaml` this step reads. Everything is optional so a
/// config without a `network.mac_pin` block resolves to the defaults
/// (enabled, live-retag forbidden).
#[derive(Debug, Deserialize, Default)]
struct RootConfigView {
    network: Option<NetworkView>,
}
#[derive(Debug, Deserialize, Default)]
struct NetworkView {
    mac_pin: Option<MacPinView>,
}
#[derive(Debug, Deserialize, Default)]
struct MacPinView {
    enabled: Option<bool>,
    apply_live_allowed: Option<bool>,
    overrides: Option<HashMap<String, String>>,
}

/// Read `network.mac_pin` from `/etc/ados/config.yaml`. Defaults: pinning ON
/// (it is non-destructive — file-only, next-boot), live re-tag OFF.
fn read_config() -> ReconcileConfig {
    let view: RootConfigView = std::fs::read_to_string(CONFIG_YAML)
        .ok()
        .and_then(|s| serde_norway::from_str(&s).ok())
        .unwrap_or_default();
    let mp = view.network.and_then(|n| n.mac_pin).unwrap_or_default();
    ReconcileConfig {
        enabled: mp.enabled.unwrap_or(true),
        apply_live_allowed: mp.apply_live_allowed.unwrap_or(false),
        overrides: mp.overrides.unwrap_or_default(),
    }
}

/// Stable-MAC pin provisioning.
pub struct NetworkMacPin;

impl Step for NetworkMacPin {
    fn id(&self) -> &str {
        "network_mac_pin"
    }
    fn requires(&self) -> &[&str] {
        // After config_identity so /etc/ados exists (for the state file) and
        // config.yaml has been written (for the enabled flag).
        &["config_identity"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: the reconcile is idempotent, so re-running on every
        // upgrade re-affirms the pin (and re-pins a hot-plugged adapter).
        None
    }
    fn kind(&self) -> StepKind {
        // Optional: a pin problem must degrade, never abort the install.
        StepKind::Optional
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        let config = read_config();
        if !config.enabled {
            tracing::info!("stable-MAC pinning disabled by config; skipping");
            return StepOutcome::Ok;
        }
        // Quirk-table auto-pin only at install time (with_learner = false).
        let state = engine::reconcile(&config, false);
        let pinned = state
            .adapters
            .iter()
            .filter(|a| matches!(a.state, AdapterState::Pinned))
            .count();
        let deferred = state
            .adapters
            .iter()
            .filter(|a| matches!(a.state, AdapterState::Deferred))
            .count();
        tracing::info!(
            adapters = state.adapters.len(),
            pinned,
            deferred,
            "stable-MAC pin reconcile complete"
        );
        StepOutcome::Ok
    }
}
