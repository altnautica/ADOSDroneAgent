//! The fleet attention reconcile tick.
//!
//! Three jobs, all cheap, all driven off the same registry read:
//!
//! 1. **Auto-promote a one-drone fleet.** Every drone boots to `thumbnail` so a
//!    fleet powering up together cannot each grab 48% of the shared channel. A
//!    single-drone fleet is the existing product and has always streamed full
//!    video, so leaving it at 320x180 waiting for an operator click would be a
//!    plain regression — with exactly one registered slot, that drone IS the
//!    hero.
//! 2. **Re-issue an unconfirmed assignment.** A drone that would not answer
//!    during a hero selection is chased here instead of blocking that
//!    selection.
//! 3. **Hold the fleet to the selection.** "Answered once" is not "still
//!    running": a hero whose video service restarts (a battery swap, a reboot,
//!    a crash restart, a re-bind) comes back as a thumbnail and nothing on this
//!    side would ever know. Each drone's swarm beacon carries its hero bit, so
//!    the tick compares the profile each drone actually beacons against the one
//!    the selection wants and re-issues on any disagreement. A hero that is not
//!    being heard at all is re-asserted on the holdoff instead, so a ground
//!    station without a swarm table still recovers it.
//!
//! Only outstanding work is issued: a fleet that agrees with the current
//! selection costs zero radio traffic, which is why this can tick every few
//! seconds against 24 drones without eating the aux lane.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_groundlink::FleetSlot;
use ados_protocol::aux_rpc_proxy::AuxRpcProxy;
use ados_video::profile::VideoProfile;
use serde_json::Value;

use super::fanout::{apply_plan, plan_hero, sole_slot_hero, HeroPlan, HeroTarget};
use super::select::run_selection;
use super::{
    fleet_hero_state, hero_sidecar, profile_caller, publish_hero_to, FleetHeroState,
    HERO_RECONCILE_INTERVAL,
};
use crate::ipc::SwarmIpcClient;
use crate::routes::gs_fleet_slot::registered_slots;

/// The oldest a beacon observation may be and still count as what a drone is
/// running now. The swarm bus drops a neighbour it has not heard for this long,
/// and a table that has not been republished for this long means the bus
/// itself has stopped, leaving every row frozen at its last age.
pub const OBSERVATION_MAX_AGE: Duration = ados_swarm::NEIGHBOR_STALE;

/// What one reconcile tick should do, given the fleet and what is outstanding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    /// Nothing outstanding — the common case, and it costs no radio traffic.
    Idle,
    /// Promote a one-drone fleet's only drone (and demote nobody, because there
    /// is nobody else).
    AutoPromote { hero: String, plan: HeroPlan },
    /// Re-issue the assignments that are unconfirmed or that a drone's beacon
    /// contradicts.
    Retry { plan: HeroPlan },
}

/// The profile each registered drone is beaconing right now, keyed by device id.
///
/// Read from the neighbour table the swarm bus publishes. A row joins to the
/// registry by SLOT, the beacon's own sender field; rows that are stale, carry
/// no hero bit, or sit on a slot nobody is registered to say nothing about any
/// drone and are skipped. A drone absent from the result is simply not being
/// heard.
pub fn observed_profiles(
    published: Option<&Value>,
    slots: &[FleetSlot],
) -> BTreeMap<String, VideoProfile> {
    let Some(rows) = published
        .and_then(|p| p.get("neighbors"))
        .and_then(Value::as_array)
    else {
        return BTreeMap::new();
    };
    let max_age_ms = OBSERVATION_MAX_AGE.as_millis() as u64;
    rows.iter()
        .filter_map(|row| {
            let slot = u8::try_from(row.get("slot")?.as_u64()?).ok()?;
            let hero = row.get("hero")?.as_bool()?;
            let age_ms = row.get("age_ms")?.as_u64()?;
            if age_ms > max_age_ms {
                return None;
            }
            let device = slots.iter().find(|s| s.slot == slot)?;
            let profile = if hero {
                VideoProfile::Hero
            } else {
                VideoProfile::Thumbnail
            };
            Some((device.device_id.clone(), profile))
        })
        .collect()
}

/// The tick's whole decision, pure over its inputs so it is testable without a
/// registry file, a swarm table or a radio.
///
/// Auto-promotion takes precedence: a one-drone fleet stuck on `thumbnail` is a
/// regression of the shipped single-drone product and must clear immediately
/// (unless a selection naming that drone is still in flight, which `settling`
/// shows).
/// Otherwise, per registered drone in slot order:
///
/// - an unconfirmed assignment is re-issued unless the beacon already shows it
///   took (the answer was lost, the change was not);
/// - with a selection made, a drone whose beacon contradicts it is re-issued
///   what the selection wants, and a hero that is not being heard is
///   re-asserted; both only once the drone is out of its `settling` holdoff.
pub fn decide_tick(
    slots: &[FleetSlot],
    hero: Option<&str>,
    unconfirmed: &BTreeMap<String, VideoProfile>,
    observed: &BTreeMap<String, VideoProfile>,
    settling: &BTreeSet<String>,
) -> TickAction {
    if let Some(sole) = sole_slot_hero(slots) {
        if hero != Some(sole) && !settling.contains(sole) {
            if let Some(plan) = plan_hero(slots, sole) {
                return TickAction::AutoPromote {
                    hero: sole.to_string(),
                    plan,
                };
            }
        }
    }
    let targets: Vec<HeroTarget> = slots
        .iter()
        .filter_map(|s| {
            let id = &s.device_id;
            let seen = observed.get(id);
            let profile = if let Some(queued) = unconfirmed.get(id) {
                (seen != Some(queued)).then_some(*queued)?
            } else {
                // No selection yet: nothing to hold the fleet to.
                let wanted = if id == hero? {
                    VideoProfile::Hero
                } else {
                    VideoProfile::Thumbnail
                };
                if settling.contains(id) {
                    return None;
                }
                match seen {
                    Some(p) if *p != wanted => wanted,
                    None if wanted == VideoProfile::Hero => wanted,
                    _ => return None,
                }
            };
            Some(HeroTarget {
                slot: s.slot,
                device_id: id.clone(),
                profile,
            })
        })
        .collect();
    if targets.is_empty() {
        TickAction::Idle
    } else {
        TickAction::Retry {
            plan: HeroPlan { targets },
        }
    }
}

/// Run the fleet attention reconciler until the process exits. Ground-station
/// profile only; spawned once at startup beside the relay proxy it uses.
/// `swarm` is the neighbour-table reader whose hero bits are the observed side
/// of the reconcile.
pub async fn run_hero_reconciler(proxy: Arc<AuxRpcProxy>, swarm: SwarmIpcClient) {
    let mut tick = tokio::time::interval(HERO_RECONCILE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let hero_state = fleet_hero_state();
    loop {
        tick.tick().await;
        reconcile_once(&proxy, &swarm, &hero_state).await;
    }
}

/// One tick: read the registry and the swarm table, forget drones that left,
/// act on what is left.
async fn reconcile_once(
    proxy: &Arc<AuxRpcProxy>,
    swarm: &SwarmIpcClient,
    hero_state: &Arc<FleetHeroState>,
) {
    let slots = registered_slots();
    hero_state.prune(&slots).await;
    let now = Instant::now();
    let observed = observed_profiles(swarm.published_within(OBSERVATION_MAX_AGE).as_ref(), &slots);
    let inputs = hero_state.tick_inputs(now).await;
    let caller = profile_caller(Arc::clone(proxy), Arc::from(slots.as_slice()));

    match decide_tick(
        &slots,
        inputs.hero.as_deref(),
        &inputs.unconfirmed,
        &observed,
        &inputs.settling,
    ) {
        TickAction::Idle => {}
        TickAction::AutoPromote { hero, plan } => {
            tracing::info!(
                device_id = %hero,
                "fleet_hero_auto_promote: sole registered drone takes the full video profile"
            );
            run_selection(
                Arc::clone(hero_state),
                slots.clone(),
                plan,
                hero,
                caller,
                hero_sidecar(),
            )
            .await;
        }
        TickAction::Retry { plan } => {
            for t in plan
                .targets
                .iter()
                .filter(|t| !inputs.unconfirmed.contains_key(&t.device_id))
            {
                // A drone that answered but is not running what it was asked
                // for: rebooted, restarted, or promoted by someone else. Worth
                // a line, and rate-bounded by the holdoff.
                tracing::info!(
                    device_id = %t.device_id,
                    wanted = t.profile.as_str(),
                    beaconing = observed.get(&t.device_id).map_or("unheard", |p| p.as_str()),
                    "fleet_hero_reasserting_assignment"
                );
            }
            let Some(generation) = hero_state
                .begin_reissue(&plan, now, inputs.generation)
                .await
            else {
                // A selection began while this pass was deciding; what it
                // decided may contradict it. The next tick decides afresh.
                return;
            };
            let outcomes = apply_plan(&plan, caller).await;
            hero_state.record_reissue(generation, &outcomes).await;
        }
    }

    // Publish whoever the hero is now, once its promotion has confirmed —
    // including one this tick just auto-promoted, which is the whole of a
    // single-drone fleet's selection and would otherwise never reach the
    // fan-out, and one whose failed promotion a retry has since confirmed.
    // Idempotent, so a settled fleet costs one tmpfs read every few seconds
    // and no write; that also means a publish that failed earlier (an
    // unwritable run dir, a full tmpfs) is retried here instead of leaving the
    // operator on the wrong drone until the next selection.
    if let Some(current) = hero_state.confirmed_hero().await {
        publish_hero_to(&hero_sidecar(), &slots, &current);
    }
}

#[cfg(test)]
mod tests;
