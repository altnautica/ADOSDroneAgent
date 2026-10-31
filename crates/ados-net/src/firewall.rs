//! share_uplink firewall + sysctl persistence and the data-cap shaping.
//!
//! One serialized reconcile owns every NAT, sysctl and tc write the network
//! daemon makes. It is level-triggered over a single [`FirewallIntent`]
//! (`share_uplink` flag, active uplink iface, cellular iface, data-cap state),
//! so the share-uplink toggle, an uplink switch and a cap transition can never
//! undo one another:
//!
//! - NAT MASQUERADE lives in a dedicated chain (`ADOS_NAT` for iptables, table
//!   `ados_nat` for nftables) that holds at most one rule, for the active uplink,
//!   and only while `share_uplink` is on and the cap has not blocked that iface.
//!   The chain is rebuilt whenever its contents differ from the intent, so a
//!   previous uplink's rule never survives a failover or a disable.
//! - The cap shapes the cellular iface whichever uplink is active: a tbf at
//!   [`THROTTLE_RATE_KBPS_95`] at 95 percent and at [`BLOCKED_RATE_KBPS`] at 100
//!   percent, so the node's own traffic is held to a control-plane trickle too.
//! - Persistence writes only what ADOS owns: the sysctl drop-in, the
//!   iptables-persistent save, or an nftables include holding the `ados_nat`
//!   table alone.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{info, warn};

use crate::cmd::CmdRunner;
use crate::router::events::DataCapState;
use crate::sidecar;

mod backend;
mod nat;
mod shaping;
#[cfg(test)]
use shaping::parse_root_tbf;

const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Persisted sysctl drop-in.
pub const SYSCTL_DROPIN_PATH: &str = "/etc/sysctl.d/99-ados-share-uplink.conf";
/// iptables-persistent rules file.
pub const IPTABLES_RULES_V4_PATH: &str = "/etc/iptables/rules.v4";
/// nftables include holding only the `ados_nat` table.
pub const NFT_INCLUDE_PATH: &str = "/etc/nftables.d/ados-nat.nft";

const NFT_TABLE: &str = "ados_nat";
const NFT_CHAIN: &str = "postrouting";
/// iptables nat-table chain that holds the share-uplink MASQUERADE rule.
const IPT_CHAIN: &str = "ADOS_NAT";

/// The default throttle rate at 95 percent of cap.
pub const THROTTLE_RATE_KBPS_95: u32 = 256;
/// The rate the cellular iface is held to at 100 percent of cap: enough for the
/// node's heartbeat and command traffic, nothing for bulk transfer.
pub const BLOCKED_RATE_KBPS: u32 = 32;

/// The inputs every firewall and shaping decision is derived from.
#[derive(Debug, Clone, PartialEq)]
pub struct FirewallIntent {
    /// Operator's `share_uplink` setting.
    pub share_uplink: bool,
    /// Kernel iface of the router's active uplink.
    pub active_iface: Option<String>,
    /// Kernel iface of the metered cellular link, when a modem is present.
    pub cellular_iface: Option<String>,
    /// Current data-cap level.
    pub cap_state: DataCapState,
}

impl Default for FirewallIntent {
    fn default() -> Self {
        Self {
            share_uplink: false,
            active_iface: None,
            cellular_iface: None,
            cap_state: DataCapState::Ok,
        }
    }
}

impl FirewallIntent {
    /// The iface that should carry the MASQUERADE rule, if any.
    pub fn nat_iface(&self) -> Option<&str> {
        if !self.share_uplink {
            return None;
        }
        let active = self.active_iface.as_deref().filter(|s| !s.is_empty())?;
        if self.cap_state == DataCapState::Blocked100
            && self.cellular_iface.as_deref() == Some(active)
        {
            return None;
        }
        Some(active)
    }

    /// The tbf rate the cellular iface should be held to, if any.
    pub fn shaping_rate_kbps(&self) -> Option<u32> {
        match self.cap_state {
            DataCapState::Ok | DataCapState::Warn80 => None,
            DataCapState::Throttle95 => Some(THROTTLE_RATE_KBPS_95),
            DataCapState::Blocked100 => Some(BLOCKED_RATE_KBPS),
        }
    }
}

/// Persistence backend. `IptablesRuntime` means iptables works but there is no
/// `/etc/iptables` dir, so rules apply now but do not survive reboot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirewallBackend {
    IptablesPersistent,
    Nftables,
    IptablesRuntime,
    None,
}

/// Resolves which firewall backend is available. Abstracted so tests can force
/// a backend without a real iptables / nft on PATH.
pub trait BackendDetector: Send + Sync {
    fn detect(&self) -> FirewallBackend;
}

/// Production detector: probes PATH for `iptables` / `nft` and checks for the
/// `/etc/iptables` dir that iptables-persistent owns.
#[derive(Debug, Default, Clone, Copy)]
pub struct PathBackendDetector;

/// What the last reconcile left in place, held under the apply lock.
#[derive(Debug, Default)]
struct Applied {
    /// The intent the last reconcile ran against.
    intent: FirewallIntent,
    /// The iface the last reconcile shaped, so a modem iface change clears it.
    shaped_iface: Option<String>,
    /// The `ip_forward` value last written at runtime; `None` before the first.
    forwarding: Option<bool>,
}

/// The firewall controller. Holds the command runner, the backend detector,
/// and the paths (overridable for tests).
pub struct ShareUplinkFirewall {
    runner: Arc<dyn CmdRunner>,
    detector: Arc<dyn BackendDetector>,
    sysctl_dropin: PathBuf,
    iptables_rules_v4: PathBuf,
    nft_include: PathBuf,
    /// Serializes every reconcile: the event consumer and the operator's
    /// command-socket toggle both reconcile, and two interleaved rebuilds could
    /// leave the rule set half of each.
    applied: tokio::sync::Mutex<Applied>,
}

impl ShareUplinkFirewall {
    /// Controller with the production detector and canonical paths.
    pub fn new(runner: Arc<dyn CmdRunner>) -> Self {
        Self::with_parts(
            runner,
            Arc::new(PathBackendDetector),
            PathBuf::from(SYSCTL_DROPIN_PATH),
            PathBuf::from(IPTABLES_RULES_V4_PATH),
            PathBuf::from(NFT_INCLUDE_PATH),
        )
    }

    /// Full constructor (tests).
    pub fn with_parts(
        runner: Arc<dyn CmdRunner>,
        detector: Arc<dyn BackendDetector>,
        sysctl_dropin: PathBuf,
        iptables_rules_v4: PathBuf,
        nft_include: PathBuf,
    ) -> Self {
        Self {
            runner,
            detector,
            sysctl_dropin,
            iptables_rules_v4,
            nft_include,
            applied: tokio::sync::Mutex::new(Applied::default()),
        }
    }

    pub fn backend(&self) -> FirewallBackend {
        self.detector.detect()
    }

    // ---------------- sysctl ----------------

    // ---------------- iptables ----------------

    // ---------------- nftables ----------------

    // ---------------- tc shaping ----------------

    // ---------------- public reconcile ----------------

    /// Bring sysctl, NAT and shaping into agreement with `intent`. Serialized,
    /// idempotent and best-effort: never panics, and rewrites persisted state
    /// only when the runtime rules changed. Returns
    /// `{applied, backend, apply_error, nat_iface, shaped_iface, cap_state}`.
    pub async fn reconcile(&self, intent: FirewallIntent) -> Value {
        let mut applied = self.applied.lock().await;
        self.reconcile_locked(&mut applied, intent).await
    }

    /// Apply an operator `share_uplink` toggle against the current uplink,
    /// keeping the cap state and cellular iface of the last reconcile.
    pub async fn set_share_uplink(&self, enabled: bool, active_iface: Option<&str>) -> Value {
        let mut applied = self.applied.lock().await;
        let intent = FirewallIntent {
            share_uplink: enabled,
            active_iface: active_iface.map(str::to_string),
            ..applied.intent.clone()
        };
        self.reconcile_locked(&mut applied, intent).await
    }

    async fn reconcile_locked(&self, applied: &mut Applied, intent: FirewallIntent) -> Value {
        let backend = self.backend();
        let mut apply_error: Option<String> = None;
        let mut note = |err: Option<String>| {
            if apply_error.is_none() {
                apply_error = err;
            }
        };

        // The runtime sysctl is written only when the flag changes (or on the
        // first pass), so the periodic pass never fights another owner of
        // ip_forward between operator toggles.
        if applied.forwarding != Some(intent.share_uplink) {
            let err = self.apply_sysctl_runtime(intent.share_uplink).await;
            if err.is_none() {
                applied.forwarding = Some(intent.share_uplink);
            }
            note(err);
        }
        note(
            self.sync_sysctl_dropin(intent.share_uplink)
                .err()
                .map(|e| format!("sysctl_dropin_failed: {e}")),
        );

        let nat_iface = intent.nat_iface();
        match backend {
            FirewallBackend::IptablesPersistent | FirewallBackend::IptablesRuntime => {
                let (changed, err) = self.iptables_sync(nat_iface).await;
                note(err);
                if changed {
                    if backend == FirewallBackend::IptablesPersistent {
                        note(self.iptables_save().await);
                    } else {
                        warn!("share_uplink.iptables_no_persistence");
                    }
                }
            }
            FirewallBackend::Nftables => {
                let (changed, err) = self.nft_sync(nat_iface).await;
                note(err);
                if changed {
                    note(self.nft_save().await);
                }
            }
            FirewallBackend::None => {
                if intent.share_uplink {
                    warn!("share_uplink.no_backend");
                    note(Some(
                        "no_firewall_backend (neither iptables nor nftables found)".to_string(),
                    ));
                }
            }
        }

        note(self.sync_shaping(applied, &intent).await);

        if applied.intent != intent {
            info!(
                share_uplink = intent.share_uplink,
                active = ?intent.active_iface,
                cellular = ?intent.cellular_iface,
                cap_state = ?intent.cap_state,
                nat = ?nat_iface,
                "share_uplink.reconciled"
            );
        }
        let result = json!({
            "applied": apply_error.is_none(),
            "backend": backend.as_str(),
            "apply_error": apply_error,
            "nat_iface": nat_iface,
            "shaped_iface": applied.shaped_iface,
            "cap_state": intent.cap_state,
        });
        applied.intent = intent;
        result
    }
}

fn non_empty(stderr: &str, fallback: &str) -> String {
    let t = stderr.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

/// A stateful stand-in for `iptables`, `nft` and `tc`, so a test asserts the
/// rules a sequence of reconciles leaves behind rather than a call script.
#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
