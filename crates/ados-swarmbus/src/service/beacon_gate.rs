//! Gating this node's own beacon on snapshot freshness.

use super::*;

/// Age of the FC's position fix as the producer reported it, or `None` when the
/// snapshot does not state one.
///
/// Absence is NOT freshness. A snapshot with no `position_age_ms` is a producer
/// that has never decoded a POSITION (the field is `null` until the first one),
/// and the caller must treat that as an untrusted fix rather than as "no reason
/// to worry" — the whole hazard here is a plausible-looking frozen fix.
pub(super) fn fix_age(snapshot: &StateSnapshot) -> Option<std::time::Duration> {
    snapshot
        .value
        .get("position_age_ms")
        .and_then(Value::as_u64)
        .map(std::time::Duration::from_millis)
}

/// The snapshot this node may broadcast as its own state, paired with whether
/// its position fix may be radiated, or `None` when there is no snapshot fresh
/// enough to stand for it at all. Pure, so both gates are testable without a
/// socket or a radio.
///
/// Two independent ages clear [`ados_swarm::OWN_STATE_STALE`], because they fail
/// independently and mean different things:
///
/// - **Arrival age** (`now - received`) catches the MAVLink router dying. Then
///   nothing in the snapshot is current, so there is no body at all and the
///   beacon goes out carrying only the slot.
/// - **Fix age** (`position_age_ms`) catches the far more likely partial stall:
///   the FC keeps heartbeating, the router keeps publishing at its unconditional
///   cadence, and only the position inside the snapshot stops moving. Every
///   neighbour dead-reckons a beacon's position and velocity FORWARD, so a frozen
///   fix must not be radiated — but the armed, mode, hero, emergency and
///   precedence readings in the same snapshot are still live and still go out.
pub(super) fn beacon_body(
    snapshot: Option<&StateSnapshot>,
    now: Instant,
) -> Option<(&Value, bool)> {
    let snapshot = snapshot?;
    if now.saturating_duration_since(snapshot.received) >= ados_swarm::OWN_STATE_STALE {
        return None;
    }
    // Fail closed on an unstated age: a producer that cannot say how old its fix
    // is has not earned the fleet's trust in that fix.
    let fix_trusted = fix_age(snapshot).is_some_and(|age| age < ados_swarm::OWN_STATE_STALE);
    Some((&snapshot.value, fix_trusted))
}

/// This node's beacon for the snapshot held at `now`: [`beacon_body`]'s two
/// gates applied through [`beacon_from_state`].
pub(super) fn own_beacon(
    snapshot: Option<&StateSnapshot>,
    now: Instant,
    slot: u8,
    seq_ms: u16,
) -> crate::beacon::SwarmBeacon {
    match beacon_body(snapshot, now) {
        Some((value, fix_trusted)) => beacon_from_state(Some(value), fix_trusted, slot, seq_ms),
        None => beacon_from_state(None, false, slot, seq_ms),
    }
}
