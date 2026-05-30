//! Data-cap throttle consumer.
//!
//! Subscribes to the uplink event bus and, on each `data_cap_threshold` event,
//! applies the corresponding tc / NAT action to the currently-active uplink
//! interface via [`ShareUplinkFirewall::apply_throttle`]. This is the glue that
//! turns the cellular data-cap state machine into real bandwidth shaping. In
//! the all-Python agent the same bridge lived in `uplink_router._run_service`'s
//! data-cap consumer.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;
use tracing::{debug, warn};

use crate::firewall::ShareUplinkFirewall;
use crate::router::events::{UplinkEvent, UplinkEventKind};
use crate::router::UplinkRouter;

/// Run the throttle consume loop until the bus closes. Resolves the active
/// iface from the router on each event so a failover between threshold events
/// re-targets the action.
///
/// The caller passes a `Receiver` obtained from `bus.subscribe()` *before*
/// spawning this loop, so an event published right after the spawn is not lost
/// to the broadcast channel's "delivered only to existing receivers" rule.
pub async fn run_throttle_consumer(
    mut rx: Receiver<UplinkEvent>,
    router: Arc<UplinkRouter>,
    firewall: Arc<ShareUplinkFirewall>,
) {
    loop {
        match rx.recv().await {
            Ok(evt) => {
                if evt.kind != UplinkEventKind::DataCapThreshold {
                    continue;
                }
                let Some(state) = evt.data_cap_state else {
                    continue;
                };
                // Record the level on the active-uplink sidecar so a reader of
                // `/run/ados/uplink-active` learns the throttle level too.
                router.set_data_cap_state(state).await;
                let iface = router.active_iface().await;
                let result = firewall.apply_throttle(iface.as_deref(), state).await;
                debug!(state = ?state, iface = ?iface, result = %result, "uplink.throttle_applied");
            }
            // A slow consumer skipped events; keep going on the newest.
            Err(RecvError::Lagged(skipped)) => {
                warn!(skipped = skipped, "uplink.throttle_consumer_lagged");
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
    async fn data_cap_event_triggers_a_throttle_action() {
        let dir = tempfile::tempdir().unwrap();
        // Router with an always-up eth0 (injected prober/route so no network).
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
        // First tick picks eth0 so active_iface() is Some("eth0").
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
        ));

        bus.publish(UplinkEvent {
            kind: UplinkEventKind::DataCapThreshold,
            active_uplink: None,
            available: Vec::new(),
            internet_reachable: true,
            data_cap_state: Some(DataCapState::Throttle95),
            timestamp_ms: 1,
        });

        // Poll for the tc tbf throttle on the active iface.
        for _ in 0..50 {
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
        assert!(
            calls.iter().any(|c| c.contains(&"tbf".to_string())),
            "expected a tc tbf throttle on the active iface, got {calls:?}"
        );
        consumer.abort();
    }
}
