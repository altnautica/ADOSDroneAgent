//! Ground-station hero selection: **`POST /api/v1/ground-station/fleet/hero`**.
//!
//! Body `{"device_id": "<id>"}`. The named drone is promoted to the full video
//! profile and EVERY other registered drone is demoted to 320x180/1 fps/50 kbps,
//! concurrently, in one operation — which is what makes hero exclusive: the
//! previous hero is demoted because it is "some other registered drone", not
//! because it is tracked separately, so the two can never disagree.
//!
//! Each drone is reached with a targeted `POST /api/video/profile` over the
//! radio's aux RPC lane. Slots come from the ground station's
//! [`FleetRegistry`](ados_groundlink::FleetRegistry), the same table the pair
//! route writes and the receive-chain reconciler drives — read per request, not
//! cached, because a drone can pair at any moment.
//!
//! ## Why this route exists at all
//!
//! One 20 MHz channel, one radio per node. A hero costs 48% of the channel's
//! airtime at MCS 1; a control-only drone costs 2.4%. Twenty-four heroes is a
//! physical impossibility, so attention is rationed. The full arithmetic — and
//! the fact that 24 drones do NOT fit even with thumbnails until the adaptive
//! MCS ladder lands — is documented on `ados_video::profile`.
//!
//! ## Partial success is reported, not hidden
//!
//! A drone that does not answer is retried once. Still failing, it lands in the
//! response's per-slot outcomes and the route answers **207**, while the new
//! hero's promotion goes through regardless: a drone stuck on `hero` costs
//! airtime, it does not endanger anything, so it must never gate the operator's
//! selection. The reconcile tick re-issues just that drone's demotion until it
//! takes, and re-asserts nothing else — a healthy fleet costs zero radio
//! traffic.

pub mod fanout;
pub mod reconcile;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use ados_groundlink::FleetSlot;
use ados_protocol::aux_rpc::RpcMethod;
use ados_protocol::aux_rpc_proxy::AuxRpcProxy;
use ados_video::profile::VideoProfile;

use crate::routes::detail;
use crate::state::AppState;

use fanout::{apply_plan, plan_hero, SlotOutcome};
use reconcile::registered_slots;

pub use reconcile::run_hero_reconciler;

/// How often the ground station reconciles the fleet's attention state.
///
/// Two jobs, both cheap: auto-promote a one-drone fleet (the existing
/// single-drone product, which must not sit at 320x180 waiting for an operator
/// click), and re-issue any demotion that has not yet been confirmed.
pub const HERO_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// The drone's attention-profile route, reached over the aux RPC lane.
const DRONE_PROFILE_PATH: &[u8] = b"/api/video/profile";

// ---------------------------------------------------------------------------
// shared state
// ---------------------------------------------------------------------------

/// The ground station's view of fleet attention.
///
/// Process-global by nature: one ground station drives exactly one fleet, and
/// both the route and the reconcile ticker must see the same selection. Held
/// behind [`fleet_hero_state`] rather than threaded through `AppState` so the
/// ticker needs no request context; tests construct their own instance.
#[derive(Debug, Default)]
pub struct FleetHeroState {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The current hero's device id, or `None` before the first selection.
    hero: Option<String>,
    /// Assignments asked for but not confirmed, retried by the reconcile tick.
    unconfirmed: BTreeMap<String, VideoProfile>,
}

impl FleetHeroState {
    /// The currently selected hero.
    pub async fn hero(&self) -> Option<String> {
        self.inner.lock().await.hero.clone()
    }

    /// Record the result of a hero selection: the selection itself is sticky,
    /// and every drone that did not confirm is queued for the reconcile tick.
    pub async fn record_selection(&self, hero: &str, outcomes: &[SlotOutcome]) {
        let mut inner = self.inner.lock().await;
        inner.hero = Some(hero.to_string());
        for o in outcomes {
            if o.ok {
                inner.unconfirmed.remove(&o.device_id);
            } else {
                inner.unconfirmed.insert(o.device_id.clone(), o.profile);
            }
        }
    }

    /// Record the result of a retry pass: confirmations clear, failures stay.
    pub async fn record_retry(&self, outcomes: &[SlotOutcome]) {
        let mut inner = self.inner.lock().await;
        for o in outcomes {
            if o.ok {
                inner.unconfirmed.remove(&o.device_id);
            }
        }
    }

    /// Drop queued assignments for drones that have left the fleet — chasing an
    /// unpaired drone forever would be a permanent radio cost for nothing.
    pub async fn prune(&self, slots: &[FleetSlot]) {
        let mut inner = self.inner.lock().await;
        inner
            .unconfirmed
            .retain(|id, _| slots.iter().any(|s| &s.device_id == id));
    }

    /// The current selection plus everything still awaiting confirmation.
    pub(super) async fn outstanding(&self) -> (Option<String>, BTreeMap<String, VideoProfile>) {
        let inner = self.inner.lock().await;
        (inner.hero.clone(), inner.unconfirmed.clone())
    }
}

/// The process-wide fleet attention state.
static FLEET_HERO_STATE: LazyLock<FleetHeroState> = LazyLock::new(FleetHeroState::default);

/// The process-wide fleet attention state, shared by the route and the
/// reconcile ticker so they can never disagree about the current selection.
pub fn fleet_hero_state() -> &'static FleetHeroState {
    &FLEET_HERO_STATE
}

// ---------------------------------------------------------------------------
// route
// ---------------------------------------------------------------------------

/// `POST /api/v1/ground-station/fleet/hero` — select the fleet's hero drone.
pub async fn post_fleet_hero(State(state): State<AppState>, body: Option<Json<Value>>) -> Response {
    if !is_ground_station(&state) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
        )
            .into_response();
    }

    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let Some(device_id) = body
        .get("device_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return detail(StatusCode::BAD_REQUEST, "device_id is required");
    };

    let Some(proxy) = state.aux_rpc_proxy.clone() else {
        return detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "relay-proxy not initialised on this node",
        );
    };

    let slots = registered_slots();

    // An unregistered target is refused BEFORE a single call goes out: an
    // operator typo must never demote the fleet to chase a drone that is not
    // there.
    let Some(plan) = plan_hero(&slots, device_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "detail": {"error": {"code": "E_UNKNOWN_DEVICE", "device_id": device_id}}
            })),
        )
            .into_response();
    };

    let outcomes = apply_plan(&plan, profile_caller(proxy)).await;
    fleet_hero_state()
        .record_selection(device_id, &outcomes)
        .await;

    (
        outcome_status(&outcomes),
        Json(outcome_body(device_id, &outcomes)),
    )
        .into_response()
}

/// `200` when every registered drone confirmed, `207 Multi-Status` when at
/// least one did not — never a blanket `500`, because the operator's hero WAS
/// promoted and the body says exactly which slots lagged.
fn outcome_status(outcomes: &[SlotOutcome]) -> StatusCode {
    if outcomes.iter().all(|o| o.ok) {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    }
}

/// The response body: the selected hero plus one row per registered slot.
fn outcome_body(hero: &str, outcomes: &[SlotOutcome]) -> Value {
    json!({
        "hero": hero,
        "slots": outcomes
            .iter()
            .map(|o| json!({
                "slot": o.slot,
                "device_id": o.device_id,
                "profile": o.profile.as_str(),
                "ok": o.ok,
                "error": o.error,
            }))
            .collect::<Vec<_>>(),
    })
}

/// The per-drone call: a targeted `POST /api/video/profile` over the aux lane.
pub(super) fn profile_caller(
    proxy: Arc<AuxRpcProxy>,
) -> impl Fn(String, VideoProfile) -> ProfileCall + Clone + Send + 'static {
    move |device_id: String, profile: VideoProfile| {
        let proxy = Arc::clone(&proxy);
        ProfileCall(Box::pin(async move {
            let body = json!({"profile": profile.as_str()}).to_string();
            match proxy
                .call(
                    device_id.as_bytes(),
                    RpcMethod::Post,
                    DRONE_PROFILE_PATH,
                    body.as_bytes(),
                )
                .await
            {
                Ok(resp) if (200..300).contains(&resp.status) => Ok(()),
                Ok(resp) => Err(format!("drone answered HTTP {}", resp.status)),
                Err(e) => Err(format!("{e}")),
            }
        }))
    }
}

/// A named boxed future, so [`profile_caller`] can be spelled as an `impl Fn`
/// returning ONE concrete `Future` type. The workspace carries no `futures`
/// crate, and an `async` block's opaque type cannot be named in the return
/// position of a closure-returning function.
pub(super) struct ProfileCall(Pin<Box<dyn Future<Output = Result<(), String>> + Send>>);

impl Future for ProfileCall {
    type Output = Result<(), String>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}

fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// Fixtures shared by this module's tests and [`reconcile`]'s.
#[cfg(test)]
pub(super) mod tests_support {
    use super::*;

    pub fn slots(ids: &[&str]) -> Vec<FleetSlot> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| FleetSlot {
                slot: (i + 1) as u8,
                device_id: (*id).to_string(),
                paired_at: 0.0,
            })
            .collect()
    }

    pub fn outcome(slot: u8, id: &str, profile: VideoProfile, ok: bool) -> SlotOutcome {
        SlotOutcome {
            slot,
            device_id: id.to_string(),
            profile,
            ok,
            error: (!ok).then(|| "no response".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reconcile::{decide_tick, TickAction};
    use super::tests_support::{outcome, slots};
    use super::*;

    #[tokio::test]
    async fn selecting_a_new_hero_demotes_the_previous_one_and_nobody_else() {
        let s = slots(&["a", "b", "c"]);
        let st = FleetHeroState::default();
        // `a` is hero.
        let first = apply_plan(&plan_hero(&s, "a").unwrap(), |_id, _p| async { Ok(()) }).await;
        st.record_selection("a", &first).await;

        // Now select `c`.
        let plan = plan_hero(&s, "c").unwrap();
        let assignments: BTreeMap<&str, VideoProfile> = plan
            .targets
            .iter()
            .map(|t| (t.device_id.as_str(), t.profile))
            .collect();
        assert_eq!(assignments["c"], VideoProfile::Hero);
        // The previous hero is demoted...
        assert_eq!(assignments["a"], VideoProfile::Thumbnail);
        // ...and the drone that was already a thumbnail is simply reasserted,
        // never promoted by accident.
        assert_eq!(assignments["b"], VideoProfile::Thumbnail);

        let second = apply_plan(&plan, |_id, _p| async { Ok(()) }).await;
        st.record_selection("c", &second).await;
        assert_eq!(st.hero().await.as_deref(), Some("c"));
    }

    #[tokio::test]
    async fn the_response_body_carries_a_row_per_slot_with_the_failure_reason() {
        let outcomes = vec![
            outcome(1, "a", VideoProfile::Hero, true),
            outcome(2, "b", VideoProfile::Thumbnail, false),
        ];
        let body = outcome_body("a", &outcomes);
        assert_eq!(body["hero"], "a");
        assert_eq!(body["slots"][0]["profile"], "hero");
        assert_eq!(body["slots"][0]["ok"], true);
        assert!(body["slots"][0]["error"].is_null());
        assert_eq!(body["slots"][1]["slot"], 2);
        assert_eq!(body["slots"][1]["ok"], false);
        assert_eq!(body["slots"][1]["error"], "no response");
    }

    #[tokio::test]
    async fn a_non_responding_drone_yields_207_while_the_new_hero_is_still_promoted() {
        let s = slots(&["deaf", "newhero"]);
        let plan = plan_hero(&s, "newhero").unwrap();
        let outcomes = apply_plan(&plan, |id, _p| async move {
            if id == "deaf" {
                Err("no response from the linked drone within the bound".to_string())
            } else {
                Ok(())
            }
        })
        .await;

        assert_eq!(outcome_status(&outcomes), StatusCode::MULTI_STATUS);
        let body = outcome_body("newhero", &outcomes);
        assert_eq!(body["hero"], "newhero");
        // Per-slot outcomes, not a blanket failure.
        assert_eq!(body["slots"][0]["device_id"], "deaf");
        assert_eq!(body["slots"][0]["ok"], false);
        // The promotion went through anyway — a drone stuck on hero is an
        // airtime problem, never a reason to refuse the operator's selection.
        assert_eq!(body["slots"][1]["device_id"], "newhero");
        assert_eq!(body["slots"][1]["profile"], "hero");
        assert_eq!(body["slots"][1]["ok"], true);

        // And the failure is queued for the reconcile tick rather than lost.
        let st = FleetHeroState::default();
        st.record_selection("newhero", &outcomes).await;
        let (hero, unconfirmed) = st.outstanding().await;
        assert_eq!(hero.as_deref(), Some("newhero"));
        let TickAction::Retry { plan } = decide_tick(&s, hero.as_deref(), &unconfirmed) else {
            panic!("the deaf drone's demotion must be re-issued");
        };
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].device_id, "deaf");
    }

    #[tokio::test]
    async fn a_fully_confirmed_selection_is_200_and_leaves_nothing_queued() {
        let s = slots(&["a", "b"]);
        let plan = plan_hero(&s, "b").unwrap();
        let outcomes = apply_plan(&plan, |_id, _p| async { Ok(()) }).await;
        assert_eq!(outcome_status(&outcomes), StatusCode::OK);
        let st = FleetHeroState::default();
        st.record_selection("b", &outcomes).await;
        assert!(st.outstanding().await.1.is_empty());
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
}
