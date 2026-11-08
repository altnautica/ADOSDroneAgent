//! Grant re-checks: what a revoke tears down, and the per-push capability
//! check that keeps a queued frame from outliving its grant.

use super::*;

impl<H: HostServices> Connection<H> {
    /// Drop every subscription the adopted `token` no longer covers.
    ///
    /// A revoke re-mints the token, but a stream armed under the old grant
    /// would keep running: its forwarder pulls frames whatever the token now
    /// says. So each forwarder whose subscribe method needs a capability the
    /// token lacks is aborted, and its subscription state is cleared so a
    /// later re-grant can subscribe again. Event subscriptions are re-checked
    /// pattern by pattern, since a topic can need its own capability.
    pub(super) fn prune_revoked(
        &self,
        token: &CapabilityToken,
        subs: Subscriptions<'_>,
        forwarders: &mut Forwarders,
    ) {
        let holds = |m: Method| {
            m.required_cap()
                .is_none_or(|c| token.granted_caps.contains(c))
        };
        forwarders.abort_where(|m| !holds(m));
        let clear_list = |list: &mut Vec<String>, m: Method| {
            if !holds(m) {
                list.clear();
            }
        };
        clear_list(subs.mavlink, Method::MavlinkSubscribe);
        clear_list(subs.vision, Method::VisionSubscribeFrames);
        clear_list(subs.detections, Method::VisionSubscribeDetections);
        for (flag, m) in [
            (subs.button, Method::ButtonSubscribe),
            (subs.msp, Method::MspSubscribe),
            (subs.aux, Method::RadioAuxStreamSubscribe),
            (subs.display_zone, Method::DisplayZoneSubscribe),
            (subs.telemetry, Method::TelemetrySubscribe),
        ] {
            if !holds(m) {
                *flag = false;
            }
        }
        if holds(Method::EventSubscribe) {
            subs.events.retain(|pattern| {
                handlers::is_subscribe_allowed(
                    &self.plugin_id,
                    pattern,
                    &token.granted_caps,
                    &self.shared_topics,
                )
            });
        } else {
            subs.events.clear();
        }
    }

    /// Whether `token` still carries the capability `method` needs. Every
    /// push re-checks it, so nothing already queued for a stream leaks out
    /// after a revoke.
    pub(super) fn holds(token: &CapabilityToken, method: Method) -> bool {
        method
            .required_cap()
            .is_none_or(|c| token.granted_caps.contains(c))
    }
}
