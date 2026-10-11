use super::fanout::HeroTarget;
use super::tests_support::{outcome, slots};
use super::*;
use crate::routes::gs_fleet_slot::test_drone::{self, SECRET};

#[tokio::test]
async fn a_promotion_carries_a_ticket_the_drone_admits() {
    // Driven through the drone's own relay authorization, not a stub. A
    // drone refuses a relayed request with no ticket, so a caller that
    // omits it leaves every drone at 320x180 forever.
    const DRONE: &str = "ados-hero-01";
    let drone = test_drone::spawn(DRONE, Some(SECRET)).await;
    let mut registry = slots(&[DRONE]);
    registry[0].relay_secret = Some(SECRET.to_string());

    let promote = profile_caller(Arc::clone(&drone.proxy), Arc::from(registry.as_slice()));
    assert_eq!(
        promote(DRONE.to_string(), VideoProfile::Hero).await,
        Ok(()),
        "the drone must admit the promotion"
    );
    let seen = drone
        .seen
        .lock()
        .last()
        .cloned()
        .expect("the drone was called");
    assert_eq!(seen.method, RpcMethod::Post);
    assert_eq!(seen.path, DRONE_PROFILE_PATH);
    assert_eq!(seen.body, br#"{"profile":"hero"}"#);
    assert_eq!(seen.status, 200);

    // The same request without a ticket is what the drone refuses.
    let bare = drone
        .proxy
        .call(
            DRONE.as_bytes(),
            RpcMethod::Post,
            DRONE_PROFILE_PATH,
            &seen.body,
        )
        .await
        .expect("the drone answers");
    assert_eq!(bare.status, 401);
}

#[tokio::test]
async fn a_promotion_for_a_drone_with_no_secret_on_file_is_refused_and_says_so() {
    // Nothing to mint from, so nothing is sent and the refusal is
    // surfaced rather than read as a success.
    const DRONE: &str = "ados-hero-02";
    let drone = test_drone::spawn(DRONE, Some(SECRET)).await;
    let promote = profile_caller(Arc::clone(&drone.proxy), Arc::from(slots(&[DRONE])));
    let err = promote(DRONE.to_string(), VideoProfile::Hero)
        .await
        .expect_err("no ticket can be minted");
    assert!(err.contains("no relay secret"), "{err}");
    assert!(drone.seen.lock().is_empty());
}

#[test]
fn the_response_body_carries_a_row_per_slot_with_the_failure_reason() {
    let report = SelectionReport {
        resolved: vec![
            outcome(1, "a", VideoProfile::Hero, true),
            outcome(2, "b", VideoProfile::Thumbnail, false),
        ],
        pending: vec![HeroTarget {
            slot: 3,
            device_id: "c".into(),
            profile: VideoProfile::Thumbnail,
        }],
    };
    let body = outcome_body("a", &report);
    assert_eq!(body["hero"], "a");
    assert_eq!(body["slots"][0]["profile"], "hero");
    assert_eq!(body["slots"][0]["ok"], true);
    assert_eq!(body["slots"][0]["pending"], false);
    assert!(body["slots"][0]["error"].is_null());
    assert_eq!(body["slots"][1]["slot"], 2);
    assert_eq!(body["slots"][1]["ok"], false);
    assert_eq!(body["slots"][1]["error"], "no response");
    // A drone still being called is neither a success nor a failure yet.
    assert_eq!(body["slots"][2]["device_id"], "c");
    assert_eq!(body["slots"][2]["pending"], true);
    assert_eq!(body["slots"][2]["ok"], false);
    assert!(body["slots"][2]["error"].is_null());
    assert_eq!(outcome_status(&report), StatusCode::MULTI_STATUS);
}

#[test]
fn only_a_fully_answered_fleet_is_a_200() {
    let all_ok = SelectionReport {
        resolved: vec![
            outcome(1, "a", VideoProfile::Thumbnail, true),
            outcome(2, "b", VideoProfile::Hero, true),
        ],
        pending: vec![],
    };
    assert_eq!(outcome_status(&all_ok), StatusCode::OK);
    let lagging = SelectionReport {
        resolved: vec![outcome(2, "b", VideoProfile::Hero, true)],
        pending: vec![HeroTarget {
            slot: 1,
            device_id: "a".into(),
            profile: VideoProfile::Thumbnail,
        }],
    };
    assert_eq!(outcome_status(&lagging), StatusCode::MULTI_STATUS);
    // Rows stay in slot order whichever list they came from.
    assert_eq!(outcome_body("b", &lagging)["slots"][0]["device_id"], "a");
}

#[tokio::test]
async fn a_slow_answer_to_an_older_selection_cannot_overwrite_a_newer_one() {
    // Selections run on their own tasks, so two can overlap: the operator
    // picks `a`, then `b` before `a`'s calls have come back.
    let s = slots(&["a", "b"]);
    let st = FleetHeroState::default();
    let now = Instant::now();
    let first = st.begin_selection(&plan_hero(&s, "a").unwrap(), now).await;
    let second = st.begin_selection(&plan_hero(&s, "b").unwrap(), now).await;

    assert!(
        st.record_hero(second, "b", &outcome(2, "b", VideoProfile::Hero, true))
            .await
    );
    // The first selection's answers arrive late.
    assert!(
        !st.record_hero(first, "a", &outcome(1, "a", VideoProfile::Hero, true))
            .await,
        "a superseded selection must not take effect"
    );
    st.record_outcome(first, &outcome(2, "b", VideoProfile::Thumbnail, false))
        .await;

    let inputs = st.tick_inputs(now).await;
    assert_eq!(inputs.hero.as_deref(), Some("b"));
    assert_ne!(
        inputs.unconfirmed.get("b"),
        Some(&VideoProfile::Thumbnail),
        "the stale demotion of the new hero must not be queued"
    );
    // The stale promotion of `a` confirmed after the newer selection began,
    // so `a` may be a second hero: its demotion is chased again.
    assert_eq!(inputs.unconfirmed.get("a"), Some(&VideoProfile::Thumbnail));
}

#[tokio::test]
async fn a_drone_just_sent_an_assignment_settles_for_the_holdoff_and_then_does_not() {
    let s = slots(&["a", "b"]);
    let st = FleetHeroState::default();
    let t0 = Instant::now();
    st.begin_selection(&plan_hero(&s, "a").unwrap(), t0).await;
    let settling = st.tick_inputs(t0 + Duration::from_secs(1)).await.settling;
    assert!(settling.contains("a") && settling.contains("b"));
    assert!(st
        .tick_inputs(t0 + REISSUE_HOLDOFF)
        .await
        .settling
        .is_empty());
    // A drone that leaves the fleet takes its stamp with it.
    st.prune(&slots(&["a"])).await;
    let settling = st.tick_inputs(t0).await.settling;
    assert!(settling.contains("a") && !settling.contains("b"));
}

#[tokio::test]
async fn a_stale_retry_landing_after_a_new_selection_requeues_the_new_assignment() {
    // hero=a, b's demotion unconfirmed. A tick starts re-sending b its
    // thumbnail, then the operator selects b. The selection's promotion
    // confirms first; the tick's thumbnail lands after it, so b is a
    // thumbnail again and its promotion must be chased.
    let s = slots(&["a", "b"]);
    let st = FleetHeroState::default();
    let now = Instant::now();
    let first = st.begin_selection(&plan_hero(&s, "a").unwrap(), now).await;
    st.record_hero(first, "a", &outcome(1, "a", VideoProfile::Hero, true))
        .await;
    st.record_outcome(first, &outcome(2, "b", VideoProfile::Thumbnail, false))
        .await;
    let inputs = st.tick_inputs(now).await;
    let retry = fanout::plan_retry(&s, &inputs.unconfirmed);
    let tick = st
        .begin_reissue(&retry, now, inputs.generation)
        .await
        .expect("nothing newer yet");

    let second = st.begin_selection(&plan_hero(&s, "b").unwrap(), now).await;
    assert!(
        st.record_hero(second, "b", &outcome(2, "b", VideoProfile::Hero, true))
            .await
    );
    assert!(
        st.tick_inputs(now).await.unconfirmed.contains_key("b"),
        "a confirmation cannot clear b while an older thumbnail call is still out"
    );
    assert_eq!(st.confirmed_hero().await, None);
    st.record_reissue(tick, &[outcome(2, "b", VideoProfile::Thumbnail, true)])
        .await;
    let inputs = st.tick_inputs(now).await;
    assert_eq!(inputs.hero.as_deref(), Some("b"));
    assert_eq!(inputs.unconfirmed.get("b"), Some(&VideoProfile::Hero));
}

#[tokio::test]
async fn a_stale_promotion_landing_after_a_demotion_requeues_the_demotion() {
    // The mirror case: a tick re-sending a its promotion races the
    // operator moving hero to b. Without the requeue a stays a second hero.
    let s = slots(&["a", "b"]);
    let st = FleetHeroState::default();
    let now = Instant::now();
    let first = st.begin_selection(&plan_hero(&s, "a").unwrap(), now).await;
    st.record_hero(first, "a", &outcome(1, "a", VideoProfile::Hero, false))
        .await;
    st.record_outcome(first, &outcome(2, "b", VideoProfile::Thumbnail, true))
        .await;
    let inputs = st.tick_inputs(now).await;
    let retry = fanout::plan_retry(&s, &inputs.unconfirmed);
    let tick = st
        .begin_reissue(&retry, now, inputs.generation)
        .await
        .unwrap();

    let second = st.begin_selection(&plan_hero(&s, "b").unwrap(), now).await;
    st.record_hero(second, "b", &outcome(2, "b", VideoProfile::Hero, true))
        .await;
    // The stale promotion's answer comes back before the demotion's.
    st.record_reissue(tick, &[outcome(1, "a", VideoProfile::Hero, true)])
        .await;
    assert_eq!(
        st.tick_inputs(now).await.unconfirmed.get("a"),
        Some(&VideoProfile::Thumbnail)
    );
    // The demotion's own confirmation now stands: nothing older is out.
    st.record_outcome(second, &outcome(1, "a", VideoProfile::Thumbnail, true))
        .await;
    assert!(st.tick_inputs(now).await.unconfirmed.is_empty());
    assert_eq!(st.confirmed_hero().await.as_deref(), Some("b"));
}

#[tokio::test]
async fn a_pass_decided_before_a_selection_began_sends_nothing() {
    let s = slots(&["a", "b"]);
    let st = FleetHeroState::default();
    let now = Instant::now();
    let first = st.begin_selection(&plan_hero(&s, "a").unwrap(), now).await;
    st.record_outcome(first, &outcome(2, "b", VideoProfile::Thumbnail, false))
        .await;
    let inputs = st.tick_inputs(now).await;
    st.begin_selection(&plan_hero(&s, "b").unwrap(), now).await;
    let retry = fanout::plan_retry(&s, &inputs.unconfirmed);
    assert_eq!(st.begin_reissue(&retry, now, inputs.generation).await, None);
}

#[tokio::test]
async fn a_failed_promotion_is_a_502_and_is_not_served_until_it_confirms() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fleet-hero.json");
    let s = slots(&["a", "b"]);
    let st = Arc::new(FleetHeroState::default());
    let refuse_b = |id: String, _p: VideoProfile| async move {
        if id == "b" {
            Err("drone answered HTTP 401".to_string())
        } else {
            Ok(())
        }
    };
    let report = select::run_selection(
        Arc::clone(&st),
        s.clone(),
        plan_hero(&s, "b").unwrap(),
        "b".into(),
        refuse_b,
        path.clone(),
    )
    .await;
    assert_eq!(outcome_status(&report), StatusCode::BAD_GATEWAY);
    let body = outcome_body("b", &report);
    assert_eq!(body["slots"][1]["ok"], false);
    assert_eq!(body["slots"][1]["error"], "drone answered HTTP 401");
    assert!(
        ados_groundlink::read_hero_from(&path).is_none(),
        "the fan-out must not be pointed at a drone that is still a thumbnail"
    );
    assert_eq!(
        st.hero().await.as_deref(),
        Some("b"),
        "the selection is kept"
    );
    assert_eq!(st.confirmed_hero().await, None);

    // The tick's retry confirms, and only then is the hero served.
    let inputs = st.tick_inputs(Instant::now()).await;
    let retry = fanout::plan_retry(&s, &inputs.unconfirmed);
    let g = st
        .begin_reissue(&retry, Instant::now(), inputs.generation)
        .await
        .unwrap();
    st.record_reissue(g, &[outcome(2, "b", VideoProfile::Hero, true)])
        .await;
    assert_eq!(st.confirmed_hero().await.as_deref(), Some("b"));
}

#[test]
fn republishing_a_settled_selection_does_not_rewrite_the_file() {
    // The reconcile tick calls this every few seconds against a fleet that
    // usually agrees with itself; a rewrite per tick would be churn for
    // nothing, and every rewrite is a window a reader can catch.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fleet-hero.json");
    let s = slots(&["a", "b"]);
    assert!(publish_hero_to(&path, &s, "b"));
    assert!(!publish_hero_to(&path, &s, "b"), "no rewrite when settled");
    // ...but a genuine change does write.
    assert!(publish_hero_to(&path, &s, "a"));
    assert_eq!(ados_groundlink::read_hero_from(&path).unwrap().slot, 1);
}

#[test]
fn a_hero_the_registry_does_not_know_publishes_nothing() {
    // Nothing to point the fan-out at. Leaving the previous value alone is
    // safe: the fan-out validates what it reads against the same registry.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fleet-hero.json");
    let s = slots(&["a", "b"]);
    assert!(publish_hero_to(&path, &s, "b"));
    assert!(!publish_hero_to(&path, &s, "never-paired"));
    let still = ados_groundlink::read_hero_from(&path).unwrap();
    assert_eq!(still.device_id, "b");
}

#[test]
fn the_hero_slot_lookup_is_exact() {
    let s = slots(&["a", "b", "c"]);
    assert_eq!(fanout::hero_slot(&s, "a"), Some(1));
    assert_eq!(fanout::hero_slot(&s, "c"), Some(3));
    // Device ids are exact; a near-miss must not publish a slot.
    assert_eq!(fanout::hero_slot(&s, "A"), None);
    assert_eq!(fanout::hero_slot(&s, ""), None);
    assert_eq!(fanout::hero_slot(&[], "a"), None);
}

#[test]
fn an_unknown_device_id_is_refused_before_any_profile_is_touched() {
    // The route returns 404 on a `None` plan, and a `None` plan is the ONLY
    // thing that reaches `apply_plan` — so no drone is called at all. A typo
    // must never demote a flying fleet.
    let s = slots(&["a", "b", "c"]);
    assert!(plan_hero(&s, "typo").is_none());
    assert!(plan_hero(&s, "").is_none());
    // Case matters: device ids are exact.
    assert!(plan_hero(&s, "A").is_none());
}
