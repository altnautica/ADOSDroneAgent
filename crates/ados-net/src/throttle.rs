//! Data-cap throttle consumer.
//!
//! Subscribes to the uplink event bus and, on each `data_cap_threshold` event,
//! applies the corresponding tc / NAT action via
//! [`ShareUplinkFirewall::apply_throttle`]. This is the glue that turns the
//! cellular data-cap state machine into real bandwidth shaping. In the
//! all-Python agent the same bridge lived in `uplink_router._run_service`'s
//! data-cap consumer.
//!
//! The throttle is applied to the CELLULAR iface, not the router's active
//! uplink: the data-cap meters bytes only on the cellular link, so shaping
//! anything else (a wired primary, a Wi-Fi client) would punish the wrong
//! interface. And because the tc qdisc lives on the cellular iface, an
//! `uplink_changed` that fails the active uplink OFF that cellular iface must
//! clear the now-stale qdisc from it (and re-applying on a later threshold
//! re-targets the live cellular iface) — otherwise the departed iface runs
//! shaped forever and the new uplink runs unshaped.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;
use tracing::{debug, warn};

use crate::firewall::ShareUplinkFirewall;
use crate::router::events::{UplinkEvent, UplinkEventKind};
use crate::router::UplinkRouter;

/// Run the throttle consume loop until the bus closes.
///
/// `cellular_iface` resolves the modem's CURRENT metered iface (`Some(iface)`
/// when a real cellular iface exists, `None` when there is no modem) — the same
/// iface the data-cap source counts bytes on — re-read on each event so a modem
/// appearing/disappearing re-targets the action.
///
/// On a `data_cap_threshold` event the corresponding tc/NAT action is applied
/// to that cellular iface, and the iface is remembered as the one carrying the
/// throttle. On an `uplink_changed` event, if the active uplink is no longer the
/// throttled cellular iface, the stale qdisc is cleared from the iface it was
/// applied to so a post-failover link is never left shaped.
///
/// The caller passes a `Receiver` obtained from `bus.subscribe()` *before*
/// spawning this loop, so an event published right after the spawn is not lost
/// to the broadcast channel's "delivered only to existing receivers" rule.
pub async fn run_throttle_consumer<F>(
    mut rx: Receiver<UplinkEvent>,
    router: Arc<UplinkRouter>,
    firewall: Arc<ShareUplinkFirewall>,
    cellular_iface: F,
) where
    F: Fn() -> Option<String> + Send,
{
    // The iface the throttle qdisc was last applied to, so a later failover can
    // clear it from exactly that iface even if the cellular iface has since
    // changed or vanished.
    let mut throttled_iface: Option<String> = None;

    loop {
        match rx.recv().await {
            Ok(evt) => match evt.kind {
                UplinkEventKind::DataCapThreshold => {
                    let Some(state) = evt.data_cap_state else {
                        continue;
                    };
                    // Record the level on the active-uplink sidecar so a reader
                    // of `/run/ados/uplink-active` learns the throttle level too.
                    router.set_data_cap_state(state).await;
                    // Shape the metered CELLULAR iface, not the active uplink.
                    let iface = cellular_iface();
                    let result = firewall.apply_throttle(iface.as_deref(), state).await;
                    // Remember where the qdisc landed so a later failover clears
                    // exactly this iface.
                    throttled_iface = iface.clone();
                    debug!(state = ?state, iface = ?iface, result = %result, "uplink.throttle_applied");
                }
                UplinkEventKind::UplinkChanged => {
                    // The qdisc lives on the cellular iface. When the active
                    // uplink is no longer that iface, the throttle is stale on a
                    // link that is no longer the cellular cap'd one — clear it so
                    // the departed iface is not left shaped. NAT migration is the
                    // share-uplink consumer's job, so this only touches tc.
                    let Some(stale) = throttled_iface.clone() else {
                        continue;
                    };
                    let active = evt.active_uplink.as_deref();
                    if active != Some(stale.as_str()) {
                        let result = firewall.clear_throttle(&stale).await;
                        debug!(iface = %stale, active = ?active, result = %result, "uplink.throttle_cleared_on_failover");
                        throttled_iface = None;
                    }
                }
                UplinkEventKind::HealthChanged => {}
            },
            // A slow consumer skipped events; keep going on the newest.
            Err(RecvError::Lagged(skipped)) => {
                warn!(skipped = skipped, "uplink.throttle_consumer_lagged");
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// Reconcile the share-uplink NAT against the configured flag at startup, then
/// re-apply it on every uplink switch.
///
/// The NAT MASQUERADE rule is scoped to the active uplink's interface, so when
/// the router fails over to a different uplink the rule must move to the new
/// iface or the shared-uplink clients lose their route. The daemon owns this
/// (the REST share-uplink write path only persists the flag), so this consumer
/// applies it once on start and re-applies on each `uplink_changed` event the
/// router emits — the same drop-on-lag broadcast contract the throttle consumer
/// uses. `read_flag` reads the configured `share_uplink` off the same on-disk
/// config the daemon already loads, re-read on each event so an operator toggle
/// lands without a restart.
///
/// The caller passes a `Receiver` obtained from `bus.subscribe()` *before*
/// spawning this loop, so an `uplink_changed` published right after the spawn is
/// not lost.
pub async fn run_share_uplink_consumer<F>(
    mut rx: Receiver<UplinkEvent>,
    router: Arc<UplinkRouter>,
    firewall: Arc<ShareUplinkFirewall>,
    read_flag: F,
) where
    F: Fn() -> bool + Send,
{
    // Reconcile once at start: bring runtime NAT into agreement with the
    // persisted flag on the iface the router has already selected (if any).
    let iface = router.active_iface().await;
    let result = firewall
        .reconcile_on_start(read_flag(), iface.as_deref())
        .await;
    debug!(iface = ?iface, result = %result, "share_uplink.reconciled_on_start");

    loop {
        match rx.recv().await {
            Ok(evt) => {
                // Only an uplink switch moves the active iface; a health or
                // data-cap event does not change which iface the NAT rule lives
                // on, so they are ignored here.
                if evt.kind != UplinkEventKind::UplinkChanged {
                    continue;
                }
                let enabled = read_flag();
                let iface = router.active_iface().await;
                let result = firewall.apply_share_uplink(enabled, iface.as_deref()).await;
                debug!(enabled = enabled, iface = ?iface, result = %result, "share_uplink.reapplied_on_switch");
            }
            Err(RecvError::Lagged(skipped)) => {
                warn!(skipped = skipped, "uplink.share_uplink_consumer_lagged");
            }
            Err(RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::testing::ScriptedRunner;
    use crate::cmd::CmdOut;
    use crate::firewall::{BackendDetector, FirewallBackend};
    use crate::router::active_flag::ActiveFlagWriter;
    use crate::router::events::DataCapState;
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
    impl RouteApplier for NoopRoute {
        fn apply(&self, _iface: &str, _gateway: Option<&str>) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn data_cap_event_throttles_the_cellular_iface_not_the_active_uplink() {
        let dir = tempfile::tempdir().unwrap();
        // Router whose ACTIVE uplink is a wired eth0 (injected prober/route so no
        // network). The metered cellular iface is a separate wwan0.
        let mut managers: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
        managers.insert("eth0".to_string(), Arc::new(AlwaysUp));
        let router = Arc::new(UplinkRouter::with_seams(
            managers,
            Some(vec!["eth0".to_string()]),
            Some(dir.path().join("uplink.cfg.json")),
            Arc::new(OkProber),
            Arc::new(NoopRoute),
            ActiveFlagWriter::with_path(dir.path().join("uplink-active")),
        ));
        // First tick picks eth0 so the active uplink is the wired primary.
        router.tick().await;
        assert_eq!(router.active_iface().await.as_deref(), Some("eth0"));

        // Firewall over a scripted runner; throttle_95 → tc del + tc add.
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut::failed(0, "")); // tc del
        runner.push(CmdOut::failed(0, "")); // tc add
        let firewall = Arc::new(ShareUplinkFirewall::with_parts(
            runner.clone(),
            Arc::new(FixedBackend(FirewallBackend::IptablesRuntime)),
            dir.path().join("sysctl.conf"),
            dir.path().join("rules.v4"),
            dir.path().join("nft.conf"),
        ));

        // Subscribe BEFORE publishing so the event is not lost to the spawn race.
        let bus = router.bus();
        let rx = bus.subscribe();
        let consumer = tokio::spawn(run_throttle_consumer(
            rx,
            Arc::clone(&router),
            Arc::clone(&firewall),
            // The metered cellular iface is wwan0, NOT the active eth0.
            || Some("wwan0".to_string()),
        ));

        bus.publish(UplinkEvent {
            kind: UplinkEventKind::DataCapThreshold,
            active_uplink: None,
            available: Vec::new(),
            internet_reachable: true,
            data_cap_state: Some(DataCapState::Throttle95),
            timestamp_ms: 1,
        });

        // Poll for the tc tbf throttle.
        for _ in 0..400 {
            if runner
                .recorded()
                .iter()
                .any(|c| c.contains(&"tbf".to_string()))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let calls = runner.recorded();
        // The tbf qdisc must be on the CELLULAR wwan0, never the active eth0.
        let tbf = calls
            .iter()
            .find(|c| c.contains(&"tbf".to_string()))
            .unwrap_or_else(|| panic!("expected a tc tbf throttle, got {calls:?}"));
        assert!(
            tbf.contains(&"wwan0".to_string()),
            "throttle must shape the cellular wwan0, got {tbf:?}"
        );
        assert!(
            !tbf.contains(&"eth0".to_string()),
            "throttle must NOT shape the active eth0, got {tbf:?}"
        );
        consumer.abort();
    }

    #[tokio::test]
    async fn failover_off_the_cellular_iface_clears_the_stale_qdisc() {
        // A data-cap throttle lands on the cellular wwan0. A later failover moves
        // the active uplink to eth0; the now-stale qdisc on wwan0 must be cleared
        // (a `tc qdisc del dev wwan0 root`), or the departed cellular link runs
        // shaped forever while the new uplink runs unshaped.
        let dir = tempfile::tempdir().unwrap();
        let mut managers: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
        managers.insert("eth0".to_string(), Arc::new(AlwaysUp));
        let router = Arc::new(UplinkRouter::with_seams(
            managers,
            Some(vec!["eth0".to_string()]),
            Some(dir.path().join("uplink.cfg.json")),
            Arc::new(OkProber),
            Arc::new(NoopRoute),
            ActiveFlagWriter::with_path(dir.path().join("uplink-active")),
        ));
        router.tick().await;

        // Firewall scripted for: throttle_95 (tc del + tc add) then the
        // failover clear (one tc del).
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut::failed(0, "")); // throttle: tc del (pre-clean)
        runner.push(CmdOut::failed(0, "")); // throttle: tc add tbf
        runner.push(CmdOut::failed(0, "")); // failover clear: tc del
        let firewall = Arc::new(ShareUplinkFirewall::with_parts(
            runner.clone(),
            Arc::new(FixedBackend(FirewallBackend::IptablesRuntime)),
            dir.path().join("sysctl.conf"),
            dir.path().join("rules.v4"),
            dir.path().join("nft.conf"),
        ));

        let bus = router.bus();
        let rx = bus.subscribe();
        let consumer = tokio::spawn(run_throttle_consumer(
            rx,
            Arc::clone(&router),
            Arc::clone(&firewall),
            || Some("wwan0".to_string()),
        ));

        // 1) Throttle lands on wwan0.
        bus.publish(UplinkEvent {
            kind: UplinkEventKind::DataCapThreshold,
            active_uplink: None,
            available: Vec::new(),
            internet_reachable: true,
            data_cap_state: Some(DataCapState::Throttle95),
            timestamp_ms: 1,
        });
        for _ in 0..400 {
            if runner
                .recorded()
                .iter()
                .any(|c| c.contains(&"tbf".to_string()))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let after_throttle = runner.recorded().len();

        // 2) Failover: the active uplink moves to eth0 (not the throttled wwan0).
        bus.publish(UplinkEvent {
            kind: UplinkEventKind::UplinkChanged,
            active_uplink: Some("eth0".to_string()),
            available: vec!["eth0".to_string()],
            internet_reachable: true,
            data_cap_state: None,
            timestamp_ms: 2,
        });

        // The consumer must emit a `tc qdisc del dev wwan0 root` clearing the
        // stale qdisc off the departed cellular iface.
        let mut cleared = false;
        for _ in 0..400 {
            cleared = runner.recorded().iter().skip(after_throttle).any(|c| {
                c.contains(&"del".to_string())
                    && c.contains(&"wwan0".to_string())
                    && c.contains(&"root".to_string())
            });
            if cleared {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            cleared,
            "failover off the cellular iface must clear the stale wwan0 qdisc, got {:?}",
            runner.recorded()
        );
        consumer.abort();
    }

    #[tokio::test]
    async fn failover_that_keeps_the_cellular_iface_active_does_not_clear() {
        // If the cellular iface is STILL the active uplink after an uplink_changed
        // (e.g. another iface dropped), the throttle is not stale and must NOT be
        // cleared.
        let dir = tempfile::tempdir().unwrap();
        let mut managers: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
        managers.insert("eth0".to_string(), Arc::new(AlwaysUp));
        let router = Arc::new(UplinkRouter::with_seams(
            managers,
            Some(vec!["eth0".to_string()]),
            Some(dir.path().join("uplink.cfg.json")),
            Arc::new(OkProber),
            Arc::new(NoopRoute),
            ActiveFlagWriter::with_path(dir.path().join("uplink-active")),
        ));
        router.tick().await;

        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut::failed(0, "")); // throttle: tc del (pre-clean)
        runner.push(CmdOut::failed(0, "")); // throttle: tc add tbf
        let firewall = Arc::new(ShareUplinkFirewall::with_parts(
            runner.clone(),
            Arc::new(FixedBackend(FirewallBackend::IptablesRuntime)),
            dir.path().join("sysctl.conf"),
            dir.path().join("rules.v4"),
            dir.path().join("nft.conf"),
        ));

        let bus = router.bus();
        let rx = bus.subscribe();
        let consumer = tokio::spawn(run_throttle_consumer(
            rx,
            Arc::clone(&router),
            Arc::clone(&firewall),
            || Some("wwan0".to_string()),
        ));

        bus.publish(UplinkEvent {
            kind: UplinkEventKind::DataCapThreshold,
            active_uplink: None,
            available: Vec::new(),
            internet_reachable: true,
            data_cap_state: Some(DataCapState::Throttle95),
            timestamp_ms: 1,
        });
        for _ in 0..400 {
            if runner
                .recorded()
                .iter()
                .any(|c| c.contains(&"tbf".to_string()))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let after_throttle = runner.recorded().len();

        // Failover whose active uplink is STILL wwan0 → no clear.
        bus.publish(UplinkEvent {
            kind: UplinkEventKind::UplinkChanged,
            active_uplink: Some("wwan0".to_string()),
            available: vec!["wwan0".to_string()],
            internet_reachable: true,
            data_cap_state: None,
            timestamp_ms: 2,
        });
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert_eq!(
            runner.recorded().len(),
            after_throttle,
            "an uplink_changed that keeps wwan0 active must not clear its qdisc, got {:?}",
            runner.recorded()
        );
        consumer.abort();
    }

    #[tokio::test]
    async fn share_uplink_consumer_reapplies_nat_on_uplink_switch() {
        let dir = tempfile::tempdir().unwrap();
        let mut managers: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
        managers.insert("eth0".to_string(), Arc::new(AlwaysUp));
        let router = Arc::new(UplinkRouter::with_seams(
            managers,
            Some(vec!["eth0".to_string()]),
            Some(dir.path().join("uplink.cfg.json")),
            Arc::new(OkProber),
            Arc::new(NoopRoute),
            ActiveFlagWriter::with_path(dir.path().join("uplink-active")),
        ));
        // First tick picks eth0 so active_iface() resolves before the consumer
        // reconciles on start.
        router.tick().await;
        assert_eq!(router.active_iface().await.as_deref(), Some("eth0"));

        // Firewall over a scripted runner. The reconcile-on-start applies the
        // enabled flag on eth0, and the uplink_changed re-apply does it again,
        // so script enough responses for two apply passes: each pass runs
        // `sysctl -w`, then `-C` (absent → rc!=0), then `-A` (add ok).
        let runner = Arc::new(ScriptedRunner::new());
        for _ in 0..2 {
            runner.push(CmdOut::failed(0, "")); // sysctl -w ok
            runner.push(CmdOut::failed(1, "")); // -C present? rc!=0 → absent
            runner.push(CmdOut::failed(0, "")); // -A add ok
        }
        let firewall = Arc::new(ShareUplinkFirewall::with_parts(
            runner.clone(),
            Arc::new(FixedBackend(FirewallBackend::IptablesRuntime)),
            dir.path().join("sysctl.conf"),
            dir.path().join("rules.v4"),
            dir.path().join("nft.conf"),
        ));

        // Subscribe BEFORE publishing so the switch event is not lost.
        let bus = router.bus();
        let rx = bus.subscribe();
        let consumer = tokio::spawn(run_share_uplink_consumer(
            rx,
            Arc::clone(&router),
            Arc::clone(&firewall),
            || true, // share_uplink enabled
        ));

        // Publish an uplink_changed; the consumer re-applies the MASQUERADE add.
        bus.publish(UplinkEvent {
            kind: UplinkEventKind::UplinkChanged,
            active_uplink: Some("eth0".to_string()),
            available: vec!["eth0".to_string()],
            internet_reachable: true,
            data_cap_state: None,
            timestamp_ms: 2,
        });

        // Poll for at least two MASQUERADE -A adds (one from reconcile-on-start,
        // one from the switch re-apply).
        for _ in 0..400 {
            let adds = runner
                .recorded()
                .iter()
                .filter(|c| c.contains(&"-A".to_string()) && c.contains(&"MASQUERADE".to_string()))
                .count();
            if adds >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let adds = runner
            .recorded()
            .iter()
            .filter(|c| c.contains(&"-A".to_string()) && c.contains(&"MASQUERADE".to_string()))
            .count();
        assert!(
            adds >= 2,
            "expected a MASQUERADE add on reconcile AND on the switch, got {:?}",
            runner.recorded()
        );
        consumer.abort();
    }

    #[tokio::test]
    async fn share_uplink_consumer_ignores_non_switch_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut managers: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
        managers.insert("eth0".to_string(), Arc::new(AlwaysUp));
        let router = Arc::new(UplinkRouter::with_seams(
            managers,
            Some(vec!["eth0".to_string()]),
            Some(dir.path().join("uplink.cfg.json")),
            Arc::new(OkProber),
            Arc::new(NoopRoute),
            ActiveFlagWriter::with_path(dir.path().join("uplink-active")),
        ));
        router.tick().await;

        // The flag is OFF, so reconcile-on-start removes NAT (sysctl=0 + a -C
        // present? probe). After that, a data-cap event must NOT trigger another
        // apply pass.
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut::failed(0, "")); // sysctl -w 0 ok
        runner.push(CmdOut::failed(1, "")); // -C present? rc!=0 → absent → no -D
        let firewall = Arc::new(ShareUplinkFirewall::with_parts(
            runner.clone(),
            Arc::new(FixedBackend(FirewallBackend::IptablesRuntime)),
            dir.path().join("sysctl.conf"),
            dir.path().join("rules.v4"),
            dir.path().join("nft.conf"),
        ));

        let bus = router.bus();
        let rx = bus.subscribe();
        let consumer = tokio::spawn(run_share_uplink_consumer(
            rx,
            Arc::clone(&router),
            Arc::clone(&firewall),
            || false, // share_uplink disabled
        ));

        // Give the reconcile-on-start a moment to run, then publish a non-switch
        // event and confirm no further sysctl/iptables calls land.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let calls_before = runner.recorded().len();
        bus.publish(UplinkEvent {
            kind: UplinkEventKind::DataCapThreshold,
            active_uplink: Some("eth0".to_string()),
            available: vec!["eth0".to_string()],
            internet_reachable: true,
            data_cap_state: Some(DataCapState::Warn80),
            timestamp_ms: 3,
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            runner.recorded().len(),
            calls_before,
            "a non-switch event must not re-apply the share-uplink NAT"
        );
        consumer.abort();
    }
}
