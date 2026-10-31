//! Firewall backend detection.

use super::*;

impl FirewallBackend {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            FirewallBackend::IptablesPersistent => "iptables-persistent",
            FirewallBackend::Nftables => "nftables",
            FirewallBackend::IptablesRuntime => "iptables-runtime",
            FirewallBackend::None => "none",
        }
    }
}

impl BackendDetector for PathBackendDetector {
    fn detect(&self) -> FirewallBackend {
        let have_iptables = which("iptables");
        let have_persistent = std::path::Path::new("/etc/iptables").is_dir();
        if have_iptables && have_persistent {
            return FirewallBackend::IptablesPersistent;
        }
        if which("nft") {
            return FirewallBackend::Nftables;
        }
        if have_iptables {
            return FirewallBackend::IptablesRuntime;
        }
        FirewallBackend::None
    }
}

/// PATH lookup for a bare executable name (no external crate).
fn which(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(bin);
        // is_file follows symlinks, which is what we want for /usr/sbin links.
        p.is_file()
    })
}
