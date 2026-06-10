//! Reactive self-healing watchdog for the onboard management-WiFi data path.
//!
//! On a board that carries both an onboard managed-WiFi adapter (a FullMAC chip
//! such as the Pi-family Broadcom or a Rock-family AIC8800) and a USB injection
//! adapter, the radio bring-up runs a global regulatory set and takes the
//! injection adapter into monitor mode while the onboard WiFi is already
//! associated. Some onboard FullMAC drivers survive the 802.11 association + WPA
//! keys through that churn but lose the data path: the interface still reports a
//! strong link, a valid IP, and a correct default route, yet passes no traffic
//! (the gateway neighbor never resolves, every ping is lost). The box then has
//! no working failover when its wired link is unplugged.
//!
//! The break lands late (tens of seconds after monitor entry) and at a variable
//! point in the radio bring-up, and channel/bind operations during normal flight
//! can re-break the link later. A one-shot rebuild right after monitor entry
//! would fire before the break and be undone. So this is a REACTIVE watchdog: on
//! a periodic tick it checks each onboard managed-WiFi connection that is
//! associated, has an IPv4 address, and has a known gateway, and asserts the
//! gateway is reachable (the neighbor table resolves it). When the gateway is
//! unreachable for a sustained window (N consecutive failing ticks) while the
//! association is up, it RE-ASSOCIATES that connection (the proven NetworkManager
//! down/up), then holds a cooldown before it could act again so it never flaps.
//!
//! Safety invariants:
//! - It NEVER touches the injection interface (the monitor-mode radio adapter):
//!   that interface runs a WFB-compatible driver, is in monitor mode, and is not
//!   a managed-WiFi connection — three independent gates each exclude it.
//! - It NEVER touches wired (the nmcli type filter keeps only `802-11-wireless`).
//! - It is a no-op when there is no onboard managed WiFi, when the WiFi is
//!   healthy, or when the WiFi is not associated at all (that is
//!   NetworkManager's job, not ours).
//!
//! The pure logic (terse-nmcli parsing, candidate classification, gateway and
//! neighbor parsing, the threshold/cooldown state machine) is unit-tested on
//! every host. All OS calls (nmcli, iw, ip, sysfs reads) are Linux-only; on a
//! non-Linux dev host the tick is an inert no-op so the crate still builds.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

/// The event kind recorded on each onboard-WiFi re-association. Bland and
/// reader-facing: it names what the code did, not any internal milestone.
pub const REASSOC_KIND: &str = "network.wifi_reassociated";

/// Default consecutive-failure count before a heal fires. A single failing tick
/// can be a momentarily-busy gateway; two in a row is a sustained dead path.
const DEFAULT_FAIL_THRESHOLD: u32 = 2;

/// Default quiet period after a heal, per connection. A re-association takes a
/// few seconds to re-DHCP; this window covers that plus slack so the watchdog
/// never re-fires on a connection that is mid-recovery.
const DEFAULT_COOLDOWN_S: u64 = 60;

/// WFB-compatible driver names: an interface running one of these is the USB
/// injection adapter, never an onboard management link. Matches the radio
/// adapter selection's compatible-driver set so the two halves agree on which
/// interface is the radio. Lower-cased compare.
#[cfg(any(target_os = "linux", test))]
const INJECTION_DRIVERS: &[&str] = &[
    "8812au",
    "8812eu",
    "rtl8812au",
    "rtl8812eu",
    "rtl88x2eu",
    "rtl88xxau",
];

/// Configuration for the WiFi self-heal watchdog, read from
/// `network.wifi_selfheal`. Default-ON: a fresh board with no config heals out
/// of the box. An operator can disable it cleanly if a bespoke network setup
/// ever conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiSelfHealConfig {
    /// Whether the watchdog runs at all. Default true.
    pub enabled: bool,
    /// Consecutive failing ticks before a re-association fires. Floored at 1 so a
    /// zero in config can never make a single transient failure trigger a heal.
    pub fail_threshold: u32,
    /// Per-connection quiet period after a heal.
    pub cooldown: Duration,
}

impl Default for WifiSelfHealConfig {
    fn default() -> Self {
        WifiSelfHealConfig {
            enabled: true,
            fail_threshold: DEFAULT_FAIL_THRESHOLD,
            cooldown: Duration::from_secs(DEFAULT_COOLDOWN_S),
        }
    }
}

/// Parse `network.wifi_selfheal` out of a config body. An absent section reads
/// as the all-defaults (enabled) config, so the watchdog is on out of the box. A
/// malformed config also falls back to the enabled default rather than silently
/// disabling the failover.
pub fn read_config_from(text: &str) -> WifiSelfHealConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        wifi_selfheal: Option<SelfHeal>,
    }
    #[derive(serde::Deserialize)]
    struct SelfHeal {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        fail_threshold: Option<u32>,
        #[serde(default)]
        cooldown_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.wifi_selfheal {
            Some(sh) => WifiSelfHealConfig {
                enabled: sh.enabled,
                fail_threshold: sh.fail_threshold.unwrap_or(DEFAULT_FAIL_THRESHOLD).max(1),
                cooldown: Duration::from_secs(sh.cooldown_s.unwrap_or(DEFAULT_COOLDOWN_S)),
            },
            None => WifiSelfHealConfig::default(),
        },
        Err(_) => WifiSelfHealConfig::default(),
    }
}

/// Read `network.wifi_selfheal` from the canonical config path. Re-read each
/// tick so a config edit takes effect without restarting the supervisor. Linux
/// only — the tick that reads it is a no-op on a non-Linux dev host.
#[cfg(target_os = "linux")]
fn read_config() -> WifiSelfHealConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => WifiSelfHealConfig::default(),
    }
}

/// One onboard managed-WiFi connection considered by the watchdog: the
/// NetworkManager connection name and the interface it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiConnection {
    /// The NetworkManager connection name (the `nmcli connection up <name>` key).
    pub name: String,
    /// The interface the connection is bound to.
    pub iface: String,
}

/// Per-connection self-heal state: how many consecutive ticks have seen a dead
/// gateway, and when (if ever) this connection was last healed. Read only on the
/// Linux tick (and in tests); on a non-Linux dev host the tick is a no-op, so the
/// fields exist but are unread there.
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
struct ConnState {
    consecutive_failures: u32,
    last_heal: Option<Instant>,
}

/// What the state machine decided to do for one connection on one tick. Pure so
/// the threshold + cooldown logic is unit-tested without nmcli / ip / a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealDecision {
    /// Gateway reachable: clear the failure count, do nothing.
    Healthy,
    /// Gateway unreachable but the threshold is not met yet, or a heal cooldown
    /// is still in force: accumulate, do nothing this tick. Carries the running
    /// consecutive-failure count for the log.
    Wait { consecutive_failures: u32 },
    /// Threshold met and no cooldown in force: re-associate now. Carries the
    /// failure count that crossed the threshold for the heal event.
    Heal { consecutive_failures: u32 },
}

/// The reactive self-heal watchdog. Holds its per-connection state across ticks;
/// `tick` is called from the supervisor's monitor pass. The fields are read only
/// on the Linux tick (and in tests); on a non-Linux dev host the tick is an inert
/// no-op, so the fields are constructed but unread there (the `events` shipper is
/// only driven from the Linux heal path).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct WifiSelfHeal {
    states: HashMap<String, ConnState>,
    events: EventEmitter,
}

impl WifiSelfHeal {
    /// Build a watchdog that records heal events through `events`.
    pub fn new(events: EventEmitter) -> Self {
        WifiSelfHeal {
            states: HashMap::new(),
            events,
        }
    }

    /// Pure state-machine step for one connection: fold this tick's gateway
    /// reachability into the connection's running state and decide whether to
    /// heal, given the threshold + cooldown. `now` is the current instant and
    /// `cooldown` the post-heal quiet window. Records the decision back into the
    /// per-connection state (failure count reset / increment, heal timestamp set
    /// on a Heal). Split out so the threshold + cooldown contract is testable
    /// without any OS calls or a real clock.
    #[cfg(any(target_os = "linux", test))]
    fn step(
        &mut self,
        name: &str,
        gateway_reachable: bool,
        fail_threshold: u32,
        cooldown: Duration,
        now: Instant,
    ) -> HealDecision {
        let st = self.states.entry(name.to_string()).or_default();
        if gateway_reachable {
            st.consecutive_failures = 0;
            return HealDecision::Healthy;
        }
        st.consecutive_failures = st.consecutive_failures.saturating_add(1);
        let count = st.consecutive_failures;
        // Below the sustained-failure threshold: keep watching.
        if count < fail_threshold.max(1) {
            return HealDecision::Wait {
                consecutive_failures: count,
            };
        }
        // Threshold met, but a recent heal still owns the cooldown: a
        // re-association takes a few seconds to re-DHCP, so do not re-fire on a
        // connection that is mid-recovery (anti-flap).
        if let Some(last) = st.last_heal {
            if now.duration_since(last) < cooldown {
                return HealDecision::Wait {
                    consecutive_failures: count,
                };
            }
        }
        // Fire: record the heal time and reset the count so the next failure
        // sequence starts fresh after the cooldown.
        st.last_heal = Some(now);
        st.consecutive_failures = 0;
        HealDecision::Heal {
            consecutive_failures: count,
        }
    }

    /// Drop per-connection state for connections that are no longer candidates,
    /// so a connection that goes away (adapter unplugged, profile deleted) does
    /// not pin stale state forever.
    #[cfg(any(target_os = "linux", test))]
    fn prune(&mut self, live: &[WifiConnection]) {
        self.states
            .retain(|name, _| live.iter().any(|c| &c.name == name));
    }

    /// One watchdog tick: enumerate onboard managed-WiFi candidates, probe each
    /// one's gateway, run the state machine, and re-associate the ones whose data
    /// path has been dead for the sustained window. Re-reads config each tick so
    /// an edit takes effect without a restart. A no-op when disabled, when there
    /// is no onboard managed WiFi, when nmcli is absent, or when every candidate
    /// is healthy.
    #[cfg(target_os = "linux")]
    pub async fn tick(&mut self) {
        let cfg = read_config();
        if !cfg.enabled {
            return;
        }
        if !nmcli_available().await {
            return;
        }
        let candidates = enumerate_candidates().await;
        self.prune(&candidates);
        if candidates.is_empty() {
            return;
        }
        let now = Instant::now();
        for conn in candidates {
            // Determine the gateway for this connection's interface. No gateway
            // means there is nothing to probe (the link is not the LAN path), so
            // it is not a self-heal candidate this tick — clear and move on.
            let Some(gateway) = default_gateway_for_iface(&conn.iface).await else {
                self.states
                    .entry(conn.name.clone())
                    .or_default()
                    .consecutive_failures = 0;
                continue;
            };
            let reachable = gateway_reachable(&conn.iface, &gateway).await;
            let decision = self.step(&conn.name, reachable, cfg.fail_threshold, cfg.cooldown, now);
            match decision {
                HealDecision::Healthy => {}
                HealDecision::Wait {
                    consecutive_failures,
                } => {
                    tracing::warn!(
                        connection = %conn.name,
                        iface = %conn.iface,
                        gateway = %gateway,
                        consecutive_failures,
                        "wifi_selfheal_gateway_unreachable"
                    );
                }
                HealDecision::Heal {
                    consecutive_failures,
                } => {
                    tracing::warn!(
                        connection = %conn.name,
                        iface = %conn.iface,
                        gateway = %gateway,
                        consecutive_failures,
                        "wifi_selfheal_reassociating"
                    );
                    reactivate_connection(&conn.name).await;
                    self.events.emit(
                        REASSOC_KIND,
                        ados_protocol::logd::Level::Info,
                        reassoc_detail(
                            &conn.iface,
                            &conn.name,
                            &gateway,
                            consecutive_failures,
                            cfg.cooldown.as_secs(),
                        ),
                    );
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn tick(&mut self) {}
}

/// Build the `network.wifi_reassociated` detail map. All fields are bland and
/// reader-facing. Built only on the Linux heal path.
#[cfg(target_os = "linux")]
fn reassoc_detail(
    iface: &str,
    connection: &str,
    gateway: &str,
    consecutive_failures: u32,
    cooldown_sec: u64,
) -> ados_protocol::logd::Fields {
    use ados_protocol::logd::{Fields, Value as MpVal};
    let mut d = Fields::new();
    d.insert("interface".to_string(), MpVal::from(iface));
    d.insert("connection".to_string(), MpVal::from(connection));
    d.insert("gateway".to_string(), MpVal::from(gateway));
    d.insert(
        "consecutive_failures".to_string(),
        MpVal::from(consecutive_failures as u64),
    );
    d.insert("cooldown_sec".to_string(), MpVal::from(cooldown_sec));
    d
}

// ---------------------------------------------------------------------------
// Pure parsing + classification (unit-tested on every host)
// ---------------------------------------------------------------------------

/// True when a driver name denotes the USB injection adapter (a WFB-compatible
/// Realtek chip), which is never an onboard management link. Lower-cased compare.
#[cfg(any(target_os = "linux", test))]
fn is_injection_driver(driver: &str) -> bool {
    let d = driver.trim().to_ascii_lowercase();
    INJECTION_DRIVERS.contains(&d.as_str())
}

/// Heuristic for whether a NetworkManager connection name denotes an access
/// point the box hosts (a hotspot the watchdog must NOT re-associate) versus the
/// infrastructure link the box joins. The agent's own hotspot connection name is
/// stable; a generic hotspot / `-ap` suffix is also treated as an AP. Anything
/// else is an infrastructure link. Pure.
#[cfg(any(target_os = "linux", test))]
fn looks_like_access_point(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "ados-hotspot"
        || n.contains("hotspot")
        || n.ends_with("-ap")
        || n.ends_with(" ap")
        || n == "ap"
}

/// Split one terse `nmcli` line into its fields on unescaped colons, unescaping
/// the `\:` and `\\` sequences nmcli uses for literal colons / backslashes
/// inside a field. Pure so the field parsing is unit-tested without nmcli.
#[cfg(any(target_os = "linux", test))]
fn split_terse_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                } else {
                    cur.push('\\');
                }
            }
            ':' => fields.push(std::mem::take(&mut cur)),
            other => cur.push(other),
        }
    }
    fields.push(cur);
    fields
}

/// Parse `nmcli -t -f NAME,TYPE,DEVICE,STATE connection show` terse output into
/// the onboard managed-WiFi candidates: an active `802-11-wireless` connection,
/// bound to a device, not an access point. Wired and non-active connections are
/// dropped. The interface-level exclusions (injection driver, monitor mode) are
/// applied by the caller, which can read sysfs / `iw`; this pure pass keeps only
/// the connection-level shape.
#[cfg(any(target_os = "linux", test))]
fn parse_active_wifi_connections(
    terse: &str,
    is_access_point: impl Fn(&str) -> bool,
) -> Vec<WifiConnection> {
    let mut out = Vec::new();
    for line in terse.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = split_terse_line(line);
        // NAME:TYPE:DEVICE:STATE — a malformed short line is skipped.
        let (Some(name), Some(ctype), Some(device), Some(state)) =
            (fields.first(), fields.get(1), fields.get(2), fields.get(3))
        else {
            continue;
        };
        if name.is_empty() || ctype != "802-11-wireless" {
            continue;
        }
        // Only an activated connection is a candidate: a defined-but-down profile
        // is NetworkManager's job to bring up, not ours to re-associate.
        if state.trim() != "activated" {
            continue;
        }
        let dev = device.trim();
        if dev.is_empty() || dev == "--" {
            continue;
        }
        // The link the box hosts (a hotspot) is never re-associated; we rebuild
        // only the link the box USES to reach the LAN.
        if is_access_point(name) {
            continue;
        }
        out.push(WifiConnection {
            name: name.to_string(),
            iface: dev.to_string(),
        });
    }
    out
}

/// Parse the gateway out of `ip -4 route show default dev <iface>` output: the
/// `via <gw>` token on the `default` line. Returns the gateway IP, or `None`
/// when there is no default route on that interface. Pure.
#[cfg(any(target_os = "linux", test))]
fn parse_gateway(text: &str) -> Option<String> {
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() == Some(&"default") {
            if let Some(idx) = parts.iter().position(|p| *p == "via") {
                if let Some(gw) = parts.get(idx + 1) {
                    return Some((*gw).to_string());
                }
            }
        }
    }
    None
}

/// Parse the neighbor (ARP) reachability for a gateway out of
/// `ip neighbor show <gw> dev <iface>` output. The line ends with the neighbor
/// state token (REACHABLE / STALE / DELAY / PROBE / INCOMPLETE / FAILED). A
/// reachable data path resolves the gateway to a MAC with a usable state
/// (REACHABLE / STALE / DELAY / PROBE — the kernel has a cached entry it is
/// using); INCOMPLETE / FAILED or an absent entry means the gateway does not
/// answer ARP, i.e. the dead-data-path condition. Pure.
#[cfg(any(target_os = "linux", test))]
fn parse_neighbor_reachable(text: &str) -> bool {
    for line in text.lines() {
        let upper = line.to_ascii_uppercase();
        // A usable cached neighbor: the kernel has (or is actively refreshing) a
        // MAC for the gateway. STALE is reachable — it just means the entry has
        // not been confirmed recently; traffic flows and revalidates it.
        if upper.contains("REACHABLE")
            || upper.contains("STALE")
            || upper.contains("DELAY")
            || upper.contains("PROBE")
        {
            return true;
        }
    }
    false
}

/// Decide whether an interface (given its driver and current mode) is an onboard
/// managed-WiFi candidate. Excludes the injection adapter (WFB-compatible
/// driver) and anything not in managed/station mode (a monitor-mode iface is the
/// radio adapter and must never be touched). `mode` is the `iw` operating mode
/// string, or `None` when unreadable — an unreadable mode is treated as NOT a
/// candidate (fail safe: never act on an interface we cannot positively confirm
/// is a managed station). Pure.
#[cfg(any(target_os = "linux", test))]
fn iface_is_managed_candidate(driver: &str, mode: Option<&str>) -> bool {
    if is_injection_driver(driver) {
        return false;
    }
    match mode {
        Some(m) => {
            let m = m.trim().to_ascii_lowercase();
            m == "managed" || m == "station"
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Linux OS edges
// ---------------------------------------------------------------------------

/// Enumerate onboard managed-WiFi candidates: active `802-11-wireless`
/// connections from nmcli, filtered to interfaces that run a non-injection
/// driver and are in managed/station mode (so the monitor-mode radio adapter is
/// excluded three ways: it is not usually a managed connection, it runs a WFB
/// driver, and it is in monitor mode).
#[cfg(target_os = "linux")]
async fn enumerate_candidates() -> Vec<WifiConnection> {
    let Some(terse) = run_cmd_output(
        "nmcli",
        &["-t", "-f", "NAME,TYPE,DEVICE,STATE", "connection", "show"],
    )
    .await
    else {
        return Vec::new();
    };
    let conns = parse_active_wifi_connections(&terse, looks_like_access_point);
    let mut out = Vec::new();
    for conn in conns {
        let driver = driver_name(&conn.iface).await;
        let mode = interface_mode(&conn.iface).await;
        if iface_is_managed_candidate(&driver, mode.as_deref()) {
            out.push(conn);
        }
    }
    out
}

/// Read the kernel driver bound to an interface from
/// `/sys/class/net/<if>/device/driver` (a symlink ending with the driver name).
/// Empty when it cannot be read.
#[cfg(target_os = "linux")]
async fn driver_name(iface: &str) -> String {
    let link = format!("/sys/class/net/{}/device/driver", iface);
    tokio::fs::read_link(&link)
        .await
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
        .unwrap_or_default()
}

/// Read an interface's operating mode ("managed" | "monitor" | …) from
/// `iw <iface> info`, or `None` when it cannot be read.
#[cfg(target_os = "linux")]
async fn interface_mode(iface: &str) -> Option<String> {
    let out = run_cmd_output("iw", &[iface, "info"]).await?;
    for line in out.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("type ") {
            let mode = rest.trim();
            if !mode.is_empty() {
                return Some(mode.to_string());
            }
        }
    }
    None
}

/// Return the default-route gateway for an interface, or `None`. Read-only.
#[cfg(target_os = "linux")]
async fn default_gateway_for_iface(iface: &str) -> Option<String> {
    let out = run_cmd_output("ip", &["-4", "route", "show", "default", "dev", iface]).await?;
    parse_gateway(&out)
}

/// Probe whether the gateway is reachable from an interface via the kernel
/// neighbor (ARP) table: a single, cheap, read-only `ip neighbor show <gw> dev
/// <iface>`. Never sends traffic on or reconfigures the radio interface. A
/// missing or INCOMPLETE/FAILED entry means the gateway does not answer ARP (the
/// dead-data-path condition).
#[cfg(target_os = "linux")]
async fn gateway_reachable(iface: &str, gateway: &str) -> bool {
    match run_cmd_output("ip", &["neighbor", "show", gateway, "dev", iface]).await {
        Some(out) => parse_neighbor_reachable(&out),
        None => false,
    }
}

/// Brief pause between the connection down and up so the kernel fully clears the
/// old association + regulatory state before the fresh one forms.
#[cfg(target_os = "linux")]
const REASSOC_SETTLE: Duration = Duration::from_millis(500);

/// Re-activate one NetworkManager connection with the proven down/up cycle. The
/// `down` gracefully tears the association + IP stack down; the `up` re-forms it
/// under the now-settled regulatory domain. Both calls are best-effort: a
/// connection that was not up returns non-zero on `down`, which is fine — the
/// `up` still rebuilds it.
#[cfg(target_os = "linux")]
async fn reactivate_connection(name: &str) {
    let _ = run_cmd("nmcli", &["connection", "down", name]).await;
    tokio::time::sleep(REASSOC_SETTLE).await;
    if !run_cmd("nmcli", &["connection", "up", name]).await {
        tracing::warn!(connection = %name, "wifi_selfheal_up_failed");
    } else {
        tracing::info!(connection = %name, "wifi_selfheal_reactivated");
    }
}

/// True when the `nmcli` binary is on PATH.
#[cfg(target_os = "linux")]
async fn nmcli_available() -> bool {
    run_cmd("sh", &["-c", "command -v nmcli"]).await
}

/// Run a command, returning true on a zero exit. stdout/stderr are discarded.
#[cfg(target_os = "linux")]
async fn run_cmd(cmd: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command and capture stdout as a string, or `None` when it could not be
/// run. A non-zero exit still returns whatever was written to stdout.
#[cfg(target_os = "linux")]
async fn run_cmd_output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ap_is_hotspot(name: &str) -> bool {
        name == "hotspot"
    }

    // ----- config parsing -----

    #[test]
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
        assert_eq!(cfg.cooldown, Duration::from_secs(DEFAULT_COOLDOWN_S));
    }

    #[test]
    fn explicit_disable_is_honored() {
        let cfg = read_config_from("network:\n  wifi_selfheal:\n    enabled: false\n");
        assert!(!cfg.enabled);
    }

    #[test]
    fn explicit_tunables_parse() {
        let body =
            "network:\n  wifi_selfheal:\n    enabled: true\n    fail_threshold: 3\n    cooldown_s: 90\n";
        let cfg = read_config_from(body);
        assert!(cfg.enabled);
        assert_eq!(cfg.fail_threshold, 3);
        assert_eq!(cfg.cooldown, Duration::from_secs(90));
    }

    #[test]
    fn zero_threshold_is_floored_to_one() {
        let cfg = read_config_from("network:\n  wifi_selfheal:\n    fail_threshold: 0\n");
        assert_eq!(cfg.fail_threshold, 1);
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        let cfg = read_config_from(": : : not yaml");
        assert!(cfg.enabled);
        assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
    }

    // ----- candidate classification (connection level) -----

    #[test]
    fn keeps_only_active_infrastructure_wifi() {
        let terse = "\
Wired connection 1:802-3-ethernet:eth0:activated
home:802-11-wireless:wlan0:activated
lo:loopback:lo:activated
";
        let got = parse_active_wifi_connections(terse, ap_is_hotspot);
        assert_eq!(
            got,
            vec![WifiConnection {
                name: "home".to_string(),
                iface: "wlan0".to_string(),
            }]
        );
    }

    #[test]
    fn drops_non_activated_wifi() {
        // A defined-but-down profile is NetworkManager's job, not ours.
        let terse = "\
home:802-11-wireless:wlan0:activated
backup:802-11-wireless::
office:802-11-wireless:wlan0:deactivated
";
        let got = parse_active_wifi_connections(terse, ap_is_hotspot);
        assert_eq!(
            got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["home"]
        );
    }

    #[test]
    fn excludes_access_point_connections() {
        let terse = "\
home:802-11-wireless:wlan0:activated
hotspot:802-11-wireless:wlan0:activated
";
        let got = parse_active_wifi_connections(terse, ap_is_hotspot);
        assert_eq!(
            got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["home"]
        );
    }

    #[test]
    fn handles_escaped_colon_in_connection_name() {
        let terse = "my\\:net:802-11-wireless:wlan0:activated\n";
        let got = parse_active_wifi_connections(terse, ap_is_hotspot);
        assert_eq!(got[0].name, "my:net");
        assert_eq!(got[0].iface, "wlan0");
    }

    #[test]
    fn empty_and_short_lines_skipped() {
        let terse =
            "\n:802-11-wireless:wlan0:activated\nshort\nhome:802-11-wireless:wlan0:activated\n";
        let got = parse_active_wifi_connections(terse, ap_is_hotspot);
        assert_eq!(
            got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["home"]
        );
    }

    #[test]
    fn no_wifi_yields_empty() {
        let terse = "Wired connection 1:802-3-ethernet:eth0:activated\nlo:loopback:lo:activated\n";
        assert!(parse_active_wifi_connections(terse, ap_is_hotspot).is_empty());
    }

    // ----- interface-level exclusion -----

    #[test]
    fn injection_driver_is_never_a_candidate() {
        // The WFB injection adapter (RTL family), even reported in managed mode,
        // is never an onboard management link.
        assert!(!iface_is_managed_candidate("rtl88x2eu", Some("managed")));
        assert!(!iface_is_managed_candidate("8812eu", Some("managed")));
        assert!(!iface_is_managed_candidate("rtl8812au", Some("monitor")));
    }

    #[test]
    fn monitor_mode_iface_is_never_a_candidate() {
        // A monitor-mode interface is the radio adapter; never touch it.
        assert!(!iface_is_managed_candidate("brcmfmac", Some("monitor")));
    }

    #[test]
    fn unreadable_mode_is_not_a_candidate() {
        // Fail safe: never act on an interface whose mode we cannot confirm.
        assert!(!iface_is_managed_candidate("brcmfmac", None));
    }

    #[test]
    fn onboard_managed_wifi_is_a_candidate() {
        assert!(iface_is_managed_candidate("brcmfmac", Some("managed")));
        assert!(iface_is_managed_candidate("aic8800_fdrv", Some("managed")));
        assert!(iface_is_managed_candidate("brcmfmac", Some("station")));
    }

    // ----- gateway + neighbor parsing -----

    #[test]
    fn parses_gateway_from_default_route() {
        let text = "default via 192.168.200.1 proto dhcp src 192.168.200.50 metric 600\n";
        assert_eq!(parse_gateway(text).as_deref(), Some("192.168.200.1"));
    }

    #[test]
    fn no_gateway_when_no_default_route() {
        let text = "192.168.200.0/24 proto kernel scope link src 192.168.200.50\n";
        assert_eq!(parse_gateway(text), None);
    }

    #[test]
    fn neighbor_reachable_states() {
        assert!(parse_neighbor_reachable(
            "192.168.200.1 dev wlan0 lladdr aa:bb:cc:dd:ee:ff REACHABLE\n"
        ));
        assert!(parse_neighbor_reachable(
            "192.168.200.1 dev wlan0 lladdr aa:bb:cc:dd:ee:ff STALE\n"
        ));
        assert!(parse_neighbor_reachable(
            "192.168.200.1 dev wlan0 lladdr aa:bb:cc:dd:ee:ff DELAY\n"
        ));
    }

    #[test]
    fn neighbor_unreachable_states() {
        // INCOMPLETE / FAILED / empty all mean the gateway does not answer ARP.
        assert!(!parse_neighbor_reachable(
            "192.168.200.1 dev wlan0  INCOMPLETE\n"
        ));
        assert!(!parse_neighbor_reachable(
            "192.168.200.1 dev wlan0 lladdr aa:bb:cc:dd:ee:ff FAILED\n"
        ));
        assert!(!parse_neighbor_reachable(""));
    }

    // ----- the threshold + cooldown state machine -----

    fn healer() -> WifiSelfHeal {
        // The emitter points at an absent socket; emits are wait-free no-ops in
        // tests (the state machine under test never inspects shipped events).
        let events = EventEmitter::with_socket("ados-test", "/nonexistent/ados/logd.sock");
        WifiSelfHeal::new(events)
    }

    #[tokio::test]
    async fn single_failure_does_not_heal() {
        let mut h = healer();
        let now = Instant::now();
        let d = h.step("home", false, 2, Duration::from_secs(60), now);
        assert_eq!(
            d,
            HealDecision::Wait {
                consecutive_failures: 1
            }
        );
    }

    #[tokio::test]
    async fn threshold_reached_heals() {
        let mut h = healer();
        let now = Instant::now();
        assert_eq!(
            h.step("home", false, 2, Duration::from_secs(60), now),
            HealDecision::Wait {
                consecutive_failures: 1
            }
        );
        assert_eq!(
            h.step("home", false, 2, Duration::from_secs(60), now),
            HealDecision::Heal {
                consecutive_failures: 2
            }
        );
    }

    #[tokio::test]
    async fn healthy_tick_clears_the_count() {
        let mut h = healer();
        let now = Instant::now();
        h.step("home", false, 2, Duration::from_secs(60), now);
        assert_eq!(
            h.step("home", true, 2, Duration::from_secs(60), now),
            HealDecision::Healthy
        );
        // A fresh failure starts counting from 1 again.
        assert_eq!(
            h.step("home", false, 2, Duration::from_secs(60), now),
            HealDecision::Wait {
                consecutive_failures: 1
            }
        );
    }

    #[tokio::test]
    async fn cooldown_blocks_a_second_heal_then_lifts() {
        let mut h = healer();
        let t0 = Instant::now();
        let cooldown = Duration::from_secs(60);
        // Cross the threshold and heal at t0. The heal resets the running count.
        h.step("home", false, 2, cooldown, t0);
        assert_eq!(
            h.step("home", false, 2, cooldown, t0),
            HealDecision::Heal {
                consecutive_failures: 2
            }
        );
        // Still failing inside the cooldown window: the threshold is re-met on the
        // second failure, but the recent heal owns the cooldown, so it must NOT
        // re-heal. The running count keeps climbing (it is not reset until a heal
        // actually fires) so the moment the cooldown lifts a heal can take.
        let t_in = t0 + Duration::from_secs(10);
        assert_eq!(
            h.step("home", false, 2, cooldown, t_in),
            HealDecision::Wait {
                consecutive_failures: 1
            }
        );
        assert_eq!(
            h.step("home", false, 2, cooldown, t_in),
            HealDecision::Wait {
                consecutive_failures: 2
            }
        );
        // After the cooldown lifts, the next sustained failure heals again. The
        // count carried into this window is 2, so this failing tick (count → 3)
        // crosses the threshold with the cooldown now expired.
        let t_after = t0 + Duration::from_secs(61);
        assert_eq!(
            h.step("home", false, 2, cooldown, t_after),
            HealDecision::Heal {
                consecutive_failures: 3
            }
        );
    }

    #[tokio::test]
    async fn per_connection_state_is_independent() {
        let mut h = healer();
        let now = Instant::now();
        // home fails twice → heals; office stays healthy.
        h.step("home", false, 2, Duration::from_secs(60), now);
        assert_eq!(
            h.step("home", false, 2, Duration::from_secs(60), now),
            HealDecision::Heal {
                consecutive_failures: 2
            }
        );
        assert_eq!(
            h.step("office", true, 2, Duration::from_secs(60), now),
            HealDecision::Healthy
        );
        // office's first failure is still just a Wait at 1.
        assert_eq!(
            h.step("office", false, 2, Duration::from_secs(60), now),
            HealDecision::Wait {
                consecutive_failures: 1
            }
        );
    }

    #[tokio::test]
    async fn prune_drops_stale_connection_state() {
        let mut h = healer();
        let now = Instant::now();
        h.step("home", false, 2, Duration::from_secs(60), now);
        h.step("office", false, 2, Duration::from_secs(60), now);
        assert_eq!(h.states.len(), 2);
        // Only `home` is still a candidate; `office` state must be pruned.
        h.prune(&[WifiConnection {
            name: "home".to_string(),
            iface: "wlan0".to_string(),
        }]);
        assert_eq!(h.states.len(), 1);
        assert!(h.states.contains_key("home"));
    }

    #[test]
    fn access_point_predicate_matches_known_shapes() {
        assert!(looks_like_access_point("ados-hotspot"));
        assert!(looks_like_access_point("ADOS-Hotspot"));
        assert!(looks_like_access_point("my-hotspot"));
        assert!(looks_like_access_point("field-ap"));
        assert!(looks_like_access_point("ap"));
        assert!(!looks_like_access_point("home"));
        assert!(!looks_like_access_point("Ajay & Nidhi"));
    }
}
