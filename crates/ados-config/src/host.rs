//! Host-scoped policy: whether this agent may be reached from the network.
//!
//! One rule, one reader, because two services have to agree on it: the control
//! front (which picks its bind scope) and the ground media server (which decides
//! whether to advertise interface host candidates). A disagreement would leave a
//! media server offering a reach the control front does not answer on.
//!
//! The host kind itself is not here: it lives in `/etc/ados/host.conf`
//! ([`ados_protocol::host_kind`]), because it describes the machine rather than a
//! preference. Only `host.share_on_lan` is a config key, and it is read once per
//! service start: a change restarts both listeners rather than asking them to
//! re-bind live.

use std::path::Path;

use serde::Deserialize;

use crate::load_yaml_or_default;
use crate::log_store::CONFIG_YAML;

/// The `host:` config section.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct HostConfig {
    /// Let phones and tablets on this network reach a ground station running on
    /// this computer. Off (the default) means only this computer can.
    #[serde(default)]
    pub share_on_lan: bool,
}

/// Whether this host may be reached from the local network: an appliance always,
/// a personal computer only when `share_on_lan` is on.
///
/// The one function every bind-scope decision calls, so the control front and
/// the media server can never disagree about whether this node is shared.
pub fn reachable_on_lan(config_path: &Path) -> bool {
    let kind = ados_protocol::host_kind::read_host_kind();
    if !kind.is_personal() {
        return true;
    }
    load_yaml_or_default::<HostConfig>(config_path, "host").share_on_lan
}

/// [`reachable_on_lan`] for the canonical config path.
pub fn reachable_on_lan_now() -> bool {
    reachable_on_lan(Path::new(CONFIG_YAML))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_not_shared() {
        // Off is the safe side: an unreadable or absent section must never widen
        // a laptop's exposure.
        assert!(!HostConfig::default().share_on_lan);
    }

    #[test]
    fn an_explicit_flag_parses() {
        let cfg: HostConfig = crate::yaml_or_default("share_on_lan: true\n", "host");
        assert!(cfg.share_on_lan);
    }

    #[test]
    fn an_unrelated_section_reads_as_not_shared() {
        let cfg: HostConfig = crate::yaml_or_default("other: 1\n", "host");
        assert!(!cfg.share_on_lan);
    }
}
