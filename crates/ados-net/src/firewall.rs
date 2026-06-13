//! share_uplink firewall + sysctl persistence and the data-cap throttle.
//!
//! Wires runtime sysctl + NAT MASQUERADE for the share-uplink feature and
//! persists it across reboots: an `ip_forward` sysctl drop-in, an
//! iptables-persistent save (or an nftables ruleset rewrite), and a
//! reconcile-on-start that brings runtime state into agreement with the
//! persisted config flag. Also owns the data-cap throttle ladder that the
//! tracker's `data_cap_threshold` events drive. Ports
//! `share_uplink_firewall.py`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::cmd::CmdRunner;
use crate::router::events::DataCapState;
use crate::sidecar;

const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Persisted sysctl drop-in.
pub const SYSCTL_DROPIN_PATH: &str = "/etc/sysctl.d/99-ados-share-uplink.conf";
/// iptables-persistent rules file.
pub const IPTABLES_RULES_V4_PATH: &str = "/etc/iptables/rules.v4";
/// nftables ruleset file.
pub const NFTABLES_CONF_PATH: &str = "/etc/nftables.conf";

const NFT_TABLE: &str = "ados_nat";
const NFT_CHAIN: &str = "postrouting";

/// The default throttle rate at 95 percent of cap.
pub const THROTTLE_RATE_KBPS_95: u32 = 256;

/// Persistence backend. `IptablesRuntime` means iptables works but there is no
/// `/etc/iptables` dir, so rules apply now but do not survive reboot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirewallBackend {
    IptablesPersistent,
    Nftables,
    IptablesRuntime,
    None,
}

impl FirewallBackend {
    fn as_str(self) -> &'static str {
        match self {
            FirewallBackend::IptablesPersistent => "iptables-persistent",
            FirewallBackend::Nftables => "nftables",
            FirewallBackend::IptablesRuntime => "iptables-runtime",
            FirewallBackend::None => "none",
        }
    }
}

/// Resolves which firewall backend is available. Abstracted so tests can force
/// a backend without a real iptables / nft on PATH.
pub trait BackendDetector: Send + Sync {
    fn detect(&self) -> FirewallBackend;
}

/// Production detector: probes PATH for `iptables` / `nft` and checks for the
/// `/etc/iptables` dir that iptables-persistent owns. Mirrors
/// `detect_firewall_backend`.
#[derive(Debug, Default, Clone, Copy)]
pub struct PathBackendDetector;

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

/// The firewall controller. Holds the command runner, the backend detector,
/// and the paths (overridable for tests).
pub struct ShareUplinkFirewall {
    runner: Arc<dyn CmdRunner>,
    detector: Arc<dyn BackendDetector>,
    sysctl_dropin: PathBuf,
    iptables_rules_v4: PathBuf,
    nftables_conf: PathBuf,
}

impl ShareUplinkFirewall {
    /// Controller with the production detector and canonical paths.
    pub fn new(runner: Arc<dyn CmdRunner>) -> Self {
        Self::with_parts(
            runner,
            Arc::new(PathBackendDetector),
            PathBuf::from(SYSCTL_DROPIN_PATH),
            PathBuf::from(IPTABLES_RULES_V4_PATH),
            PathBuf::from(NFTABLES_CONF_PATH),
        )
    }

    /// Full constructor (tests).
    pub fn with_parts(
        runner: Arc<dyn CmdRunner>,
        detector: Arc<dyn BackendDetector>,
        sysctl_dropin: PathBuf,
        iptables_rules_v4: PathBuf,
        nftables_conf: PathBuf,
    ) -> Self {
        Self {
            runner,
            detector,
            sysctl_dropin,
            iptables_rules_v4,
            nftables_conf,
        }
    }

    pub fn backend(&self) -> FirewallBackend {
        self.detector.detect()
    }

    // ---------------- sysctl ----------------

    fn write_sysctl_dropin(&self) -> std::io::Result<()> {
        let body = "# Managed by ADOS share_uplink. Do not edit by hand.\nnet.ipv4.ip_forward=1\n";
        sidecar::write_atomic(&self.sysctl_dropin, body.as_bytes())
    }

    fn remove_sysctl_dropin(&self) {
        match std::fs::remove_file(&self.sysctl_dropin) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(error = %e, "share_uplink.sysctl_dropin_remove_failed"),
        }
    }

    async fn apply_sysctl_runtime(&self, enabled: bool) -> Option<String> {
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

    // ---------------- iptables ----------------

    async fn iptables_rule_present(&self, iface: &str) -> bool {
        self.runner
            .run(
                &[
                    "iptables",
                    "-t",
                    "nat",
                    "-C",
                    "POSTROUTING",
                    "-o",
                    iface,
                    "-j",
                    "MASQUERADE",
                ],
                CMD_TIMEOUT,
            )
            .await
            .ok()
    }

    async fn iptables_add_rule(&self, iface: &str) -> Option<String> {
        let out = self
            .runner
            .run(
                &[
                    "iptables",
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "-o",
                    iface,
                    "-j",
                    "MASQUERADE",
                ],
                CMD_TIMEOUT,
            )
            .await;
        (!out.ok()).then(|| non_empty(&out.stderr, "iptables_add_failed"))
    }

    async fn iptables_remove_rule(&self, iface: &str) -> Option<String> {
        if !self.iptables_rule_present(iface).await {
            return None;
        }
        let out = self
            .runner
            .run(
                &[
                    "iptables",
                    "-t",
                    "nat",
                    "-D",
                    "POSTROUTING",
                    "-o",
                    iface,
                    "-j",
                    "MASQUERADE",
                ],
                CMD_TIMEOUT,
            )
            .await;
        (!out.ok()).then(|| non_empty(&out.stderr, "iptables_remove_failed"))
    }

    async fn iptables_save(&self) -> Option<String> {
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

    // ---------------- nftables ----------------

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

    async fn nft_rule_present(&self, iface: &str) -> bool {
        let out = self
            .runner
            .run(
                &["nft", "list", "chain", "ip", NFT_TABLE, NFT_CHAIN],
                CMD_TIMEOUT,
            )
            .await;
        if !out.ok() {
            return false;
        }
        out.stdout.contains(&format!("oifname \"{iface}\"")) && out.stdout.contains("masquerade")
    }

    async fn nft_add_rule(&self, iface: &str) -> Option<String> {
        if let Some(err) = self.nft_ensure_table_chain().await {
            return Some(err);
        }
        if self.nft_rule_present(iface).await {
            return None;
        }
        let out = self
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
        (!out.ok()).then(|| non_empty(&out.stderr, "nft_add_failed"))
    }

    async fn nft_remove_rule(&self) -> Option<String> {
        let out = self
            .runner
            .run(
                &["nft", "flush", "chain", "ip", NFT_TABLE, NFT_CHAIN],
                CMD_TIMEOUT,
            )
            .await;
        if !out.ok() && !out.stderr.to_lowercase().contains("no such") {
            return Some(non_empty(&out.stderr, "nft_flush_failed"));
        }
        None
    }

    async fn nft_save(&self) -> Option<String> {
        let out = self
            .runner
            .run(&["nft", "list", "ruleset"], CMD_TIMEOUT)
            .await;
        if !out.ok() {
            return Some(non_empty(&out.stderr, "nft_save_failed"));
        }
        let body = format!("{}\n", out.stdout);
        if let Err(exc) = sidecar::write_atomic(&self.nftables_conf, body.as_bytes()) {
            return Some(format!("nft_save_write_failed: {exc}"));
        }
        None
    }

    // ---------------- public apply ----------------

    /// Apply or remove sysctl + NAT MASQUERADE and persist. Best-effort: never
    /// panics. Mirrors `apply_share_uplink`. Returns `{applied, backend,
    /// apply_error}`.
    pub async fn apply_share_uplink(&self, enabled: bool, active_iface: Option<&str>) -> Value {
        let backend = self.backend();
        if backend == FirewallBackend::None {
            let msg = "no_firewall_backend (neither iptables nor nftables found)";
            error!("share_uplink.no_backend");
            return json!({"applied": false, "backend": backend.as_str(), "apply_error": msg});
        }

        let mut apply_error: Option<String> = None;

        // sysctl.
        if let Some(rt_err) = self.apply_sysctl_runtime(enabled).await {
            apply_error = Some(rt_err);
        }
        let dropin_res = if enabled {
            self.write_sysctl_dropin()
        } else {
            self.remove_sysctl_dropin();
            Ok(())
        };
        if let Err(exc) = dropin_res {
            apply_error = apply_error.or(Some(format!("sysctl_dropin_failed: {exc}")));
        }

        // NAT.
        if let Some(iface) = active_iface.filter(|s| !s.is_empty()) {
            match backend {
                FirewallBackend::IptablesPersistent | FirewallBackend::IptablesRuntime => {
                    if enabled {
                        if !self.iptables_rule_present(iface).await {
                            if let Some(err) = self.iptables_add_rule(iface).await {
                                apply_error = apply_error.or(Some(err));
                            }
                        }
                    } else if let Some(err) = self.iptables_remove_rule(iface).await {
                        apply_error = apply_error.or(Some(err));
                    }
                    if backend == FirewallBackend::IptablesPersistent {
                        if let Some(err) = self.iptables_save().await {
                            apply_error = apply_error.or(Some(err));
                        }
                    } else {
                        warn!("share_uplink.iptables_no_persistence");
                    }
                }
                FirewallBackend::Nftables => {
                    let err = if enabled {
                        self.nft_add_rule(iface).await
                    } else {
                        self.nft_remove_rule().await
                    };
                    if let Some(e) = err {
                        apply_error = apply_error.or(Some(e));
                    }
                    if let Some(e) = self.nft_save().await {
                        apply_error = apply_error.or(Some(e));
                    }
                }
                FirewallBackend::None => {}
            }
        } else if enabled {
            warn!("share_uplink.no_active_iface");
        }

        info!(
            enabled = enabled,
            iface = active_iface,
            backend = backend.as_str(),
            "share_uplink.apply_done"
        );
        json!({
            "applied": apply_error.is_none(),
            "backend": backend.as_str(),
            "apply_error": apply_error,
        })
    }

    // ---------------- tc throttle ----------------

    async fn tc_add_throttle(&self, iface: &str, rate_kbps: u32) -> Option<String> {
        // Delete any existing root qdisc first so repeated calls converge.
        // Absence is fine; ignore the result.
        self.runner
            .run(&["tc", "qdisc", "del", "dev", iface, "root"], CMD_TIMEOUT)
            .await;
        let rate = format!("{rate_kbps}kbit");
        let out = self
            .runner
            .run(
                &[
                    "tc", "qdisc", "add", "dev", iface, "root", "tbf", "rate", &rate, "burst",
                    "32kbit", "latency", "400ms",
                ],
                CMD_TIMEOUT,
            )
            .await;
        (!out.ok()).then(|| non_empty(&out.stderr, "tc_add_failed"))
    }

    async fn tc_remove_throttle(&self, iface: &str) -> Option<String> {
        let out = self
            .runner
            .run(&["tc", "qdisc", "del", "dev", iface, "root"], CMD_TIMEOUT)
            .await;
        let low = out.stderr.to_lowercase();
        if !out.ok() && !low.contains("no such") && !low.contains("cannot find") {
            return Some(non_empty(&out.stderr, "tc_remove_failed"));
        }
        None
    }

    /// Remove the root qdisc (the data-cap throttle) from a single iface without
    /// touching NAT. Used when the active uplink fails over OFF the metered
    /// cellular iface: the throttle qdisc was applied to the cellular iface, so
    /// when that iface stops carrying the uplink the stale qdisc must be cleared
    /// from it (NAT is owned by the share-uplink consumer and follows the active
    /// uplink on its own). Absent qdisc is not an error.
    pub async fn clear_throttle(&self, iface: &str) -> Value {
        let tc_err = self.tc_remove_throttle(iface).await;
        json!({
            "cleared": tc_err.is_none(),
            "iface": iface,
            "tc_error": tc_err,
        })
    }

    /// Apply bandwidth throttle or hard block on the active uplink. Mirrors
    /// `apply_throttle`. The blocked_100 path removes the qdisc THEN drops the
    /// MASQUERADE rule (ordering matters: the qdisc must go before NAT stops so
    /// a stale throttle never lingers on a re-enabled link).
    pub async fn apply_throttle(&self, active_iface: Option<&str>, state: DataCapState) -> Value {
        let iface = match active_iface.filter(|s| !s.is_empty()) {
            Some(i) => i,
            None => {
                return json!({"applied": false, "state": state, "reason": "no_active_iface"});
            }
        };

        match state {
            DataCapState::Ok | DataCapState::Warn80 => {
                let tc_err = self.tc_remove_throttle(iface).await;
                // Re-add NAT in case a previous blocked_100 removed it.
                let backend = self.backend();
                let mut nat_restore_error: Option<String> = None;
                match backend {
                    FirewallBackend::IptablesPersistent | FirewallBackend::IptablesRuntime => {
                        if !self.iptables_rule_present(iface).await {
                            nat_restore_error = self.iptables_add_rule(iface).await;
                            if backend == FirewallBackend::IptablesPersistent {
                                self.iptables_save().await;
                            }
                        }
                    }
                    FirewallBackend::Nftables => {
                        nat_restore_error = self.nft_add_rule(iface).await;
                        self.nft_save().await;
                    }
                    FirewallBackend::None => {}
                }
                json!({
                    "state": state,
                    "iface": iface,
                    "tc_error": tc_err,
                    "nat_restore_error": nat_restore_error,
                    "applied": tc_err.is_none(),
                })
            }
            DataCapState::Throttle95 => {
                let tc_err = self.tc_add_throttle(iface, THROTTLE_RATE_KBPS_95).await;
                json!({
                    "state": state,
                    "iface": iface,
                    "tc_error": tc_err,
                    "rate_kbps": THROTTLE_RATE_KBPS_95,
                    "applied": tc_err.is_none(),
                })
            }
            DataCapState::Blocked100 => {
                // Ordering risk: remove the throttle qdisc FIRST, THEN drop NAT.
                self.tc_remove_throttle(iface).await;
                let backend = self.backend();
                let mut nat_err: Option<String> = None;
                match backend {
                    FirewallBackend::IptablesPersistent | FirewallBackend::IptablesRuntime => {
                        nat_err = self.iptables_remove_rule(iface).await;
                        if backend == FirewallBackend::IptablesPersistent {
                            self.iptables_save().await;
                        }
                    }
                    FirewallBackend::Nftables => {
                        nat_err = self.nft_remove_rule().await;
                        self.nft_save().await;
                    }
                    FirewallBackend::None => {}
                }
                json!({
                    "state": state,
                    "iface": iface,
                    "nat_remove_error": nat_err,
                    "applied": nat_err.is_none(),
                })
            }
        }
    }

    /// Reconcile firewall state against a configured share_uplink flag and the
    /// active iface, on service start. Mirrors `reconcile_on_start` (config
    /// load + active-iface lookup are the caller's job here; this takes the
    /// resolved inputs and applies them).
    pub async fn reconcile_on_start(
        &self,
        configured_enabled: bool,
        active_iface: Option<&str>,
    ) -> Value {
        info!(
            configured = configured_enabled,
            iface = active_iface,
            "share_uplink.reconcile_start"
        );
        let result = self
            .apply_share_uplink(configured_enabled, active_iface)
            .await;
        json!({
            "reconciled": true,
            "configured_enabled": configured_enabled,
            "iface": active_iface,
            "applied": result["applied"],
            "backend": result["backend"],
            "apply_error": result["apply_error"],
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::testing::ScriptedRunner;
    use crate::cmd::CmdOut;

    struct FixedBackend(FirewallBackend);
    impl BackendDetector for FixedBackend {
        fn detect(&self) -> FirewallBackend {
            self.0
        }
    }

    fn fw(
        runner: Arc<ScriptedRunner>,
        backend: FirewallBackend,
        dir: &std::path::Path,
    ) -> ShareUplinkFirewall {
        ShareUplinkFirewall::with_parts(
            runner,
            Arc::new(FixedBackend(backend)),
            dir.join("99-ados-share-uplink.conf"),
            dir.join("rules.v4"),
            dir.join("nftables.conf"),
        )
    }

    #[tokio::test]
    async fn apply_enable_iptables_persistent_adds_rule_and_saves_dropin() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut::failed(0, "")); // sysctl -w ok
        runner.push(CmdOut::failed(1, "")); // -C present? rc!=0 → absent
        runner.push(CmdOut::failed(0, "")); // -A add ok
        runner.push(CmdOut {
            rc: 0,
            stdout: "# rules\n".to_string(),
            stderr: String::new(),
        }); // iptables-save
        let f = fw(
            runner.clone(),
            FirewallBackend::IptablesPersistent,
            dir.path(),
        );
        let res = f.apply_share_uplink(true, Some("eth0")).await;
        assert_eq!(res["applied"], true);
        assert_eq!(res["backend"], "iptables-persistent");
        // sysctl drop-in persisted with ip_forward=1.
        let dropin = std::fs::read_to_string(dir.path().join("99-ados-share-uplink.conf")).unwrap();
        assert!(dropin.contains("net.ipv4.ip_forward=1"));
        // rules.v4 written by the save.
        assert!(dir.path().join("rules.v4").is_file());
    }

    #[tokio::test]
    async fn apply_with_no_backend_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        let f = fw(runner, FirewallBackend::None, dir.path());
        let res = f.apply_share_uplink(true, Some("eth0")).await;
        assert_eq!(res["applied"], false);
        assert_eq!(res["backend"], "none");
        assert!(res["apply_error"]
            .as_str()
            .unwrap()
            .contains("no_firewall_backend"));
    }

    #[tokio::test]
    async fn throttle_95_installs_exact_tbf_args() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut::failed(0, "")); // tc del (pre-clean)
        runner.push(CmdOut::failed(0, "")); // tc add ok
        let f = fw(runner.clone(), FirewallBackend::IptablesRuntime, dir.path());
        let res = f
            .apply_throttle(Some("wwan0"), DataCapState::Throttle95)
            .await;
        assert_eq!(res["applied"], true);
        assert_eq!(res["rate_kbps"], 256);
        // The add call carries the exact tbf parameters.
        let add = &runner.recorded()[1];
        assert_eq!(
            add,
            &vec![
                "tc", "qdisc", "add", "dev", "wwan0", "root", "tbf", "rate", "256kbit", "burst",
                "32kbit", "latency", "400ms",
            ]
        );
    }

    #[tokio::test]
    async fn blocked_100_removes_qdisc_before_dropping_masquerade() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut::failed(0, "")); // tc del (remove throttle)
        runner.push(CmdOut::failed(0, "")); // iptables -C present? rc 0 → present
        runner.push(CmdOut::failed(0, "")); // iptables -D remove ok
        runner.push(CmdOut {
            rc: 0,
            stdout: "# rules\n".to_string(),
            stderr: String::new(),
        }); // iptables-save
        let f = fw(
            runner.clone(),
            FirewallBackend::IptablesPersistent,
            dir.path(),
        );
        let res = f
            .apply_throttle(Some("wwan0"), DataCapState::Blocked100)
            .await;
        assert_eq!(res["applied"], true);
        let calls = runner.recorded();
        // Ordering: the tc-del comes before the iptables -D.
        let tc_del_idx = calls
            .iter()
            .position(|c| c.first().map(String::as_str) == Some("tc"))
            .unwrap();
        let ipt_del_idx = calls
            .iter()
            .position(|c| c.contains(&"-D".to_string()))
            .unwrap();
        assert!(
            tc_del_idx < ipt_del_idx,
            "qdisc removal must precede MASQUERADE drop"
        );
    }

    #[tokio::test]
    async fn throttle_no_active_iface_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        let f = fw(runner, FirewallBackend::IptablesRuntime, dir.path());
        let res = f.apply_throttle(None, DataCapState::Throttle95).await;
        assert_eq!(res["applied"], false);
        assert_eq!(res["reason"], "no_active_iface");
    }
}
