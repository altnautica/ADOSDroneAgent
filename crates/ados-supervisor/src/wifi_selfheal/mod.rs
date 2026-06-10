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
//!
//! Module layout:
//! - `config`: `WifiSelfHealConfig` and its parsing.
//! - `decision`: the `WifiConnection` model, the `HealDecision` enum, and the
//!   pure nmcli / gateway / neighbor parsing + candidate classification.
//! - `os`: the Linux OS edges (candidate enumeration, reachability probes,
//!   NetworkManager down/up). The threshold + cooldown state machine lives on
//!   the `WifiSelfHeal` struct below.

use std::collections::HashMap;
use std::time::Instant;

use ados_protocol::logd::emitter::EventEmitter;

pub mod config;
pub mod decision;
pub mod os;

pub use config::{read_config_from, WifiSelfHealConfig};
pub use decision::{HealDecision, WifiConnection};

/// The event kind recorded on each onboard-WiFi re-association. Bland and
/// reader-facing: it names what the code did, not any internal milestone.
pub const REASSOC_KIND: &str = "network.wifi_reassociated";

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
        cooldown: std::time::Duration,
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
        let cfg = config::read_config();
        if !cfg.enabled {
            return;
        }
        if !os::nmcli_available().await {
            return;
        }
        let candidates = os::enumerate_candidates().await;
        self.prune(&candidates);
        if candidates.is_empty() {
            return;
        }
        let now = Instant::now();
        for conn in candidates {
            // Determine the gateway for this connection's interface. No gateway
            // means there is nothing to probe (the link is not the LAN path), so
            // it is not a self-heal candidate this tick — clear and move on.
            let Some(gateway) = os::default_gateway_for_iface(&conn.iface).await else {
                self.states
                    .entry(conn.name.clone())
                    .or_default()
                    .consecutive_failures = 0;
                continue;
            };
            let reachable = os::gateway_reachable(&conn.iface, &gateway).await;
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
                    os::reactivate_connection(&conn.name).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
}
