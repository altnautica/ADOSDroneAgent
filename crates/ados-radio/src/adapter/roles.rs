//! Role assignment for the verified WFB adapters on a node.
//!
//! [`resolve_roles`] maps every verified adapter to a role from the
//! `radio.adapters` config; an empty list assigns roles automatically.
//! [`AdapterPlan`] turns the resolved list into what the radio processes need:
//! the interfaces handed to `wfb_rx`, the one interface the transmit lanes use,
//! and the adapter left to the mesh carrier.

use crate::config::{AdapterRole, RadioAdaptersConfig};

use super::select::DetectedAdapter;

/// A verified adapter with its resolved role and antenna label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAdapter {
    pub adapter: DetectedAdapter,
    pub role: AdapterRole,
    pub antenna: Option<String>,
}

impl ResolvedAdapter {
    pub fn ifname(&self) -> &str {
        &self.adapter.ifname
    }
}

/// Assign a role to every adapter, in the order given.
///
/// With an empty `radio.adapters` list: the first adapter is `rxtx`, the rest
/// `rx`, except that with `mesh_enabled` the last of two or more adapters is
/// left to the mesh carrier (`mesh`). A single adapter is always `rxtx`.
///
/// With explicit entries: each adapter takes the role and antenna of the first
/// entry whose matcher names it (MAC case-insensitive, or USB path); an
/// adapter no entry names receives (`rx`).
pub fn resolve_roles(
    adapters: &[DetectedAdapter],
    cfg: &RadioAdaptersConfig,
    mesh_enabled: bool,
) -> Vec<ResolvedAdapter> {
    if cfg.adapters.is_empty() {
        let last = adapters.len().saturating_sub(1);
        return adapters
            .iter()
            .enumerate()
            .map(|(i, a)| ResolvedAdapter {
                adapter: a.clone(),
                role: match i {
                    0 => AdapterRole::Rxtx,
                    i if mesh_enabled && i == last => AdapterRole::Mesh,
                    _ => AdapterRole::Rx,
                },
                antenna: None,
            })
            .collect();
    }
    adapters
        .iter()
        .map(|a| {
            let entry = cfg
                .adapters
                .iter()
                .find(|e| e.matcher.matches(a.mac.as_deref(), a.usb_path.as_deref()));
            ResolvedAdapter {
                adapter: a.clone(),
                role: entry.map_or(AdapterRole::Rx, |e| e.role),
                antenna: entry.and_then(|e| e.antenna.clone()),
            }
        })
        .collect()
}

/// The adapter that carries the transmit lanes: the first `tx` adapter, else
/// the first `rxtx` one. `None` when neither role was assigned.
pub fn tx_adapter(resolved: &[ResolvedAdapter]) -> Option<&ResolvedAdapter> {
    resolved
        .iter()
        .find(|r| r.role == AdapterRole::Tx)
        .or_else(|| resolved.iter().find(|r| r.role == AdapterRole::Rxtx))
}

/// The radio layout one bring-up runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterPlan {
    adapters: Vec<ResolvedAdapter>,
    tx: usize,
    rx: Vec<usize>,
}

impl AdapterPlan {
    /// Build the plan from a resolved list, or `None` when no adapter can carry
    /// WFB (the list is empty or every adapter is `mesh`).
    ///
    /// When the config assigned no `tx`/`rxtx` adapter the first receiving
    /// adapter carries the transmit lanes too; when it assigned no receiving
    /// adapter the transmit adapter receives as well. Either way the link keeps
    /// both directions rather than going deaf or mute on a config gap.
    pub fn new(adapters: Vec<ResolvedAdapter>) -> Option<Self> {
        let rx: Vec<usize> = adapters
            .iter()
            .enumerate()
            .filter(|(_, r)| r.role.receives())
            .map(|(i, _)| i)
            .collect();
        let tx = match tx_adapter(&adapters) {
            Some(t) => adapters.iter().position(|r| r == t)?,
            None => {
                let first = *rx.first()?;
                tracing::warn!(
                    interface = %adapters[first].ifname(),
                    "wfb_adapter_no_tx_role: the first receiving adapter carries the transmit lanes"
                );
                first
            }
        };
        let rx = if rx.is_empty() {
            tracing::warn!(
                interface = %adapters[tx].ifname(),
                "wfb_adapter_no_rx_role: the transmit adapter receives as well"
            );
            vec![tx]
        } else {
            rx
        };
        Some(Self { adapters, tx, rx })
    }

    /// Every resolved adapter, in detection order.
    pub fn adapters(&self) -> &[ResolvedAdapter] {
        &self.adapters
    }

    /// The adapter carrying the transmit lanes.
    pub fn tx(&self) -> &ResolvedAdapter {
        &self.adapters[self.tx]
    }

    /// The interfaces handed to `wfb_rx`, in detection order. `wfb_rx` numbers
    /// them in this order in its per-antenna stats.
    pub fn rx_ifaces(&self) -> Vec<String> {
        self.rx
            .iter()
            .map(|&i| self.adapters[i].adapter.ifname.clone())
            .collect()
    }

    /// Every interface WFB runs on (the transmit adapter first, then each
    /// receiving one not already listed): the set that needs monitor mode, TX
    /// power and the channel at bring-up and on every retune.
    pub fn radio_ifaces(&self) -> Vec<String> {
        let mut out = vec![self.tx().adapter.ifname.clone()];
        for iface in self.rx_ifaces() {
            if !out.contains(&iface) {
                out.push(iface);
            }
        }
        out
    }

    /// The adapter left to the mesh carrier, if any.
    pub fn mesh(&self) -> Option<&ResolvedAdapter> {
        self.adapters.iter().find(|r| r.role == AdapterRole::Mesh)
    }

    /// True when `index` (detection order) is the transmit adapter.
    pub fn is_tx(&self, index: usize) -> bool {
        index == self.tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AdapterEntry, AdapterMatch};

    fn adapter(i: u8) -> DetectedAdapter {
        DetectedAdapter {
            ifname: format!("wlan{i}"),
            mac: Some(format!("aa:bb:cc:dd:ee:0{i}")),
            usb_path: Some(format!("1-1.{i}")),
            driver: "rtl88x2eu".into(),
            chipset: "RTL8812EU".into(),
            verified: true,
            usb_speed_mbps: Some(480),
            usb_degraded: false,
        }
    }

    fn roles(r: &[ResolvedAdapter]) -> Vec<AdapterRole> {
        r.iter().map(|a| a.role).collect()
    }

    fn four() -> Vec<DetectedAdapter> {
        (1..=4).map(adapter).collect()
    }

    use AdapterRole::{Mesh, Rx, Rxtx, Tx};

    #[test]
    fn four_auto_adapters_with_mesh_leave_the_last_to_the_mesh() {
        let r = resolve_roles(&four(), &RadioAdaptersConfig::default(), true);
        assert_eq!(roles(&r), vec![Rxtx, Rx, Rx, Mesh]);
        let plan = AdapterPlan::new(r).unwrap();
        assert_eq!(plan.mesh().unwrap().ifname(), "wlan4");
        assert_eq!(plan.rx_ifaces(), vec!["wlan1", "wlan2", "wlan3"]);
    }

    #[test]
    fn four_auto_adapters_without_mesh_all_receive() {
        let r = resolve_roles(&four(), &RadioAdaptersConfig::default(), false);
        assert_eq!(roles(&r), vec![Rxtx, Rx, Rx, Rx]);
        let plan = AdapterPlan::new(r).unwrap();
        assert!(plan.mesh().is_none());
        assert_eq!(plan.rx_ifaces(), vec!["wlan1", "wlan2", "wlan3", "wlan4"]);
        assert_eq!(plan.tx().ifname(), "wlan1");
    }

    #[test]
    fn explicit_entries_match_by_mac_or_usb_path_and_the_rest_receive() {
        let cfg = RadioAdaptersConfig {
            adapters: vec![
                AdapterEntry {
                    // Case differs from the detected MAC on purpose.
                    matcher: AdapterMatch {
                        mac: Some("AA:BB:CC:DD:EE:03".into()),
                        usb_path: None,
                    },
                    role: Rxtx,
                    antenna: Some("omni-5.8".into()),
                },
                AdapterEntry {
                    matcher: AdapterMatch {
                        mac: None,
                        usb_path: Some("1-1.1".into()),
                    },
                    role: Mesh,
                    antenna: Some("dipole-2.4".into()),
                },
            ],
        };
        let r = resolve_roles(&four(), &cfg, false);
        assert_eq!(roles(&r), vec![Mesh, Rx, Rxtx, Rx]);
        assert_eq!(r[0].antenna.as_deref(), Some("dipole-2.4"));
        assert_eq!(r[2].antenna.as_deref(), Some("omni-5.8"));
        assert!(r[1].antenna.is_none());
        assert!(r[3].antenna.is_none());
        // Explicit entries switch the automatic rules off: no mesh is invented.
        let r = resolve_roles(&four(), &cfg, true);
        assert_eq!(roles(&r), vec![Mesh, Rx, Rxtx, Rx]);
    }

    #[test]
    fn tx_selection_prefers_a_tx_adapter_over_rxtx() {
        let mut r = resolve_roles(&four(), &RadioAdaptersConfig::default(), false);
        assert_eq!(tx_adapter(&r).unwrap().ifname(), "wlan1");
        r[2].role = Tx;
        assert_eq!(tx_adapter(&r).unwrap().ifname(), "wlan3");
        let plan = AdapterPlan::new(r).unwrap();
        assert_eq!(plan.tx().ifname(), "wlan3");
        assert!(plan.is_tx(2));
        // A tx-only adapter is not handed to wfb_rx, but it is brought up.
        assert_eq!(plan.rx_ifaces(), vec!["wlan1", "wlan2", "wlan4"]);
        assert_eq!(
            plan.radio_ifaces(),
            vec!["wlan3", "wlan1", "wlan2", "wlan4"]
        );
    }

    #[test]
    fn a_single_adapter_is_rxtx_even_with_the_mesh_enabled() {
        let one = vec![adapter(1)];
        for mesh in [false, true] {
            let r = resolve_roles(&one, &RadioAdaptersConfig::default(), mesh);
            assert_eq!(roles(&r), vec![Rxtx]);
            let plan = AdapterPlan::new(r).unwrap();
            assert_eq!(plan.tx().ifname(), "wlan1");
            assert_eq!(plan.rx_ifaces(), vec!["wlan1"]);
            assert_eq!(plan.radio_ifaces(), vec!["wlan1"]);
        }
    }

    #[test]
    fn a_plan_without_a_tx_role_transmits_on_the_first_receiver() {
        let r: Vec<ResolvedAdapter> = four()
            .into_iter()
            .map(|a| ResolvedAdapter {
                adapter: a,
                role: Rx,
                antenna: None,
            })
            .collect();
        let plan = AdapterPlan::new(r).unwrap();
        assert_eq!(plan.tx().ifname(), "wlan1");
        assert_eq!(
            plan.radio_ifaces(),
            vec!["wlan1", "wlan2", "wlan3", "wlan4"]
        );
    }

    #[test]
    fn no_adapters_or_only_mesh_is_no_plan() {
        assert!(AdapterPlan::new(Vec::new()).is_none());
        let mesh_only = vec![ResolvedAdapter {
            adapter: adapter(1),
            role: Mesh,
            antenna: None,
        }];
        assert!(AdapterPlan::new(mesh_only).is_none());
    }
}
