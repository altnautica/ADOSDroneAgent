//! The pure half of hero selection: which drone gets which profile, and how the
//! per-drone calls are issued.
//!
//! Split from the route so the whole policy — exclusivity, the demote set, the
//! single retry, the partial-success outcome — is testable with no radio, no
//! registry file and no HTTP.

use std::collections::BTreeMap;
use std::future::Future;

use ados_groundlink::FleetSlot;
use ados_video::profile::VideoProfile;

/// What one hero selection asks of one drone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeroTarget {
    pub slot: u8,
    pub device_id: String,
    pub profile: VideoProfile,
}

/// Every drone a hero selection touches, in slot order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeroPlan {
    pub targets: Vec<HeroTarget>,
}

impl HeroPlan {
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

/// How one drone answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotOutcome {
    pub slot: u8,
    pub device_id: String,
    pub profile: VideoProfile,
    pub ok: bool,
    /// Why it failed, after the retry. `None` on success.
    pub error: Option<String>,
}

/// The per-drone assignments implied by making `hero` the hero.
///
/// Exactly one drone is promoted and EVERY other registered drone is demoted in
/// the same operation, which is what makes hero exclusive: the previous hero is
/// demoted because it is "some other registered drone", not because it is
/// tracked separately. A hero that is not in the registry yields `None` and the
/// caller must reject the request without issuing a single call — an operator
/// typo must never demote the fleet.
pub fn plan_hero(slots: &[FleetSlot], hero: &str) -> Option<HeroPlan> {
    if !slots.iter().any(|s| s.device_id == hero) {
        return None;
    }
    Some(HeroPlan {
        targets: slots
            .iter()
            .map(|s| HeroTarget {
                slot: s.slot,
                device_id: s.device_id.clone(),
                profile: if s.device_id == hero {
                    VideoProfile::Hero
                } else {
                    VideoProfile::Thumbnail
                },
            })
            .collect(),
    })
}

/// The plan that re-issues only the assignments a previous operation could not
/// confirm, dropping any device that has since left the fleet.
///
/// The reconcile tick uses this instead of re-asserting all 24 slots every few
/// seconds: a healthy fleet then costs zero radio traffic, and only a drone that
/// actually failed keeps being chased.
pub fn plan_retry(slots: &[FleetSlot], unconfirmed: &BTreeMap<String, VideoProfile>) -> HeroPlan {
    HeroPlan {
        targets: slots
            .iter()
            .filter_map(|s| {
                unconfirmed.get(&s.device_id).map(|p| HeroTarget {
                    slot: s.slot,
                    device_id: s.device_id.clone(),
                    profile: *p,
                })
            })
            .collect(),
    }
}

/// The slot `hero` holds, or `None` when it is not registered.
///
/// The fan-out addresses drones by SLOT (a slot is the low byte of the wfb-ng
/// `link_id`, so it is what a video egress port is derived from) while every
/// operator-facing surface names them by device id. This is the one translation
/// between the two, and it answers `None` rather than guessing for a hero the
/// registry does not know — publishing a slot for a drone that is not there
/// would point the fan-out at a port nothing transmits on.
pub fn hero_slot(slots: &[FleetSlot], hero: &str) -> Option<u8> {
    slots.iter().find(|s| s.device_id == hero).map(|s| s.slot)
}

/// The drone a one-slot fleet must be running as hero.
///
/// A single-drone fleet is the existing product: it has always streamed full
/// video, and booting every drone to `thumbnail` would silently regress it to
/// 320x180. When the registry holds exactly one slot that drone IS the hero, no
/// operator action required. `None` for any other fleet size — with two or more
/// drones the choice is the operator's and must not be guessed.
pub fn sole_slot_hero(slots: &[FleetSlot]) -> Option<&str> {
    match slots {
        [only] => Some(only.device_id.as_str()),
        _ => None,
    }
}

/// Issue every assignment CONCURRENTLY, retrying each failure exactly once.
///
/// Concurrency is the point: a 24-drone fleet demoted serially at the RPC
/// timeout would take minutes, and the promotion of the new hero must not queue
/// behind the demotion of a drone that has gone silent. A drone that still fails
/// after its retry is reported in its outcome and left for the reconcile tick —
/// a drone stuck on `hero` is an airtime problem, not a safety one, so it never
/// blocks the new hero's promotion.
///
/// Outcomes come back in slot order regardless of completion order, so the
/// response body is stable.
pub async fn apply_plan<F, Fut>(plan: &HeroPlan, call: F) -> Vec<SlotOutcome>
where
    F: Fn(String, VideoProfile) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send,
{
    let mut set = tokio::task::JoinSet::new();
    for (index, target) in plan.targets.iter().enumerate() {
        let call = call.clone();
        let target = target.clone();
        set.spawn(async move {
            let mut error = call(target.device_id.clone(), target.profile).await.err();
            if error.is_some() {
                // One retry. A single dropped aux fragment is the common
                // failure and a second attempt clears it.
                error = call(target.device_id.clone(), target.profile).await.err();
            }
            (
                index,
                SlotOutcome {
                    slot: target.slot,
                    device_id: target.device_id,
                    profile: target.profile,
                    ok: error.is_none(),
                    error,
                },
            )
        });
    }

    let mut done: Vec<Option<SlotOutcome>> = vec![None; plan.targets.len()];
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((index, outcome)) => done[index] = Some(outcome),
            Err(e) => tracing::warn!(error = %e, "fleet_hero_task_panicked"),
        }
    }
    done.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn slots(ids: &[&str]) -> Vec<FleetSlot> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| FleetSlot {
                slot: (i + 1) as u8,
                device_id: (*id).to_string(),
                paired_at_ms: 0,
                relay_secret: None,
            })
            .collect()
    }

    #[test]
    fn selecting_a_hero_demotes_exactly_the_others_and_nobody_is_missed() {
        let s = slots(&["a", "b", "c"]);
        let plan = plan_hero(&s, "b").unwrap();
        assert_eq!(
            plan.targets,
            vec![
                HeroTarget {
                    slot: 1,
                    device_id: "a".into(),
                    profile: VideoProfile::Thumbnail
                },
                HeroTarget {
                    slot: 2,
                    device_id: "b".into(),
                    profile: VideoProfile::Hero
                },
                HeroTarget {
                    slot: 3,
                    device_id: "c".into(),
                    profile: VideoProfile::Thumbnail
                },
            ]
        );
        // Exclusivity: exactly one hero in the plan, always.
        assert_eq!(
            plan.targets
                .iter()
                .filter(|t| t.profile == VideoProfile::Hero)
                .count(),
            1
        );
    }

    #[test]
    fn an_unregistered_device_yields_no_plan_at_all() {
        // The route turns this into a 404 with zero calls issued: a typo must
        // never demote the whole fleet.
        assert!(plan_hero(&slots(&["a", "b"]), "ghost").is_none());
        assert!(plan_hero(&[], "a").is_none());
    }

    #[test]
    fn a_one_slot_fleet_is_its_own_hero_and_a_bigger_one_is_not_guessed() {
        assert_eq!(sole_slot_hero(&slots(&["only"])), Some("only"));
        assert_eq!(sole_slot_hero(&slots(&["a", "b"])), None);
        assert_eq!(sole_slot_hero(&[]), None);
    }

    #[test]
    fn the_retry_plan_covers_only_unconfirmed_and_still_registered_drones() {
        let s = slots(&["a", "b", "c"]);
        let mut unconfirmed = BTreeMap::new();
        unconfirmed.insert("c".to_string(), VideoProfile::Thumbnail);
        unconfirmed.insert("gone".to_string(), VideoProfile::Thumbnail);
        let plan = plan_retry(&s, &unconfirmed);
        assert_eq!(
            plan.targets,
            vec![HeroTarget {
                slot: 3,
                device_id: "c".into(),
                profile: VideoProfile::Thumbnail
            }]
        );
        // A healthy fleet costs no radio traffic at all.
        assert!(plan_retry(&s, &BTreeMap::new()).is_empty());
    }

    #[tokio::test]
    async fn every_target_is_called_and_outcomes_come_back_in_slot_order() {
        let plan = plan_hero(&slots(&["a", "b", "c"]), "b").unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcomes = {
            let seen = Arc::clone(&seen);
            apply_plan(&plan, move |id, p| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().await.push((id, p));
                    Ok(())
                }
            })
            .await
        };
        assert!(outcomes.iter().all(|o| o.ok));
        assert_eq!(
            outcomes.iter().map(|o| o.slot).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let mut seen = seen.lock().await.clone();
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            seen,
            vec![
                ("a".to_string(), VideoProfile::Thumbnail),
                ("b".to_string(), VideoProfile::Hero),
                ("c".to_string(), VideoProfile::Thumbnail),
            ]
        );
    }

    #[tokio::test]
    async fn a_failing_drone_is_retried_once_and_never_blocks_the_hero() {
        let plan = plan_hero(&slots(&["dead", "newhero"]), "newhero").unwrap();
        let attempts = Arc::new(Mutex::new(BTreeMap::<String, u32>::new()));
        let outcomes = {
            let attempts = Arc::clone(&attempts);
            apply_plan(&plan, move |id, _p| {
                let attempts = Arc::clone(&attempts);
                async move {
                    let n = {
                        let mut a = attempts.lock().await;
                        let e = a.entry(id.clone()).or_insert(0);
                        *e += 1;
                        *e
                    };
                    if id == "dead" {
                        Err(format!("no response (attempt {n})"))
                    } else {
                        Ok(())
                    }
                }
            })
            .await
        };
        let dead = outcomes.iter().find(|o| o.device_id == "dead").unwrap();
        assert!(!dead.ok);
        assert!(dead.error.as_deref().unwrap().contains("attempt 2"));
        // Promoted regardless — the new hero is never held hostage by a drone
        // that will not demote.
        let hero = outcomes.iter().find(|o| o.device_id == "newhero").unwrap();
        assert!(hero.ok);
        assert_eq!(hero.profile, VideoProfile::Hero);
        let attempts = attempts.lock().await;
        assert_eq!(attempts["dead"], 2, "exactly one retry, not a retry storm");
        assert_eq!(attempts["newhero"], 1, "a success is never retried");
    }
}
