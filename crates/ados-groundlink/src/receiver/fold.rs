//! Fold one aggregator stats interval into the published receiver state.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use super::{AggregatorPkt, ReceiverState};

/// Fold one stats line into the published state: the fragment and FEC totals
/// accumulate (the wire counts reset every interval), the output rate is this
/// interval's (stats interval = 1 s).
pub(super) fn fold_aggregator_pkt(state: &mut ReceiverState, pkt: AggregatorPkt) {
    state.fragments_after_dedup = state.fragments_after_dedup.saturating_add(pkt.unique);
    state.fec_repaired = state.fec_repaired.saturating_add(pkt.fec_recovered);
    state.output_kbps = pkt.bytes_out * 8 / 1000;
}

/// Fold one interval's per-source packet counts into the relay map. Each relay
/// MAC is resolved to its mesh IPv4 through the neighbour table `arp`; a
/// resolved relay adds this interval's count (zero when it forwarded nothing),
/// an unresolved one keeps its previous value (`null` until first resolved).
pub(super) fn fold_relay_fragments(
    state: &mut ReceiverState,
    per_source: &HashMap<Ipv4Addr, u64>,
    arp: &HashMap<Ipv4Addr, String>,
) {
    for relay in &mut state.relays {
        let Some(ip) = arp
            .iter()
            .find(|(_, mac)| mac.eq_ignore_ascii_case(&relay.mac))
            .map(|(ip, _)| ip)
        else {
            continue;
        };
        let added = per_source.get(ip).copied().unwrap_or(0) as i64;
        relay.fragments = Some(relay.fragments.unwrap_or(0).saturating_add(added));
    }
}
