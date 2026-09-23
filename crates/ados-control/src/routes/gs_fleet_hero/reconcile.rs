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
pub const OBSERVATION_MAX_AGE: Duration = ados_swarmbus::NEIGHBOR_STALE;

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
mod tests {
    use super::*;
    use crate::routes::gs_fleet_hero::tests_support::{outcome, slots};
    use serde_json::json;

    use VideoProfile::{Hero, Thumbnail};

    fn none() -> BTreeMap<String, VideoProfile> {
        BTreeMap::new()
    }

    fn seen(rows: &[(&str, VideoProfile)]) -> BTreeMap<String, VideoProfile> {
        rows.iter().map(|(id, p)| (id.to_string(), *p)).collect()
    }

    fn settled() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn retried(action: TickAction) -> Vec<(String, VideoProfile)> {
        match action {
            TickAction::Retry { plan } => plan
                .targets
                .into_iter()
                .map(|t| (t.device_id, t.profile))
                .collect(),
            other => panic!("expected a retry, got {other:?}"),
        }
    }

    #[test]
    fn a_one_slot_fleet_auto_promotes_its_only_drone_to_hero() {
        // The load-bearing case: today's single-drone product boots to
        // `thumbnail` and MUST come back to full video with no operator action.
        let s = slots(&["only"]);
        let action = decide_tick(&s, None, &none(), &none(), &settled());
        let TickAction::AutoPromote { hero, plan } = action else {
            panic!("expected auto-promotion, got {action:?}");
        };
        assert_eq!(hero, "only");
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].profile, Hero);

        // Once promoted and beaconing it, the tick goes quiet — no repeated
        // radio traffic.
        assert_eq!(
            decide_tick(
                &s,
                Some("only"),
                &none(),
                &seen(&[("only", Hero)]),
                &settled()
            ),
            TickAction::Idle
        );
        // A selection of that drone whose promotion has not answered yet is
        // not raced by a second one.
        let in_flight: BTreeSet<String> = ["only".to_string()].into();
        assert_eq!(
            decide_tick(&s, None, &none(), &none(), &in_flight),
            TickAction::Idle
        );
    }

    #[test]
    fn a_multi_drone_fleet_is_never_auto_promoted() {
        // With two or more drones the choice is the operator's; guessing one
        // would hand 48% of the channel to an arbitrary aircraft.
        assert_eq!(
            decide_tick(&slots(&["a", "b"]), None, &none(), &none(), &settled()),
            TickAction::Idle
        );
        assert_eq!(
            decide_tick(&[], None, &none(), &none(), &settled()),
            TickAction::Idle
        );
    }

    #[test]
    fn the_tick_retries_only_the_drone_that_failed() {
        let s = slots(&["a", "b", "c"]);
        let unconfirmed = seen(&[("c", Thumbnail)]);
        let beacons = seen(&[("a", Hero), ("b", Thumbnail)]);
        assert_eq!(
            retried(decide_tick(
                &s,
                Some("a"),
                &unconfirmed,
                &beacons,
                &settled()
            )),
            vec![("c".to_string(), Thumbnail)]
        );
    }

    #[test]
    fn a_hero_that_restarted_as_a_thumbnail_is_promoted_again() {
        // A battery swap or a video service restart brings the hero back at
        // boot default. Nothing is outstanding, so only its beacon shows it.
        let s = slots(&["a", "b"]);
        let beacons = seen(&[("a", Thumbnail), ("b", Thumbnail)]);
        assert_eq!(
            retried(decide_tick(&s, Some("a"), &none(), &beacons, &settled())),
            vec![("a".to_string(), Hero)]
        );
    }

    #[test]
    fn a_drone_still_beaconing_hero_after_its_demotion_is_demoted_again() {
        let s = slots(&["a", "b"]);
        let beacons = seen(&[("a", Hero), ("b", Hero)]);
        assert_eq!(
            retried(decide_tick(&s, Some("a"), &none(), &beacons, &settled())),
            vec![("b".to_string(), Thumbnail)]
        );
    }

    #[test]
    fn a_fleet_whose_beacons_agree_costs_nothing() {
        let s = slots(&["a", "b", "c"]);
        let beacons = seen(&[("a", Thumbnail), ("b", Hero), ("c", Thumbnail)]);
        assert_eq!(
            decide_tick(&s, Some("b"), &none(), &beacons, &settled()),
            TickAction::Idle
        );
    }

    #[test]
    fn a_drone_that_was_just_sent_an_assignment_is_given_time_to_beacon_it() {
        // The bit reaches the ground a beacon or two after the change; a tick
        // landing in between must not re-send what is already under way.
        let s = slots(&["a", "b"]);
        let beacons = seen(&[("a", Thumbnail), ("b", Hero)]);
        let settling: BTreeSet<String> = ["a".to_string(), "b".to_string()].into();
        assert_eq!(
            decide_tick(&s, Some("a"), &none(), &beacons, &settling),
            TickAction::Idle
        );
    }

    #[test]
    fn an_unheard_hero_is_reasserted_and_an_unheard_thumbnail_is_left_alone() {
        // No beacon to compare against (the drone is out of range, or this
        // node runs no swarm table): the hero is re-asserted on the holdoff,
        // since a rebooted one would otherwise never be noticed; a thumbnail
        // needs nothing, because thumbnail is what a drone boots to.
        let s = slots(&["a", "b"]);
        assert_eq!(
            retried(decide_tick(&s, Some("a"), &none(), &none(), &settled())),
            vec![("a".to_string(), Hero)]
        );
    }

    #[test]
    fn a_queued_assignment_the_beacon_shows_took_is_not_resent() {
        // The answer was lost on the way back; the change was not.
        let s = slots(&["a", "b"]);
        let unconfirmed = seen(&[("b", Thumbnail)]);
        let beacons = seen(&[("a", Hero), ("b", Thumbnail)]);
        assert_eq!(
            decide_tick(&s, Some("a"), &unconfirmed, &beacons, &settled()),
            TickAction::Idle
        );
    }

    #[test]
    fn only_fresh_rows_on_registered_slots_count_as_observed() {
        let s = slots(&["a", "b", "c"]);
        let table = json!({
            "neighbors": [
                {"slot": 1, "hero": true, "age_ms": 400},
                {"slot": 2, "hero": false, "age_ms": OBSERVATION_MAX_AGE.as_millis() as u64 + 1},
                {"slot": 3, "age_ms": 100},
                {"slot": 9, "hero": false, "age_ms": 100},
            ]
        });
        assert_eq!(observed_profiles(Some(&table), &s), seen(&[("a", Hero)]));
        assert!(observed_profiles(None, &s).is_empty());
        assert!(observed_profiles(Some(&json!({})), &s).is_empty());
    }

    #[tokio::test]
    async fn a_failed_demotion_is_queued_and_clears_once_it_confirms() {
        let st = FleetHeroState::default();
        let s = slots(&["a", "b"]);
        let now = Instant::now();
        let generation = st.begin_selection(&plan_hero(&s, "a").unwrap(), now).await;
        st.record_hero(generation, "a", &outcome(1, "a", Hero, true))
            .await;
        st.record_outcome(generation, &outcome(2, "b", Thumbnail, false))
            .await;
        let inputs = st.tick_inputs(now).await;
        assert_eq!(inputs.hero.as_deref(), Some("a"));
        assert_eq!(inputs.unconfirmed.get("b"), Some(&Thumbnail));
        assert!(!inputs.unconfirmed.contains_key("a"));

        let retry = HeroPlan {
            targets: vec![HeroTarget {
                slot: 2,
                device_id: "b".into(),
                profile: Thumbnail,
            }],
        };
        let generation = st
            .begin_reissue(&retry, now, inputs.generation)
            .await
            .expect("no selection has begun since the inputs were read");
        st.record_reissue(generation, &[outcome(2, "b", Thumbnail, true)])
            .await;
        let inputs = st.tick_inputs(now).await;
        assert!(inputs.unconfirmed.is_empty());
        assert_eq!(
            decide_tick(
                &s,
                inputs.hero.as_deref(),
                &inputs.unconfirmed,
                &seen(&[("a", Hero), ("b", Thumbnail)]),
                &inputs.settling,
            ),
            TickAction::Idle
        );
    }

    #[tokio::test]
    async fn the_tick_republishes_a_selection_whose_first_publish_did_not_land() {
        // The tick's publish step, exercised as the tick runs it: read the
        // current hero, publish it. A publish that failed at selection time (an
        // unwritable run dir, a full tmpfs) would otherwise leave the operator
        // on the wrong drone until they made another selection.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-hero.json");
        let s = slots(&["a", "b", "c"]);

        let st = FleetHeroState::default();
        let generation = st
            .begin_selection(&plan_hero(&s, "c").unwrap(), Instant::now())
            .await;
        st.record_hero(generation, "c", &outcome(3, "c", Hero, true))
            .await;
        // Nothing published: the write failed at selection time.
        assert!(ados_groundlink::read_hero_from(&path).is_none());

        let hero = st.confirmed_hero().await.expect("the selection is sticky");
        assert!(publish_hero_to(&path, &s, &hero));
        assert_eq!(ados_groundlink::read_hero_from(&path).unwrap().slot, 3);

        // And the next tick is a no-op, so a healthy fleet costs no writes.
        assert!(!publish_hero_to(&path, &s, &hero));
    }

    #[tokio::test]
    async fn a_drone_that_leaves_the_fleet_is_no_longer_chased() {
        let st = FleetHeroState::default();
        let generation = st
            .begin_selection(&HeroPlan::default(), Instant::now())
            .await;
        st.record_outcome(generation, &outcome(2, "gone", Thumbnail, false))
            .await;
        st.prune(&slots(&["a"])).await;
        assert!(st.tick_inputs(Instant::now()).await.unconfirmed.is_empty());
    }
}
