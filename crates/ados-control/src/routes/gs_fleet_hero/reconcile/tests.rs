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
