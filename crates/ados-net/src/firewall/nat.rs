//! Share-uplink NAT: sysctl forwarding plus the iptables / nftables MASQUERADE rule.

use super::*;

impl ShareUplinkFirewall {
    pub(super) fn sync_sysctl_dropin(&self, enabled: bool) -> std::io::Result<()> {
        if !enabled {
            return match std::fs::remove_file(&self.sysctl_dropin) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let body = "# Managed by ADOS share_uplink. Do not edit by hand.\nnet.ipv4.ip_forward=1\n";
        if std::fs::read_to_string(&self.sysctl_dropin).is_ok_and(|cur| cur == body) {
            return Ok(());
        }
        sidecar::write_atomic(&self.sysctl_dropin, body.as_bytes())
    }

    pub(super) async fn apply_sysctl_runtime(&self, enabled: bool) -> Option<String> {
        let arg = if enabled {
            "net.ipv4.ip_forward=1"
        } else {
            "net.ipv4.ip_forward=0"
        };
        let out = self.runner.run(&["sysctl", "-w", arg], CMD_TIMEOUT).await;
        if !out.ok() {
            return Some(non_empty(&out.stderr, "sysctl_failed"));
        }
        None
    }

    async fn ipt(&self, args: &[&str]) -> crate::cmd::CmdOut {
        let mut argv = vec!["iptables", "-t", "nat"];
        argv.extend_from_slice(args);
        self.runner.run(&argv, CMD_TIMEOUT).await
    }

    /// Bring the `ADOS_NAT` chain to exactly the rule `nat_iface` needs.
    /// Returns `(changed, error)`.
    pub(super) async fn iptables_sync(&self, nat_iface: Option<&str>) -> (bool, Option<String>) {
        let mut changed = false;
        let created = self.ipt(&["-N", IPT_CHAIN]).await;
        if !created.ok() && !created.stderr.to_lowercase().contains("exists") {
            return (
                false,
                Some(non_empty(&created.stderr, "iptables_chain_failed")),
            );
        }
        if !self.ipt(&["-C", "POSTROUTING", "-j", IPT_CHAIN]).await.ok() {
            let jump = self.ipt(&["-A", "POSTROUTING", "-j", IPT_CHAIN]).await;
            if !jump.ok() {
                return (false, Some(non_empty(&jump.stderr, "iptables_jump_failed")));
            }
            changed = true;
        }
        let listed = self.ipt(&["-S", IPT_CHAIN]).await;
        if !listed.ok() {
            return (
                changed,
                Some(non_empty(&listed.stderr, "iptables_list_failed")),
            );
        }
        let current: Vec<&str> = listed
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with(&format!("-A {IPT_CHAIN} ")))
            .collect();
        let wanted = nat_iface.map(|i| format!("-A {IPT_CHAIN} -o {i} -j MASQUERADE"));
        let in_sync = match &wanted {
            Some(rule) => current.len() == 1 && current[0] == rule.as_str(),
            None => current.is_empty(),
        };
        if in_sync {
            return (changed, None);
        }
        let flushed = self.ipt(&["-F", IPT_CHAIN]).await;
        if !flushed.ok() {
            return (
                changed,
                Some(non_empty(&flushed.stderr, "iptables_flush_failed")),
            );
        }
        if let Some(iface) = nat_iface {
            let add = self
                .ipt(&["-A", IPT_CHAIN, "-o", iface, "-j", "MASQUERADE"])
                .await;
            if !add.ok() {
                return (true, Some(non_empty(&add.stderr, "iptables_add_failed")));
            }
        }
        (true, None)
    }

    pub(super) async fn iptables_save(&self) -> Option<String> {
        let out = self.runner.run(&["iptables-save"], CMD_TIMEOUT).await;
        if !out.ok() {
            return Some(non_empty(&out.stderr, "iptables_save_failed"));
        }
        let body = format!("{}\n", out.stdout);
        if let Err(exc) = sidecar::write_atomic(&self.iptables_rules_v4, body.as_bytes()) {
            return Some(format!("iptables_save_write_failed: {exc}"));
        }
        None
    }

    async fn nft_ensure_table_chain(&self) -> Option<String> {
        let t = self
            .runner
            .run(&["nft", "add", "table", "ip", NFT_TABLE], CMD_TIMEOUT)
            .await;
        if !t.ok() && !t.stderr.to_lowercase().contains("exists") {
            return Some(non_empty(&t.stderr, "nft_table_failed"));
        }
        let c = self
            .runner
            .run(
                &[
                    "nft",
                    "add",
                    "chain",
                    "ip",
                    NFT_TABLE,
                    NFT_CHAIN,
                    "{",
                    "type",
                    "nat",
                    "hook",
                    "postrouting",
                    "priority",
                    "100",
                    ";",
                    "}",
                ],
                CMD_TIMEOUT,
            )
            .await;
        if !c.ok() && !c.stderr.to_lowercase().contains("exists") {
            return Some(non_empty(&c.stderr, "nft_chain_failed"));
        }
        None
    }

    /// Bring the `ados_nat` postrouting chain to exactly the rule `nat_iface`
    /// needs. Returns `(changed, error)`.
    pub(super) async fn nft_sync(&self, nat_iface: Option<&str>) -> (bool, Option<String>) {
        if let Some(err) = self.nft_ensure_table_chain().await {
            return (false, Some(err));
        }
        let listed = self
            .runner
            .run(
                &["nft", "list", "chain", "ip", NFT_TABLE, NFT_CHAIN],
                CMD_TIMEOUT,
            )
            .await;
        if !listed.ok() {
            return (false, Some(non_empty(&listed.stderr, "nft_list_failed")));
        }
        let current: Vec<&str> = listed
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("masquerade"))
            .collect();
        let in_sync = match nat_iface {
            Some(iface) => {
                current.len() == 1 && current[0].contains(&format!("oifname \"{iface}\""))
            }
            None => current.is_empty(),
        };
        if in_sync {
            return (false, None);
        }
        let flushed = self
            .runner
            .run(
                &["nft", "flush", "chain", "ip", NFT_TABLE, NFT_CHAIN],
                CMD_TIMEOUT,
            )
            .await;
        if !flushed.ok() {
            return (false, Some(non_empty(&flushed.stderr, "nft_flush_failed")));
        }
        if let Some(iface) = nat_iface {
            let add = self
                .runner
                .run(
                    &[
                        "nft",
                        "add",
                        "rule",
                        "ip",
                        NFT_TABLE,
                        NFT_CHAIN,
                        "oifname",
                        iface,
                        "masquerade",
                    ],
                    CMD_TIMEOUT,
                )
                .await;
            if !add.ok() {
                return (true, Some(non_empty(&add.stderr, "nft_add_failed")));
            }
        }
        (true, None)
    }

    /// Persist only the `ados_nat` table, as an include that replaces itself
    /// on load. The system `nftables.conf` and every other table stay as the
    /// operator left them.
    pub(super) async fn nft_save(&self) -> Option<String> {
        let out = self
            .runner
            .run(&["nft", "list", "table", "ip", NFT_TABLE], CMD_TIMEOUT)
            .await;
        if !out.ok() {
            return Some(non_empty(&out.stderr, "nft_save_failed"));
        }
        let body = render_nft_include(&out.stdout);
        if let Err(exc) = sidecar::write_atomic(&self.nft_include, body.as_bytes()) {
            return Some(format!("nft_save_write_failed: {exc}"));
        }
        None
    }
}

/// The persisted nftables include: create-then-delete makes a reload replace
/// the table instead of appending to it or failing on an existing one.
fn render_nft_include(table_listing: &str) -> String {
    format!(
        "#!/usr/sbin/nft -f\n# Managed by ADOS share_uplink. Do not edit by hand.\ntable ip {NFT_TABLE}\ndelete table ip {NFT_TABLE}\n{}\n",
        table_listing.trim_end()
    )
}
