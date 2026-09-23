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
//! response's per-slot outcomes. A demotion that failed or is still in flight
//! answers **207**: a drone stuck on `hero` costs airtime, it does not endanger
//! anything, so it must never gate the operator's selection. The selection runs
//! on its own task and takes effect the moment the hero's own call resolves; a
//! demotion still in flight a few seconds later is reported `pending` and
//! finishes in the background (see [`select`]). A hero whose OWN promotion
//! failed answers **502** with the same per-slot body, and the fan-out is not
//! re-pointed at it: the drone is still a thumbnail, and the promotion is
//! chased by the reconcile tick until it confirms.
//!
//! ## Overlapping operations
//!
//! Selections and reconcile passes run concurrently, so a slow call from an
//! older operation can land on a drone AFTER a newer one did. Every call in
//! flight is tracked with the generation and profile it carries. A newer
//! confirmation does not clear a drone while an older call carrying a
//! different profile is still out, and an older answer that contradicts the
//! current selection queues the current assignment again.
//!
//! ## The beacon is the truth
//!
//! Every call carries the per-drone relay ticket, because a drone refuses any
//! relayed request without one. What a drone is actually running comes back on
//! its swarm beacon's hero bit, and the reconcile tick compares that against the
//! selection: a hero that rebooted (it comes back as a thumbnail) is promoted
//! again, a drone that failed to demote is demoted again, and a fleet that
//! agrees costs zero radio traffic.

pub mod fanout;
pub mod reconcile;
pub mod select;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use ados_groundlink::FleetSlot;
use ados_protocol::aux_rpc::RpcMethod;
use ados_protocol::aux_rpc_proxy::{AuxRpcProxy, RPC_DEFAULT_TIMEOUT};
use ados_video::profile::VideoProfile;

use crate::routes::detail;
use crate::routes::gs_fleet_slot::{mint_ticket, registered_slots};
use crate::state::AppState;

use fanout::{plan_hero, HeroPlan, SlotOutcome};
use select::{select_hero, SelectionReport};

pub use reconcile::run_hero_reconciler;

/// How often the ground station reconciles the fleet's attention state.
///
/// Three jobs, all cheap: auto-promote a one-drone fleet (the existing
/// single-drone product, which must not sit at 320x180 waiting for an operator
/// click), re-issue any assignment that has not yet been confirmed, and
/// re-assert any drone whose beacon disagrees with the selection.
pub const HERO_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// How long after a drone was sent an assignment before its beacon is held to
/// it, and so the fastest a drone that keeps disagreeing is re-sent one.
///
/// The settle time: the hero bit reaches the ground through the drone's video
/// sidecar, its state snapshot and its next beacon, so a drone that has just
/// taken an assignment still beacons the old one for a moment. The rate bound: a
/// drone whose bit never flips (an answer that never lands, a video service that
/// is still starting) is chased once per holdoff, not once per tick. Longer than
/// a whole call plus its retry, so the tick never races a selection whose calls
/// are still in flight.
pub const REISSUE_HOLDOFF: Duration = Duration::from_secs(30);
const _: () = assert!(REISSUE_HOLDOFF.as_secs() > 2 * RPC_DEFAULT_TIMEOUT.as_secs());

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
    /// The current hero's device id, or `None` before the first selection has
    /// had its promotion answered.
    hero: Option<String>,
    /// Assignments asked for but not confirmed, retried by the reconcile tick.
    unconfirmed: BTreeMap<String, VideoProfile>,
    /// What the current selection assigns each drone. A late answer from an
    /// older operation is checked against this.
    wanted: BTreeMap<String, VideoProfile>,
    /// Bumped by every selection and carried back by every outcome, so a slow
    /// answer to an older selection is dropped instead of overwriting a newer
    /// one. Selections are no longer serialized behind the request that made
    /// them, so two can overlap.
    generation: u64,
    /// When each drone was last sent an assignment, for [`REISSUE_HOLDOFF`].
    issued: BTreeMap<String, Instant>,
    /// Calls sent and not yet answered, per drone: the generation and profile
    /// each one carries.
    in_flight: BTreeMap<String, Vec<(u64, VideoProfile)>>,
}

impl Inner {
    /// Stamp and track every call a plan is about to send.
    fn issue(&mut self, plan: &HeroPlan, generation: u64, now: Instant) {
        for t in &plan.targets {
            self.issued.insert(t.device_id.clone(), now);
            self.in_flight
                .entry(t.device_id.clone())
                .or_default()
                .push((generation, t.profile));
        }
    }

    /// Record one answered call, current or stale.
    fn settle(&mut self, generation: u64, outcome: &SlotOutcome) {
        let id = &outcome.device_id;
        if let Some(calls) = self.in_flight.get_mut(id) {
            if let Some(i) = calls
                .iter()
                .position(|c| *c == (generation, outcome.profile))
            {
                calls.remove(i);
            }
            if calls.is_empty() {
                self.in_flight.remove(id);
            }
        }
        if generation != self.generation {
            self.note_stale(outcome);
        }
    }

    /// A current-generation answer: a confirmation clears the drone unless an
    /// older call carrying a different profile is still out (it may land
    /// after this one); a failure is queued.
    fn note(&mut self, outcome: &SlotOutcome) {
        let contested = self
            .in_flight
            .get(&outcome.device_id)
            .is_some_and(|calls| calls.iter().any(|(_, p)| *p != outcome.profile));
        if outcome.ok && !contested {
            self.unconfirmed.remove(&outcome.device_id);
        } else {
            self.unconfirmed
                .insert(outcome.device_id.clone(), outcome.profile);
        }
    }

    /// An answer from an older operation. A confirmed one landed on the drone,
    /// possibly after the current selection's call, so a profile that
    /// contradicts the current assignment queues that assignment again. A
    /// failed one most likely never landed; if it did, the beacon holdoff
    /// catches the disagreement.
    fn note_stale(&mut self, outcome: &SlotOutcome) {
        if !outcome.ok {
            return;
        }
        if let Some(want) = self.wanted.get(&outcome.device_id).copied() {
            if want != outcome.profile {
                self.unconfirmed.insert(outcome.device_id.clone(), want);
            }
        }
    }
}

/// What one reconcile tick decides from.
#[derive(Debug, Clone, Default)]
pub(super) struct TickInputs {
    pub hero: Option<String>,
    pub unconfirmed: BTreeMap<String, VideoProfile>,
    /// Drones sent an assignment within [`REISSUE_HOLDOFF`]: their beacon may
    /// not have caught up yet.
    pub settling: BTreeSet<String>,
    /// The selection these inputs belong to. A pass that finds a newer one
    /// begun by the time it would send issues nothing.
    pub generation: u64,
}

impl FleetHeroState {
    /// The currently selected hero.
    pub async fn hero(&self) -> Option<String> {
        self.inner.lock().await.hero.clone()
    }

    /// Open a hero selection and return its generation.
    ///
    /// The plan names every registered drone, so it supersedes whatever an
    /// earlier selection left queued. Every target is stamped as issued now,
    /// which keeps the reconcile tick off them while their calls are in flight.
    pub async fn begin_selection(&self, plan: &HeroPlan, now: Instant) -> u64 {
        let mut inner = self.inner.lock().await;
        inner.generation += 1;
        inner.unconfirmed.clear();
        inner.wanted = plan
            .targets
            .iter()
            .map(|t| (t.device_id.clone(), t.profile))
            .collect();
        let generation = inner.generation;
        inner.issue(plan, generation, now);
        generation
    }

    /// Record the hero's own outcome: this is where a selection takes effect,
    /// and it is sticky whether or not the promotion took (a failed one is
    /// queued like any other). Returns `false`, recording nothing but a stale
    /// contradiction, when a newer selection has begun since.
    pub async fn record_hero(&self, generation: u64, hero: &str, outcome: &SlotOutcome) -> bool {
        let mut inner = self.inner.lock().await;
        inner.settle(generation, outcome);
        if inner.generation != generation {
            return false;
        }
        inner.hero = Some(hero.to_string());
        inner.note(outcome);
        true
    }

    /// Record one demotion's outcome: a confirmation clears, a failure is
    /// queued for the reconcile tick. A newer selection having begun since
    /// leaves only a stale contradiction to queue.
    pub async fn record_outcome(&self, generation: u64, outcome: &SlotOutcome) {
        let mut inner = self.inner.lock().await;
        inner.settle(generation, outcome);
        if inner.generation == generation {
            inner.note(outcome);
        }
    }

    /// Stamp a reconcile pass's targets as issued now and return the generation
    /// its outcomes belong to, or `None` when a selection newer than the one
    /// the pass decided from has begun: the pass then sends nothing, because
    /// what it decided may contradict the new selection.
    pub async fn begin_reissue(
        &self,
        plan: &HeroPlan,
        now: Instant,
        decided_at: u64,
    ) -> Option<u64> {
        let mut inner = self.inner.lock().await;
        if inner.generation != decided_at {
            return None;
        }
        inner.issue(plan, decided_at, now);
        Some(decided_at)
    }

    /// Record a reconcile pass: confirmations clear. A failure changes nothing:
    /// a queued one is chased again next tick, and a disagreement the beacon
    /// showed is re-checked once the holdoff has passed.
    pub async fn record_reissue(&self, generation: u64, outcomes: &[SlotOutcome]) {
        let mut inner = self.inner.lock().await;
        for o in outcomes {
            inner.settle(generation, o);
            if inner.generation == generation && o.ok {
                inner.note(o);
            }
        }
    }

    /// The hero, once its promotion has confirmed: what the fan-out may serve.
    pub async fn confirmed_hero(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        inner
            .hero
            .clone()
            .filter(|h| !inner.unconfirmed.contains_key(h))
    }

    /// Drop everything held for drones that have left the fleet — chasing an
    /// unpaired drone forever would be a permanent radio cost for nothing.
    pub async fn prune(&self, slots: &[FleetSlot]) {
        let mut inner = self.inner.lock().await;
        let present = |id: &String| slots.iter().any(|s| &s.device_id == id);
        inner.unconfirmed.retain(|id, _| present(id));
        inner.issued.retain(|id, _| present(id));
    }

    /// The current selection, everything still awaiting confirmation, and the
    /// drones still settling at `now`.
    pub(super) async fn tick_inputs(&self, now: Instant) -> TickInputs {
        let inner = self.inner.lock().await;
        TickInputs {
            hero: inner.hero.clone(),
            unconfirmed: inner.unconfirmed.clone(),
            settling: inner
                .issued
                .iter()
                .filter(|(_, at)| now.saturating_duration_since(**at) < REISSUE_HOLDOFF)
                .map(|(id, _)| id.clone())
                .collect(),
            generation: inner.generation,
        }
    }
}

// ---------------------------------------------------------------------------
// publishing the selection to the fan-out
// ---------------------------------------------------------------------------

/// Publish `hero`'s slot so the ground station's video fan-out serves it.
///
/// The selection is made here; the fan-out that feeds the mediamtx ingest and
/// the LCD tap runs in `ados-groundlink`, in another process. Without this hop
/// that fan-out served whichever slot its generation started on — the LOWEST
/// registered one — so promoting a hero on any other slot left the operator
/// watching a different aircraft at 1 fps while the drone they picked streamed
/// full video to nobody. A run-dir sidecar rather than another IPC channel, so a
/// restart on either side re-derives its view from disk instead of the two
/// drifting apart in silence.
///
/// Idempotent: a file that already names this slot and device is left alone, so
/// the reconcile tick can call this every few seconds without rewriting tmpfs
/// for a settled fleet. Returns `true` when it wrote.
///
/// A hero the registry does not know publishes NOTHING and returns `false`.
/// Leaving the previous value in place is the safe answer: the fan-out validates
/// what it reads against the same registry, so a selection whose drone has left
/// is already treated as no selection there.
pub(super) fn publish_hero_to(path: &Path, slots: &[FleetSlot], hero: &str) -> bool {
    let Some(slot) = fanout::hero_slot(slots, hero) else {
        return false;
    };
    let already = ados_groundlink::read_hero_from(path)
        .is_some_and(|h| h.slot == slot && h.device_id == hero);
    if already {
        return false;
    }
    match ados_groundlink::write_hero_to(path, slot, hero) {
        Ok(()) => {
            tracing::info!(device_id = %hero, slot, "fleet_hero_published_for_fanout");
            true
        }
        Err(e) => {
            // Best-effort by necessity: the promotion already went out over the
            // radio, so an unwritable run dir must not fail the operator's
            // selection. Logged loudly because the visible symptom — the wrong
            // drone on screen — gives no hint of the cause, and the next
            // reconcile tick retries.
            tracing::error!(
                error = %e,
                path = %path.display(),
                device_id = %hero,
                slot,
                "fleet_hero_publish_failed_fanout_will_serve_the_wrong_drone"
            );
            false
        }
    }
}

/// Where the fan-out reads the published hero.
pub(super) fn hero_sidecar() -> PathBuf {
    PathBuf::from(ados_groundlink::hero_path())
}

/// The process-wide fleet attention state.
static FLEET_HERO_STATE: LazyLock<Arc<FleetHeroState>> = LazyLock::new(Arc::default);

/// The process-wide fleet attention state, shared by the route and the
/// reconcile ticker so they can never disagree about the current selection.
/// An `Arc` so a selection running on its own task can outlive the request
/// that started it.
pub fn fleet_hero_state() -> Arc<FleetHeroState> {
    Arc::clone(&FLEET_HERO_STATE)
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

    // The selection runs on its own task and this handler only awaits its
    // handle: a client that times out, or a connection that drops mid-request,
    // no longer aborts the calls in flight or skips recording and publishing
    // the hero.
    let caller = profile_caller(proxy, Arc::from(slots.as_slice()));
    let selected = select_hero(
        fleet_hero_state(),
        slots,
        plan,
        device_id.to_string(),
        caller,
        hero_sidecar(),
    )
    .await;
    let report = match selected {
        Ok(report) => report,
        Err(e) => {
            tracing::error!(error = %e, device_id = %device_id, "fleet_hero_selection_task_failed");
            return detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "hero selection task failed",
            );
        }
    };

    (
        outcome_status(&report),
        Json(outcome_body(device_id, &report)),
    )
        .into_response()
}

/// `200` when every registered drone confirmed; `502 Bad Gateway` when the
/// hero's OWN promotion failed, because the drone the operator picked is still
/// a thumbnail; `207 Multi-Status` when only demotions failed or have not
/// answered yet. Each carries the same per-slot body.
fn outcome_status(report: &SelectionReport) -> StatusCode {
    if report.hero_failed() {
        StatusCode::BAD_GATEWAY
    } else if report.complete() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    }
}

/// The response body: the selected hero plus one row per registered slot, in
/// slot order. A `pending` row is a drone still being called when the report
/// was cut; it is neither a success nor a failure yet.
fn outcome_body(hero: &str, report: &SelectionReport) -> Value {
    let resolved = report.resolved.iter().map(|o| {
        (
            o.slot,
            json!({
                "slot": o.slot,
                "device_id": o.device_id,
                "profile": o.profile.as_str(),
                "ok": o.ok,
                "pending": false,
                "error": o.error,
            }),
        )
    });
    let pending = report.pending.iter().map(|t| {
        (
            t.slot,
            json!({
                "slot": t.slot,
                "device_id": t.device_id,
                "profile": t.profile.as_str(),
                "ok": false,
                "pending": true,
                "error": Value::Null,
            }),
        )
    });
    let mut rows: Vec<(u8, Value)> = resolved.chain(pending).collect();
    rows.sort_by_key(|(slot, _)| *slot);
    json!({
        "hero": hero,
        "slots": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
    })
}

/// The per-drone call: a targeted `POST /api/video/profile` over the aux lane,
/// carrying the relay ticket minted from `slots` for that drone.
///
/// The drone refuses a relayed request without a ticket it can verify, so the
/// ticket is not optional here. It is minted per attempt, so a retry never
/// presents one that expired while the first attempt timed out.
pub(super) fn profile_caller(
    proxy: Arc<AuxRpcProxy>,
    slots: Arc<[FleetSlot]>,
) -> impl Fn(String, VideoProfile) -> ProfileCall + Clone + Send + 'static {
    move |device_id: String, profile: VideoProfile| {
        let proxy = Arc::clone(&proxy);
        let ticket = mint_ticket(&slots, &device_id);
        ProfileCall(Box::pin(async move {
            let body = json!({"profile": profile.as_str()}).to_string();
            match proxy
                .call_with_ticket(
                    device_id.as_bytes(),
                    RpcMethod::Post,
                    DRONE_PROFILE_PATH,
                    body.as_bytes(),
                    ticket.as_bytes(),
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

/// Fixtures shared by this module's tests and its submodules'.
#[cfg(test)]
pub(super) mod tests_support {
    use super::*;

    pub fn slots(ids: &[&str]) -> Vec<FleetSlot> {
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
        // Nothing to mint from, so the call goes out bare and the drone's
        // refusal is surfaced rather than read as a success.
        const DRONE: &str = "ados-hero-02";
        let drone = test_drone::spawn(DRONE, Some(SECRET)).await;
        let promote = profile_caller(Arc::clone(&drone.proxy), Arc::from(slots(&[DRONE])));
        let err = promote(DRONE.to_string(), VideoProfile::Hero)
            .await
            .expect_err("the drone refuses an unticketed call");
        assert!(err.contains("401"), "{err}");
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
}
