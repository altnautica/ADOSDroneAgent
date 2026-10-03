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
/// `/etc/iptables` dir that iptables-persistent owns.
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

    fn sync_sysctl_dropin(&self, enabled: bool) -> std::io::Result<()> {
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

    async fn ipt(&self, args: &[&str]) -> crate::cmd::CmdOut {
        let mut argv = vec!["iptables", "-t", "nat"];
        argv.extend_from_slice(args);
        self.runner.run(&argv, CMD_TIMEOUT).await
    }

    /// Bring the `ADOS_NAT` chain to exactly the rule `nat_iface` needs.
    /// Returns `(changed, error)`.
    async fn iptables_sync(&self, nat_iface: Option<&str>) -> (bool, Option<String>) {
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

    /// Bring the `ados_nat` postrouting chain to exactly the rule `nat_iface`
    /// needs. Returns `(changed, error)`.
    async fn nft_sync(&self, nat_iface: Option<&str>) -> (bool, Option<String>) {
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
    async fn nft_save(&self) -> Option<String> {
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

    // ---------------- tc shaping ----------------

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

    /// The root tbf on `iface`: `None` when there is none, `Some(rate)` when
    /// there is one (`rate` is `None` when it could not be parsed).
    async fn tc_root_tbf(&self, iface: &str) -> Option<Option<u32>> {
        let out = self
            .runner
            .run(&["tc", "qdisc", "show", "dev", iface, "root"], CMD_TIMEOUT)
            .await;
        if !out.ok() {
            return None;
        }
        parse_root_tbf(&out.stdout)
    }

    /// Hold the cellular iface to the rate the cap state needs, and clear a
    /// previous cellular iface's shaping when the modem iface changed.
    async fn sync_shaping(&self, applied: &mut Applied, intent: &FirewallIntent) -> Option<String> {
        let target = intent.cellular_iface.as_deref().filter(|s| !s.is_empty());
        if let Some(prev) = applied.shaped_iface.clone() {
            if Some(prev.as_str()) != target {
                self.tc_remove_throttle(&prev).await;
                applied.shaped_iface = None;
            }
        }
        let iface = target?;
        let current = self.tc_root_tbf(iface).await;
        match intent.shaping_rate_kbps() {
            None => {
                applied.shaped_iface = None;
                if current.is_some() {
                    return self.tc_remove_throttle(iface).await;
                }
                None
            }
            Some(rate) => {
                if current == Some(Some(rate)) {
                    applied.shaped_iface = Some(iface.to_string());
                    return None;
                }
                let err = self.tc_add_throttle(iface, rate).await;
                if err.is_none() {
                    applied.shaped_iface = Some(iface.to_string());
                }
                err
            }
        }
    }

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

/// The persisted nftables include: create-then-delete makes a reload replace
/// the table instead of appending to it or failing on an existing one.
fn render_nft_include(table_listing: &str) -> String {
    format!(
        "#!/usr/sbin/nft -f\n# Managed by ADOS share_uplink. Do not edit by hand.\ntable ip {NFT_TABLE}\ndelete table ip {NFT_TABLE}\n{}\n",
        table_listing.trim_end()
    )
}

/// Parse `tc qdisc show dev <iface> root`: `None` when the root is not a tbf,
/// `Some(rate_kbps)` when it is (`None` inside when the rate is not in kbit).
fn parse_root_tbf(stdout: &str) -> Option<Option<u32>> {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("qdisc tbf "))?;
    let mut tokens = line.split_whitespace();
    let rate = tokens
        .by_ref()
        .skip_while(|t| *t != "rate")
        .nth(1)
        .and_then(|r| {
            let lower = r.to_ascii_lowercase();
            lower.strip_suffix("kbit")?.parse::<u32>().ok()
        });
    Some(rate)
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
pub(crate) mod testing {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use async_trait::async_trait;
    use parking_lot::Mutex;

    use crate::cmd::{CmdOut, CmdRunner};

    #[derive(Default)]
    struct State {
        ipt_chain: bool,
        ipt_jump: bool,
        ipt_rules: Vec<String>,
        nft_rules: Vec<String>,
        qdisc: BTreeMap<String, u32>,
        calls: Vec<Vec<String>>,
    }

    #[derive(Default)]
    pub(crate) struct FakeNet {
        state: Mutex<State>,
    }

    fn ok(stdout: impl Into<String>) -> CmdOut {
        CmdOut {
            rc: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    impl FakeNet {
        /// Oifs of the MASQUERADE rules in whichever backend chain holds them.
        pub(crate) fn nat_ifaces(&self) -> Vec<String> {
            let s = self.state.lock();
            let mut all = s.ipt_rules.clone();
            all.extend(s.nft_rules.iter().cloned());
            all
        }
        /// The tbf rate on `iface`, if shaped.
        pub(crate) fn tbf_rate(&self, iface: &str) -> Option<u32> {
            self.state.lock().qdisc.get(iface).copied()
        }
        /// Drop the qdisc behind the reconciler's back.
        pub(crate) fn clear_qdisc(&self, iface: &str) {
            self.state.lock().qdisc.remove(iface);
        }
        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.state.lock().calls.clone()
        }

        fn nft_listing(rules: &[String]) -> String {
            let mut body = String::from(
                "table ip ados_nat {\n\tchain postrouting {\n\t\ttype nat hook postrouting priority srcnat; policy accept;\n",
            );
            for r in rules {
                body.push_str(&format!("\t\toifname \"{r}\" masquerade\n"));
            }
            body.push_str("\t}\n}\n");
            body
        }

        fn handle(&self, argv: &[&str]) -> CmdOut {
            let mut s = self.state.lock();
            s.calls.push(argv.iter().map(|a| a.to_string()).collect());
            match argv {
                ["iptables", "-t", "nat", "-N", _] => {
                    if s.ipt_chain {
                        return CmdOut::failed(1, "iptables: Chain already exists.");
                    }
                    s.ipt_chain = true;
                    ok("")
                }
                ["iptables", "-t", "nat", "-C", "POSTROUTING", "-j", _] => {
                    if s.ipt_jump {
                        ok("")
                    } else {
                        CmdOut::failed(1, "")
                    }
                }
                ["iptables", "-t", "nat", "-A", "POSTROUTING", "-j", _] => {
                    s.ipt_jump = true;
                    ok("")
                }
                ["iptables", "-t", "nat", "-S", chain] => {
                    let mut out = format!("-N {chain}\n");
                    for r in &s.ipt_rules {
                        out.push_str(&format!("-A {chain} -o {r} -j MASQUERADE\n"));
                    }
                    ok(out)
                }
                ["iptables", "-t", "nat", "-F", _] => {
                    s.ipt_rules.clear();
                    ok("")
                }
                ["iptables", "-t", "nat", "-A", _, "-o", iface, "-j", "MASQUERADE"] => {
                    s.ipt_rules.push(iface.to_string());
                    ok("")
                }
                ["iptables-save"] => ok("# rules\n"),
                ["nft", "list", "chain", ..] | ["nft", "list", "table", ..] => {
                    ok(Self::nft_listing(&s.nft_rules))
                }
                ["nft", "flush", "chain", ..] => {
                    s.nft_rules.clear();
                    ok("")
                }
                ["nft", "add", "rule", .., "oifname", iface, "masquerade"] => {
                    s.nft_rules.push(iface.to_string());
                    ok("")
                }
                ["tc", "qdisc", "show", "dev", iface, "root"] => match s.qdisc.get(*iface) {
                    Some(rate) => ok(format!(
                        "qdisc tbf 8001: root refcnt 2 rate {rate}Kbit burst 4Kb lat 400ms\n"
                    )),
                    None => ok("qdisc fq_codel 0: root refcnt 2 limit 10240p\n"),
                },
                ["tc", "qdisc", "del", "dev", iface, "root"] => {
                    if s.qdisc.remove(*iface).is_some() {
                        ok("")
                    } else {
                        CmdOut::failed(2, "Error: Cannot find specified qdisc on specified device.")
                    }
                }
                ["tc", "qdisc", "add", "dev", iface, "root", "tbf", "rate", rate, ..] => {
                    let kbps = rate.trim_end_matches("kbit").parse().unwrap();
                    s.qdisc.insert(iface.to_string(), kbps);
                    ok("")
                }
                _ => ok(""),
            }
        }
    }

    #[async_trait]
    impl CmdRunner for FakeNet {
        async fn run(&self, argv: &[&str], _timeout: Duration) -> CmdOut {
            self.handle(argv)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeNet;
    use super::*;

    struct FixedBackend(FirewallBackend);
    impl BackendDetector for FixedBackend {
        fn detect(&self) -> FirewallBackend {
            self.0
        }
    }

    fn fw(
        net: Arc<FakeNet>,
        backend: FirewallBackend,
        dir: &std::path::Path,
    ) -> ShareUplinkFirewall {
        ShareUplinkFirewall::with_parts(
            net,
            Arc::new(FixedBackend(backend)),
            dir.join("99-ados-share-uplink.conf"),
            dir.join("rules.v4"),
            dir.join("nftables.d").join("ados-nat.nft"),
        )
    }

    fn intent(share: bool, active: &str, cap: DataCapState) -> FirewallIntent {
        FirewallIntent {
            share_uplink: share,
            active_iface: Some(active.to_string()),
            cellular_iface: Some("wwan0".to_string()),
            cap_state: cap,
        }
    }

    #[tokio::test]
    async fn share_uplink_follows_the_active_uplink_and_leaves_no_stale_rule() {
        for backend in [
            FirewallBackend::IptablesPersistent,
            FirewallBackend::Nftables,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let net = Arc::new(FakeNet::default());
            let f = fw(Arc::clone(&net), backend, dir.path());
            let res = f.reconcile(intent(true, "eth0", DataCapState::Ok)).await;
            assert_eq!(res["applied"], true, "{res}");
            assert_eq!(net.nat_ifaces(), vec!["eth0"]);
            let dropin =
                std::fs::read_to_string(dir.path().join("99-ados-share-uplink.conf")).unwrap();
            assert!(dropin.contains("net.ipv4.ip_forward=1"));

            f.reconcile(intent(true, "wlan0", DataCapState::Ok)).await;
            assert_eq!(net.nat_ifaces(), vec!["wlan0"], "{backend:?}");

            f.reconcile(intent(false, "wlan0", DataCapState::Ok)).await;
            assert!(net.nat_ifaces().is_empty(), "{backend:?}");
            assert!(!dir.path().join("99-ados-share-uplink.conf").exists());
        }
    }

    #[tokio::test]
    async fn cap_transitions_never_add_nat_while_sharing_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(
            Arc::clone(&net),
            FirewallBackend::IptablesPersistent,
            dir.path(),
        );
        for cap in [
            DataCapState::Ok,
            DataCapState::Warn80,
            DataCapState::Blocked100,
            DataCapState::Ok,
        ] {
            f.reconcile(intent(false, "wwan0", cap)).await;
            assert!(net.nat_ifaces().is_empty(), "NAT added at {cap:?}");
        }
    }

    #[tokio::test]
    async fn the_cap_block_drops_nat_only_on_the_cellular_uplink() {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(Arc::clone(&net), FirewallBackend::Nftables, dir.path());
        // Wired uplink active while the cellular cap is exhausted: the wired
        // share stays.
        f.reconcile(intent(true, "eth0", DataCapState::Blocked100))
            .await;
        assert_eq!(net.nat_ifaces(), vec!["eth0"]);
        // Failover onto the blocked cellular link: no NAT through it.
        f.reconcile(intent(true, "wwan0", DataCapState::Blocked100))
            .await;
        assert!(net.nat_ifaces().is_empty());
        // A new month lifts the block.
        f.reconcile(intent(true, "wwan0", DataCapState::Ok)).await;
        assert_eq!(net.nat_ifaces(), vec!["wwan0"]);
    }

    #[tokio::test]
    async fn cap_shaping_survives_failover_away_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(
            Arc::clone(&net),
            FirewallBackend::IptablesRuntime,
            dir.path(),
        );
        f.reconcile(intent(false, "wwan0", DataCapState::Throttle95))
            .await;
        assert_eq!(net.tbf_rate("wwan0"), Some(THROTTLE_RATE_KBPS_95));
        f.reconcile(intent(false, "eth0", DataCapState::Throttle95))
            .await;
        assert_eq!(net.tbf_rate("wwan0"), Some(THROTTLE_RATE_KBPS_95));
        assert_eq!(net.tbf_rate("eth0"), None);
        f.reconcile(intent(false, "wwan0", DataCapState::Throttle95))
            .await;
        assert_eq!(net.tbf_rate("wwan0"), Some(THROTTLE_RATE_KBPS_95));
        // A qdisc lost behind the daemon's back is restored by the next pass.
        net.clear_qdisc("wwan0");
        f.reconcile(intent(false, "wwan0", DataCapState::Throttle95))
            .await;
        assert_eq!(net.tbf_rate("wwan0"), Some(THROTTLE_RATE_KBPS_95));
        // Back under the cap: unshaped.
        f.reconcile(intent(false, "wwan0", DataCapState::Warn80))
            .await;
        assert_eq!(net.tbf_rate("wwan0"), None);
    }

    #[tokio::test]
    async fn the_cap_block_holds_the_cellular_link_to_a_trickle() {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(
            Arc::clone(&net),
            FirewallBackend::IptablesRuntime,
            dir.path(),
        );
        f.reconcile(intent(false, "eth0", DataCapState::Blocked100))
            .await;
        assert_eq!(net.tbf_rate("wwan0"), Some(BLOCKED_RATE_KBPS));
    }

    #[tokio::test]
    async fn a_modem_iface_change_moves_the_shaping() {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(
            Arc::clone(&net),
            FirewallBackend::IptablesRuntime,
            dir.path(),
        );
        f.reconcile(intent(false, "eth0", DataCapState::Throttle95))
            .await;
        let mut moved = intent(false, "eth0", DataCapState::Throttle95);
        moved.cellular_iface = Some("usb0".to_string());
        f.reconcile(moved).await;
        assert_eq!(net.tbf_rate("wwan0"), None);
        assert_eq!(net.tbf_rate("usb0"), Some(THROTTLE_RATE_KBPS_95));
    }

    #[tokio::test]
    async fn a_steady_state_reconcile_rewrites_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(
            Arc::clone(&net),
            FirewallBackend::IptablesPersistent,
            dir.path(),
        );
        f.reconcile(intent(true, "eth0", DataCapState::Throttle95))
            .await;
        let before = net.calls().len();
        f.reconcile(intent(true, "eth0", DataCapState::Throttle95))
            .await;
        let mutating: Vec<_> = net.calls()[before..]
            .iter()
            .filter(|c| {
                c.iter().any(|a| a == "-F" || a == "add" || a == "del")
                    || c.first().is_some_and(|a| a == "iptables-save")
            })
            .cloned()
            .collect();
        assert!(mutating.is_empty(), "steady state mutated: {mutating:?}");
    }

    #[tokio::test]
    async fn nft_persists_only_the_ados_table() {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(Arc::clone(&net), FirewallBackend::Nftables, dir.path());
        f.reconcile(intent(true, "eth0", DataCapState::Ok)).await;
        let body =
            std::fs::read_to_string(dir.path().join("nftables.d").join("ados-nat.nft")).unwrap();
        assert!(body.starts_with("#!/usr/sbin/nft -f\n"));
        assert!(body.contains("table ip ados_nat\ndelete table ip ados_nat\n"));
        assert!(body.contains("oifname \"eth0\" masquerade"));
        assert!(!net.calls().iter().any(|c| c.iter().any(|a| a == "ruleset")));
    }

    #[tokio::test]
    async fn sharing_with_no_backend_reports_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(net, FirewallBackend::None, dir.path());
        let res = f.reconcile(intent(true, "eth0", DataCapState::Ok)).await;
        assert_eq!(res["applied"], false);
        assert_eq!(res["backend"], "none");
        assert!(res["apply_error"]
            .as_str()
            .unwrap()
            .contains("no_firewall_backend"));
    }

    #[tokio::test]
    async fn set_share_uplink_keeps_the_last_cap_state() {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(
            Arc::clone(&net),
            FirewallBackend::IptablesRuntime,
            dir.path(),
        );
        f.reconcile(intent(false, "wwan0", DataCapState::Blocked100))
            .await;
        // The operator turns sharing on while the cellular uplink is blocked.
        f.set_share_uplink(true, Some("wwan0")).await;
        assert!(net.nat_ifaces().is_empty());
        assert_eq!(net.tbf_rate("wwan0"), Some(BLOCKED_RATE_KBPS));
    }

    #[test]
    fn parse_root_tbf_reads_the_kbit_rate() {
        assert_eq!(
            parse_root_tbf("qdisc tbf 8001: root refcnt 2 rate 256Kbit burst 4Kb lat 400ms\n"),
            Some(Some(256))
        );
        assert_eq!(
            parse_root_tbf("qdisc tbf 8001: root refcnt 2 rate 2Mbit burst 4Kb lat 400ms\n"),
            Some(None)
        );
        assert_eq!(parse_root_tbf("qdisc fq_codel 0: root refcnt 2\n"), None);
    }
}
