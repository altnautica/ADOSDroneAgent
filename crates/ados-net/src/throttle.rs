//! Firewall reconcile consumer.
//!
//! Subscribes to the uplink event bus and keeps the one [`FirewallIntent`]
//! the firewall reconciles against: the operator's `share_uplink` flag, the
//! router's active iface, the modem's cellular iface and the data-cap level.
//! A `data_cap_threshold` event updates the level, an `uplink_changed` event
//! moves the active iface, and a periodic pass re-reads everything, so the
//! shaping and NAT a failover or a lost qdisc disturbed are restored without
//! waiting for the next cap transition.
//!
//! The shaping always targets the CELLULAR iface: the data cap meters bytes on
//! the cellular link only, so it stays shaped whichever uplink is active.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;
use tracing::{debug, warn};

use crate::firewall::{FirewallIntent, ShareUplinkFirewall};
use crate::router::events::{DataCapState, UplinkEvent, UplinkEventKind};
use crate::router::UplinkRouter;

/// Cadence of the level-triggered pass that runs between events.
pub const FIREWALL_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Run the firewall reconcile loop until the bus closes.
///
/// `read_flag` reads the configured `share_uplink` and `cellular_iface`
/// resolves the modem's current metered iface (`None` with no modem); both
/// are re-read on every pass. `initial_cap` is the tracker's persisted level,
/// so a restart inside a blocked month never opens NAT on the cellular link
/// before the first poll republishes it.
///
/// The caller passes a `Receiver` obtained from `bus.subscribe()` *before*
/// spawning this loop, so an event published right after the spawn is not lost.
pub async fn run_firewall_reconciler<F, C>(
    mut rx: Receiver<UplinkEvent>,
    router: Arc<UplinkRouter>,
    firewall: Arc<ShareUplinkFirewall>,
    read_flag: F,
    cellular_iface: C,
    initial_cap: DataCapState,
) where
    F: Fn() -> bool + Send,
    C: Fn() -> Option<String> + Send,
{
    let mut cap_state = initial_cap;
    let mut tick = tokio::time::interval(FIREWALL_RECONCILE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            // The first tick fires at once: the reconcile-on-start.
            _ = tick.tick() => {}
            evt = rx.recv() => match evt {
                Ok(evt) => match evt.kind {
                    UplinkEventKind::DataCapThreshold => {
                        let Some(state) = evt.data_cap_state else {
                            continue;
                        };
                        // Record the level on the active-uplink sidecar so a
                        // reader of `/run/ados/uplink-active` learns it too.
                        router.set_data_cap_state(state).await;
                        cap_state = state;
                    }
                    UplinkEventKind::UplinkChanged => {}
                    UplinkEventKind::HealthChanged => continue,
                },
                // A slow consumer skipped events; the pass below re-reads all.
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped = skipped, "uplink.firewall_consumer_lagged");
                }
                Err(RecvError::Closed) => break,
            },
        }
        let intent = FirewallIntent {
            share_uplink: read_flag(),
            active_iface: router.active_iface().await,
            cellular_iface: cellular_iface(),
            cap_state,
        };
        let result = firewall.reconcile(intent).await;
        debug!(result = %result, "uplink.firewall_reconciled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firewall::testing::FakeNet;
    use crate::firewall::{BackendDetector, FirewallBackend, THROTTLE_RATE_KBPS_95};
    use crate::router::active_flag::ActiveFlagWriter;
    use crate::router::{Prober, RouteApplier, UplinkManager};
    use std::collections::HashMap;

    struct FixedBackend(FirewallBackend);
    impl BackendDetector for FixedBackend {
        fn detect(&self) -> FirewallBackend {
            self.0
        }
    }

    struct AlwaysUp;
    #[async_trait::async_trait]
    impl UplinkManager for AlwaysUp {
        async fn is_up(&self) -> bool {
            true
        }
        fn get_iface(&self) -> String {
            "eth0".to_string()
        }
        async fn get_gateway(&self) -> Option<String> {
            None
        }
    }

    struct OkProber;
    #[async_trait::async_trait]
    impl Prober for OkProber {
        async fn probe(&self, _iface: Option<&str>) -> bool {
            true
        }
    }

    struct NoopRoute;
    #[async_trait::async_trait]
    impl RouteApplier for NoopRoute {
        async fn apply(&self, _iface: &str, _gateway: Option<&str>) -> bool {
            true
        }
    }

    fn router(dir: &std::path::Path) -> Arc<UplinkRouter> {
        let mut managers: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
        managers.insert("eth0".to_string(), Arc::new(AlwaysUp));
        Arc::new(UplinkRouter::with_seams(
            managers,
            Some(vec!["eth0".to_string()]),
            Some(dir.join("uplink.cfg.json")),
            Arc::new(OkProber),
            Arc::new(NoopRoute),
            ActiveFlagWriter::with_path(dir.join("uplink-active")),
        ))
    }

    fn firewall(net: &Arc<FakeNet>, dir: &std::path::Path) -> Arc<ShareUplinkFirewall> {
        Arc::new(ShareUplinkFirewall::with_parts(
            Arc::clone(net) as Arc<dyn crate::cmd::CmdRunner>,
            Arc::new(FixedBackend(FirewallBackend::IptablesRuntime)),
            dir.join("sysctl.conf"),
            dir.join("rules.v4"),
            dir.join("ados-nat.nft"),
        ))
    }

    async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..400 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    fn cap_event(state: DataCapState) -> UplinkEvent {
        UplinkEvent {
            kind: UplinkEventKind::DataCapThreshold,
            active_uplink: None,
            available: Vec::new(),
            internet_reachable: true,
            data_cap_state: Some(state),
            timestamp_ms: 1,
        }
    }

    fn switch_event(active: &str) -> UplinkEvent {
        UplinkEvent {
            kind: UplinkEventKind::UplinkChanged,
            active_uplink: Some(active.to_string()),
            available: vec![active.to_string()],
            internet_reachable: true,
            data_cap_state: None,
            timestamp_ms: 2,
        }
    }

    #[tokio::test]
    async fn a_cap_event_shapes_the_cellular_iface_and_a_switch_keeps_it_shaped() {
        let dir = tempfile::tempdir().unwrap();
        let router = router(dir.path());
        router.tick().await;
        assert_eq!(router.active_iface().await.as_deref(), Some("eth0"));
        let net = Arc::new(FakeNet::default());
        let bus = router.bus();
        let rx = bus.subscribe();
        let consumer = tokio::spawn(run_firewall_reconciler(
            rx,
            Arc::clone(&router),
            firewall(&net, dir.path()),
            || false,
            || Some("wwan0".to_string()),
            DataCapState::Ok,
        ));

        bus.publish(cap_event(DataCapState::Throttle95));
        assert!(wait_for(|| net.tbf_rate("wwan0") == Some(THROTTLE_RATE_KBPS_95)).await);
        assert_eq!(
            net.tbf_rate("eth0"),
            None,
            "the active wired uplink is never shaped"
        );

        // An uplink switch never lifts the cap shaping.
        let before = net.calls().len();
        bus.publish(switch_event("eth0"));
        assert!(wait_for(|| net.calls().len() > before).await);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(net.tbf_rate("wwan0"), Some(THROTTLE_RATE_KBPS_95));
        consumer.abort();
    }

    #[tokio::test]
    async fn an_ok_cap_event_never_adds_nat_while_sharing_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let router = router(dir.path());
        router.tick().await;
        let net = Arc::new(FakeNet::default());
        let bus = router.bus();
        let rx = bus.subscribe();
        let consumer = tokio::spawn(run_firewall_reconciler(
            rx,
            Arc::clone(&router),
            firewall(&net, dir.path()),
            || false,
            || Some("wwan0".to_string()),
            DataCapState::Ok,
        ));
        let before = net.calls().len();
        bus.publish(cap_event(DataCapState::Ok));
        assert!(wait_for(|| net.calls().len() > before).await);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(net.nat_ifaces().is_empty());
        consumer.abort();
    }

    #[tokio::test]
    async fn sharing_follows_the_active_uplink_from_start() {
        let dir = tempfile::tempdir().unwrap();
        let router = router(dir.path());
        router.tick().await;
        let net = Arc::new(FakeNet::default());
        let bus = router.bus();
        let rx = bus.subscribe();
        let consumer = tokio::spawn(run_firewall_reconciler(
            rx,
            Arc::clone(&router),
            firewall(&net, dir.path()),
            || true,
            || None,
            DataCapState::Ok,
        ));
        assert!(wait_for(|| net.nat_ifaces() == vec!["eth0".to_string()]).await);
        consumer.abort();
    }
}
