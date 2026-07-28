//! The fleet attention reconcile tick.
//!
//! Two jobs, both cheap, both driven off the same registry read:
//!
//! 1. **Auto-promote a one-drone fleet.** Every drone boots to `thumbnail` so a
//!    fleet powering up together cannot each grab 48% of the shared channel. A
//!    single-drone fleet is the existing product and has always streamed full
//!    video, so leaving it at 320x180 waiting for an operator click would be a
//!    plain regression — with exactly one registered slot, that drone IS the
//!    hero.
//! 2. **Re-issue an unconfirmed demotion.** A drone that would not answer during
//!    a hero selection is chased here instead of blocking that selection.
//!
//! Only outstanding work is issued: a fleet that agrees with the current
//! selection costs zero radio traffic, which is why this can tick every few
//! seconds against 24 drones without eating the aux lane.

use std::collections::BTreeMap;
use std::sync::Arc;

use ados_groundlink::{FleetRegistry, FleetSlot, FLEET_REGISTRY_PATH};
use ados_protocol::aux_rpc_proxy::AuxRpcProxy;
use ados_video::profile::VideoProfile;

use super::fanout::{apply_plan, plan_hero, plan_retry, sole_slot_hero, HeroPlan};
use super::{fleet_hero_state, profile_caller, FleetHeroState, HERO_RECONCILE_INTERVAL};

/// What one reconcile tick should do, given the fleet and what is outstanding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    /// Nothing outstanding — the common case, and it costs no radio traffic.
    Idle,
    /// Promote a one-drone fleet's only drone (and demote nobody, because there
    /// is nobody else).
    AutoPromote { hero: String, plan: HeroPlan },
    /// Re-issue the assignments a previous operation could not confirm.
    Retry { plan: HeroPlan },
}

/// The tick's whole decision, pure over its inputs so it is testable without a
/// registry file or a radio.
///
/// Auto-promotion takes precedence: a one-drone fleet stuck on `thumbnail` is a
/// regression of the shipped single-drone product and must clear immediately,
/// whereas an unconfirmed demotion is only an airtime cost.
pub fn decide_tick(
    slots: &[FleetSlot],
    hero: Option<&str>,
    unconfirmed: &BTreeMap<String, VideoProfile>,
) -> TickAction {
    if let Some(sole) = sole_slot_hero(slots) {
        if hero != Some(sole) {
            if let Some(plan) = plan_hero(slots, sole) {
                return TickAction::AutoPromote {
                    hero: sole.to_string(),
                    plan,
                };
            }
        }
    }
    let plan = plan_retry(slots, unconfirmed);
    if plan.is_empty() {
        TickAction::Idle
    } else {
        TickAction::Retry { plan }
    }
}

/// Run the fleet attention reconciler until the process exits. Ground-station
/// profile only; spawned once at startup beside the relay proxy it uses.
pub async fn run_hero_reconciler(proxy: Arc<AuxRpcProxy>) {
    let mut tick = tokio::time::interval(HERO_RECONCILE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        reconcile_once(&proxy, fleet_hero_state()).await;
    }
}

/// One tick: read the registry, forget drones that left, act on what is left.
async fn reconcile_once(proxy: &Arc<AuxRpcProxy>, hero_state: &FleetHeroState) {
    let slots = registered_slots();
    hero_state.prune(&slots).await;
    let (hero, unconfirmed) = hero_state.outstanding().await;

    match decide_tick(&slots, hero.as_deref(), &unconfirmed) {
        TickAction::Idle => {}
        TickAction::AutoPromote { hero, plan } => {
            tracing::info!(
                device_id = %hero,
                "fleet_hero_auto_promote: sole registered drone takes the full video profile"
            );
            let outcomes = apply_plan(&plan, profile_caller(Arc::clone(proxy))).await;
            hero_state.record_selection(&hero, &outcomes).await;
        }
        TickAction::Retry { plan } => {
            let outcomes = apply_plan(&plan, profile_caller(Arc::clone(proxy))).await;
            hero_state.record_retry(&outcomes).await;
        }
    }
}

/// The registered slot table, read fresh. Never cached: `ados-groundlink` owns
/// the writes and a drone can pair at any moment, so a cached copy is stale the
/// instant it matters.
pub(super) fn registered_slots() -> Vec<FleetSlot> {
    FleetRegistry::load(std::path::Path::new(FLEET_REGISTRY_PATH))
        .slots()
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::gs_fleet_hero::tests_support::{outcome, slots};

    #[test]
    fn a_one_slot_fleet_auto_promotes_its_only_drone_to_hero() {
        // The load-bearing case: today's single-drone product boots to
        // `thumbnail` and MUST come back to full video with no operator action.
        let s = slots(&["only"]);
        let action = decide_tick(&s, None, &BTreeMap::new());
        let TickAction::AutoPromote { hero, plan } = action else {
            panic!("expected auto-promotion, got {action:?}");
        };
        assert_eq!(hero, "only");
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].profile, VideoProfile::Hero);

        // Once promoted the tick goes quiet — no repeated radio traffic.
        assert_eq!(
            decide_tick(&s, Some("only"), &BTreeMap::new()),
            TickAction::Idle
        );
    }

    #[test]
    fn a_multi_drone_fleet_is_never_auto_promoted() {
        // With two or more drones the choice is the operator's; guessing one
        // would hand 48% of the channel to an arbitrary aircraft.
        assert_eq!(
            decide_tick(&slots(&["a", "b"]), None, &BTreeMap::new()),
            TickAction::Idle
        );
        assert_eq!(decide_tick(&[], None, &BTreeMap::new()), TickAction::Idle);
    }

    #[test]
    fn the_tick_retries_only_the_drone_that_failed() {
        let s = slots(&["a", "b", "c"]);
        let mut unconfirmed = BTreeMap::new();
        unconfirmed.insert("c".to_string(), VideoProfile::Thumbnail);
        let TickAction::Retry { plan } = decide_tick(&s, Some("a"), &unconfirmed) else {
            panic!("expected a retry");
        };
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].device_id, "c");
    }

    #[tokio::test]
    async fn a_failed_demotion_is_queued_and_clears_once_it_confirms() {
        let st = FleetHeroState::default();
        let s = slots(&["a", "b"]);
        st.record_selection(
            "a",
            &[
                outcome(1, "a", VideoProfile::Hero, true),
                outcome(2, "b", VideoProfile::Thumbnail, false),
            ],
        )
        .await;
        assert_eq!(st.hero().await.as_deref(), Some("a"));
        let (_, unconfirmed) = st.outstanding().await;
        assert_eq!(unconfirmed.get("b"), Some(&VideoProfile::Thumbnail));
        assert!(!unconfirmed.contains_key("a"));

        st.record_retry(&[outcome(2, "b", VideoProfile::Thumbnail, true)])
            .await;
        assert_eq!(
            decide_tick(&s, Some("a"), &st.outstanding().await.1),
            TickAction::Idle
        );
    }

    #[tokio::test]
    async fn a_drone_that_leaves_the_fleet_is_no_longer_chased() {
        let st = FleetHeroState::default();
        st.record_selection("a", &[outcome(2, "gone", VideoProfile::Thumbnail, false)])
            .await;
        st.prune(&slots(&["a"])).await;
        assert!(st.outstanding().await.1.is_empty());
    }
}
