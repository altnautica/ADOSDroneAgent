//! One hero selection, run to completion regardless of who is waiting for it.
//!
//! The selection used to run inside the HTTP request future. A drone that did
//! not answer cost the fan-out its full call bound plus one retry, the GCS gave
//! up first, the dropped request future aborted every call still in flight, and
//! the selection was never recorded or published: the operator was left
//! watching the old hero while the drone they picked streamed full video to
//! nobody.
//!
//! [`run_selection`] is spawned instead, so no caller can cancel it, and it
//! takes effect as soon as the HERO answers rather than after the slowest
//! demotion. Demotions still in flight when the report is cut keep running in
//! the background and land in the reconcile queue exactly as if the caller had
//! waited for them.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_groundlink::FleetSlot;
use ados_video::profile::VideoProfile;
use tokio::task::JoinError;

use super::fanout::{issue_plan, HeroPlan, HeroTarget, SlotOutcome};
use super::{publish_hero_to, FleetHeroState};

/// How long a selection keeps collecting demotion outcomes for its report once
/// the hero's own call has resolved.
///
/// A healthy fleet answers well inside this (a full 24-drone fan-out drains in a
/// few seconds at the demotion bound), so the common case still reports every
/// slot. A drone that is silent would otherwise hold the answer for its whole
/// call bound plus a retry, past the point any client is still waiting; it is
/// reported `pending` instead, and its outcome is recorded when it resolves.
pub const DEMOTION_REPORT_WINDOW: Duration = Duration::from_secs(4);

/// What a selection can say about the fleet at the moment it answers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectionReport {
    /// Every drone whose call resolved (after its one retry) before the report
    /// was cut, in slot order.
    pub resolved: Vec<SlotOutcome>,
    /// Every drone still being called when the report was cut, in slot order.
    /// Their outcomes are recorded in the fleet state when they resolve.
    pub pending: Vec<HeroTarget>,
}

impl SelectionReport {
    /// Every registered drone answered, and every answer was a success.
    pub fn complete(&self) -> bool {
        self.pending.is_empty() && self.resolved.iter().all(|o| o.ok)
    }

    /// The hero's own promotion resolved and failed: the selection is recorded
    /// and chased, but the drone the operator picked is still a thumbnail.
    pub fn hero_failed(&self) -> bool {
        self.resolved
            .iter()
            .any(|o| o.profile == VideoProfile::Hero && !o.ok)
    }
}

/// [`run_selection`] on its own task, awaited through its handle.
///
/// This is what a request handler awaits. Awaiting a `JoinHandle` is
/// cancel-safe: dropping this future (a client that timed out, a connection
/// that closed mid-request) detaches the selection rather than aborting it, so
/// the calls still land and the hero is still recorded and published.
pub async fn select_hero<F, Fut>(
    hero_state: Arc<FleetHeroState>,
    slots: Vec<FleetSlot>,
    plan: HeroPlan,
    hero: String,
    call: F,
    publish_path: PathBuf,
) -> Result<SelectionReport, JoinError>
where
    F: Fn(String, VideoProfile) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    tokio::spawn(run_selection(
        hero_state,
        slots,
        plan,
        hero,
        call,
        publish_path,
    ))
    .await
}

/// Run one hero selection: issue the plan, record and publish the hero the
/// moment its own call resolves, and report on the demotions for at most
/// [`DEMOTION_REPORT_WINDOW`] after that.
///
/// Run through [`select_hero`] by the route, so no caller can cancel it. Every
/// outcome carries the selection's generation back to `hero_state`, so a slow
/// answer from an older selection can never overwrite a newer one.
pub async fn run_selection<F, Fut>(
    hero_state: Arc<FleetHeroState>,
    slots: Vec<FleetSlot>,
    plan: HeroPlan,
    hero: String,
    call: F,
    publish_path: PathBuf,
) -> SelectionReport
where
    F: Fn(String, VideoProfile) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send,
{
    let generation = hero_state.begin_selection(&plan, Instant::now()).await;
    let mut outcomes = issue_plan(&plan, call);
    let mut done: Vec<Option<SlotOutcome>> = vec![None; plan.targets.len()];
    let mut report_until: Option<tokio::time::Instant> = None;

    loop {
        let next = match report_until {
            None => outcomes.recv().await,
            Some(deadline) => match tokio::time::timeout_at(deadline, outcomes.recv()).await {
                Ok(next) => next,
                Err(_) => break,
            },
        };
        let Some((index, outcome)) = next else {
            // Every call has resolved.
            break;
        };
        if outcome.profile == VideoProfile::Hero {
            // The selection takes effect here, not after the slowest demotion.
            // The fan-out is re-pointed only once the promotion has CONFIRMED:
            // re-pointing on a failed one would put the operator on a drone
            // that is still a 1 fps thumbnail, when what they had a moment ago
            // was a full-rate stream. The reconcile tick publishes it once the
            // queued promotion confirms.
            if hero_state.record_hero(generation, &hero, &outcome).await && outcome.ok {
                publish_hero_to(&publish_path, &slots, &hero);
            }
            report_until = Some(tokio::time::Instant::now() + DEMOTION_REPORT_WINDOW);
        } else {
            hero_state.record_outcome(generation, &outcome).await;
        }
        done[index] = Some(outcome);
    }

    if done.iter().any(Option::is_none) {
        // The report is cut; the calls are not. Each lagging demotion is
        // recorded when it resolves, so a failure lands in the reconcile queue
        // instead of being lost with the report.
        tokio::spawn(async move {
            while let Some((_, outcome)) = outcomes.recv().await {
                hero_state.record_outcome(generation, &outcome).await;
            }
        });
    }

    let mut report = SelectionReport::default();
    for (target, outcome) in plan.targets.into_iter().zip(done) {
        match outcome {
            Some(outcome) => report.resolved.push(outcome),
            None => report.pending.push(target),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::gs_fleet_hero::fanout::plan_hero;
    use crate::routes::gs_fleet_hero::outcome_status;
    use crate::routes::gs_fleet_hero::reconcile::{decide_tick, TickAction};
    use crate::routes::gs_fleet_hero::tests_support::slots;
    use axum::http::StatusCode;
    use std::collections::BTreeMap;
    use tokio::sync::Notify;

    /// A call that every drone answers at once.
    async fn answer(_id: String, _p: VideoProfile) -> Result<(), String> {
        Ok(())
    }

    /// Poll until `st` names `hero`, bounded so a regression fails instead of
    /// hanging.
    async fn hero_becomes(st: &FleetHeroState, hero: &str) -> bool {
        for _ in 0..200 {
            if st.hero().await.as_deref() == Some(hero) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn selecting_a_new_hero_demotes_the_previous_one_and_publishes_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-hero.json");
        let s = slots(&["a", "b", "c"]);
        let st = Arc::new(FleetHeroState::default());
        let plan = plan_hero(&s, "a").unwrap();
        select_hero(
            Arc::clone(&st),
            s.clone(),
            plan,
            "a".into(),
            answer,
            path.clone(),
        )
        .await
        .unwrap();

        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let record = {
            let calls = Arc::clone(&calls);
            move |id: String, p: VideoProfile| {
                calls.lock().push((id, p));
                async { Ok(()) }
            }
        };
        let plan = plan_hero(&s, "c").unwrap();
        let report = select_hero(
            Arc::clone(&st),
            s.clone(),
            plan,
            "c".into(),
            record,
            path.clone(),
        )
        .await
        .unwrap();

        assert!(report.complete());
        let mut calls = calls.lock().clone();
        calls.sort_by(|a, b| a.0.cmp(&b.0));
        // The previous hero is demoted, the drone that was already a thumbnail
        // is simply reasserted, and nobody else is promoted by accident.
        assert_eq!(
            calls,
            vec![
                ("a".to_string(), VideoProfile::Thumbnail),
                ("b".to_string(), VideoProfile::Thumbnail),
                ("c".to_string(), VideoProfile::Hero),
            ]
        );
        assert_eq!(st.hero().await.as_deref(), Some("c"));
        // A hero that is not on the lowest slot is what the fan-out needs told.
        let published = ados_groundlink::read_hero_from(&path).expect("a selection must publish");
        assert_eq!(published.slot, 3);
        assert_eq!(published.device_id, "c");
    }

    #[tokio::test]
    async fn a_non_responding_drone_yields_207_while_the_new_hero_is_still_promoted() {
        let dir = tempfile::tempdir().unwrap();
        let s = slots(&["deaf", "newhero"]);
        let st = Arc::new(FleetHeroState::default());
        let deaf = |id: String, _p: VideoProfile| async move {
            if id == "deaf" {
                Err("no response from the linked drone within the bound".to_string())
            } else {
                Ok(())
            }
        };
        let plan = plan_hero(&s, "newhero").unwrap();
        let report = select_hero(
            Arc::clone(&st),
            s.clone(),
            plan,
            "newhero".into(),
            deaf,
            dir.path().join("fleet-hero.json"),
        )
        .await
        .unwrap();

        assert_eq!(outcome_status(&report), StatusCode::MULTI_STATUS);
        assert!(
            !report.resolved[0].ok,
            "per-slot outcome, not a blanket failure"
        );
        // The promotion went through anyway — a drone stuck on hero is an
        // airtime problem, never a reason to refuse the operator's selection.
        assert!(report.resolved[1].ok);
        assert_eq!(report.resolved[1].profile, VideoProfile::Hero);

        // And the failure is queued for the reconcile tick rather than lost.
        let inputs = st.tick_inputs(Instant::now()).await;
        assert_eq!(inputs.hero.as_deref(), Some("newhero"));
        let TickAction::Retry { plan } = decide_tick(
            &s,
            inputs.hero.as_deref(),
            &inputs.unconfirmed,
            &BTreeMap::new(),
            &inputs.settling,
        ) else {
            panic!("the deaf drone's demotion must be re-issued");
        };
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].device_id, "deaf");
    }

    #[tokio::test]
    async fn a_fully_confirmed_selection_is_200_and_leaves_nothing_queued() {
        let dir = tempfile::tempdir().unwrap();
        let s = slots(&["a", "b"]);
        let st = Arc::new(FleetHeroState::default());
        let plan = plan_hero(&s, "b").unwrap();
        let report = select_hero(
            Arc::clone(&st),
            s,
            plan,
            "b".into(),
            answer,
            dir.path().join("fleet-hero.json"),
        )
        .await
        .unwrap();
        assert_eq!(outcome_status(&report), StatusCode::OK);
        assert!(st.tick_inputs(Instant::now()).await.unconfirmed.is_empty());
    }

    #[tokio::test]
    async fn a_request_dropped_after_the_hero_answered_still_records_and_publishes() {
        // The hero answers at once; a powered-off drone never does. The client
        // gives up and the request future is dropped, which is what the HTTP
        // server does to a handler whose connection closed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-hero.json");
        let s = slots(&["hero", "off"]);
        let st = Arc::new(FleetHeroState::default());
        let answered = Arc::new(Notify::new());
        let call = {
            let answered = Arc::clone(&answered);
            move |id: String, _p: VideoProfile| {
                let answered = Arc::clone(&answered);
                async move {
                    if id == "hero" {
                        answered.notify_one();
                        Ok(())
                    } else {
                        std::future::pending().await
                    }
                }
            }
        };
        let plan = plan_hero(&s, "hero").unwrap();
        let request = tokio::spawn(select_hero(
            Arc::clone(&st),
            s.clone(),
            plan,
            "hero".into(),
            call,
            path.clone(),
        ));

        answered.notified().await;
        request.abort();

        assert!(
            hero_becomes(&st, "hero").await,
            "the selection must take effect though its request was dropped"
        );
        let published = ados_groundlink::read_hero_from(&path).expect("the hero must be published");
        assert_eq!(published.device_id, "hero");
    }

    #[tokio::test]
    async fn a_request_dropped_before_the_hero_answered_still_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-hero.json");
        let s = slots(&["other", "hero"]);
        let st = Arc::new(FleetHeroState::default());
        let reached = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let call = {
            let reached = Arc::clone(&reached);
            let release = Arc::clone(&release);
            move |id: String, _p: VideoProfile| {
                let reached = Arc::clone(&reached);
                let release = Arc::clone(&release);
                async move {
                    if id == "hero" {
                        reached.notify_one();
                        release.notified().await;
                    }
                    Ok(())
                }
            }
        };
        let plan = plan_hero(&s, "hero").unwrap();
        let request = tokio::spawn(select_hero(
            Arc::clone(&st),
            s.clone(),
            plan,
            "hero".into(),
            call,
            path.clone(),
        ));

        reached.notified().await;
        request.abort();
        let _ = request.await;
        // The promotion lands only after its caller has gone.
        release.notify_one();

        assert!(hero_becomes(&st, "hero").await);
        assert_eq!(ados_groundlink::read_hero_from(&path).unwrap().slot, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_demotion_is_reported_pending_and_its_failure_still_reaches_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let s = slots(&["hero", "silent"]);
        let st = Arc::new(FleetHeroState::default());
        let call = |id: String, _p: VideoProfile| async move {
            if id == "silent" {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Err("no response from the linked drone within the bound".to_string())
            } else {
                Ok(())
            }
        };
        let plan = plan_hero(&s, "hero").unwrap();
        let started = tokio::time::Instant::now();
        let report = select_hero(
            Arc::clone(&st),
            s.clone(),
            plan,
            "hero".into(),
            call,
            dir.path().join("fleet-hero.json"),
        )
        .await
        .unwrap();

        // Answered on the hero, not on the slowest drone.
        assert!(started.elapsed() <= DEMOTION_REPORT_WINDOW + Duration::from_secs(1));
        assert_eq!(report.resolved.len(), 1);
        assert_eq!(report.resolved[0].device_id, "hero");
        assert_eq!(report.pending.len(), 1);
        assert_eq!(report.pending[0].device_id, "silent");
        assert_eq!(outcome_status(&report), StatusCode::MULTI_STATUS);

        // The call and its retry run out in the background, and the failure
        // is queued for the reconcile tick exactly as if the caller had waited.
        tokio::time::sleep(Duration::from_secs(300)).await;
        let inputs = st.tick_inputs(Instant::now()).await;
        assert_eq!(
            inputs.unconfirmed.get("silent"),
            Some(&VideoProfile::Thumbnail)
        );
    }
}
