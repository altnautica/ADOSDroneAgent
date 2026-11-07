//! The step contract and the context every step runs with.

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::adapters::Adapter;
use super::fcstate::FcState;
use super::out::Outbox;
use super::params::ParamStore;
use super::schema::{Action, StepState, StepView, WizardId};

/// What the pilot tells the setup engine each tick.
#[derive(Debug, Clone, Default)]
pub struct PilotInputs {
    /// Operator's shaped axes `[roll, pitch, thr, yaw]` (contract C1 signs).
    pub axes: [f32; 4],
    /// When the axes were last updated; `None` = no stick session.
    pub axes_ms: Option<u64>,
    /// The pilot's link-loss detector fired (no surface for `pilot.link_loss_s`).
    pub link_lost: bool,
    /// Blocking pilot readiness items other than `setup_needed` (message keys).
    pub readiness_blockers: Vec<String>,
}

/// Values carried between steps and persisted per aircraft.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WizardData {
    pub frame_class: Option<u8>,
    pub frame_type: Option<u8>,
    pub wing: Option<String>,
    pub prop_in: Option<f32>,
    pub motor_kv: Option<f32>,
    pub cells: Option<f32>,
    pub chemistry: Option<String>,
    pub notch_deferred: bool,
    pub hover_throttle: Option<f32>,
    /// First-hover attitude error RMS `[roll, pitch]` (deg).
    pub baseline_att_err_deg: Option<[f32; 2]>,
    /// First-hover step-response overshoot is not measured; verify compares
    /// its own box against these first-hover tracking numbers.
    pub baseline_track_err_deg: Option<[f32; 2]>,
    pub snapshot: Option<String>,
    pub area_confirmed: bool,
    /// Tuning-box centre `(n, e)` in LOCAL_NED.
    pub box_center: Option<[f32; 2]>,
    pub autotune_yaw_pending: bool,
    /// Tuning-box centre as `(lat, lon)` degE7, for fixed-wing loiters.
    pub box_latlon: Option<(i32, i32)>,
    /// Measured cruise throttle (%), first flight.
    pub cruise_throttle: Option<f32>,
}

/// Everything a step may read or drive during one call.
pub struct Cx<'a> {
    pub now: u64,
    pub wizard: WizardId,
    pub fc: &'a FcState,
    pub params: &'a mut ParamStore,
    pub out: &'a mut Outbox,
    pub data: &'a mut WizardData,
    pub adapter: Adapter,
    pub inputs: &'a PilotInputs,
    /// `true` when the `ground` wizard has completed for this aircraft.
    pub ground_done: bool,
    /// Body of the parameter snapshot taken before this aircraft's first
    /// parameter-changing step, for "Revert gains".
    pub snapshot: Option<&'a str>,
}

/// The mutable part of a step's view, shared by every step.
#[derive(Debug, Clone)]
pub struct StepCore {
    pub state: StepState,
    pub progress: Option<f32>,
    pub args: Map<String, Value>,
    pub image: Option<String>,
    pub live: Map<String, Value>,
    pub actions: Vec<Action>,
    pub blockers: Vec<String>,
    pub result: Option<Map<String, Value>>,
    /// Body text variant (`.plane`, `.px4`, …) appended to the body key.
    pub body_variant: Option<&'static str>,
    /// Sub-state the body text names (`setup.<step>.body.<phase>`), when set
    /// it replaces the variant.
    pub phase_key: Option<String>,
}

impl Default for StepCore {
    fn default() -> Self {
        StepCore {
            state: StepState::Pending,
            progress: None,
            args: Map::new(),
            image: None,
            live: Map::new(),
            actions: Vec::new(),
            blockers: Vec::new(),
            result: None,
            body_variant: None,
            phase_key: None,
        }
    }
}

impl StepCore {
    pub fn running(&mut self) {
        self.state = StepState::Running;
        self.actions.clear();
    }

    pub fn wait_user(&mut self, actions: Vec<Action>) {
        self.state = StepState::WaitingUser;
        self.actions = actions;
    }

    pub fn done(&mut self, result: Map<String, Value>) {
        self.state = StepState::Done;
        self.progress = Some(1.0);
        self.actions.clear();
        self.result = Some(result);
    }

    pub fn fail(&mut self, blocker: &str) {
        self.state = StepState::Failed;
        self.actions = vec![Action::primary("retry")];
        self.blockers = vec![blocker.to_string()];
    }

    pub fn skip(&mut self, reason: &str) {
        self.state = StepState::Skipped;
        self.actions.clear();
        self.result = Some(super::schema::obj([("reason", Value::from(reason))]));
    }

    pub fn set_live(&mut self, key: &str, v: impl Into<Value>) {
        self.live.insert(key.to_string(), v.into());
    }

    pub fn view(&self, wizard: WizardId, step: &str) -> StepView {
        let body = match (&self.phase_key, self.body_variant) {
            (Some(p), _) => format!("setup.{step}.body.{p}"),
            (None, Some(v)) => format!("setup.{step}.body.{v}"),
            (None, None) => format!("setup.{step}.body"),
        };
        StepView {
            wizard,
            step: step.to_string(),
            title_key: format!("setup.{step}.title"),
            body_key: body,
            args: self.args.clone(),
            image: self.image.clone(),
            state: self.state,
            progress: self.progress,
            live: self.live.clone(),
            actions: self.actions.clone(),
            blockers: self.blockers.clone(),
            result: self.result.clone(),
        }
    }
}

/// Errors an action returns (`setup.err.<code>`).
pub type ActErr = &'static str;

pub trait Step: Send {
    fn id(&self) -> &'static str;
    fn core(&self) -> &StepCore;
    fn core_mut(&mut self) -> &mut StepCore;

    /// `Some(reason)` when this step does not apply to this aircraft; the
    /// engine marks it skipped with `setup.blocker.<reason>`.
    fn skip_reason(&self, _cx: &Cx) -> Option<&'static str> {
        None
    }

    /// The operator may skip it.
    fn optional(&self) -> bool {
        false
    }

    /// The step flies: the air abort monitor applies.
    fn flight(&self) -> bool {
        false
    }

    /// Operator sticks are blended in (AUTOTUNE) rather than a takeover.
    fn blends_sticks(&self) -> bool {
        false
    }

    fn begin(&mut self, cx: &mut Cx);
    fn on_msg(&mut self, _msg: &MavMessage, _cx: &mut Cx) {}
    fn tick(&mut self, cx: &mut Cx);
    fn action(&mut self, id: &str, args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr>;

    /// Stop whatever the step is doing (the engine handles landing).
    fn abort(&mut self, _cx: &mut Cx) {}

    /// The engine skipped this step with `reason`.
    fn on_skipped(&mut self, _reason: &str, _cx: &mut Cx) {}
}

/// A numeric argument.
pub fn arg_f32(args: &Map<String, Value>, key: &str) -> Option<f32> {
    args.get(key).and_then(Value::as_f64).map(|v| v as f32)
}

pub fn arg_str<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// `id`/`core`/`core_mut` for a step struct with a `core: StepCore` field.
macro_rules! step_core {
    ($id:literal) => {
        fn id(&self) -> &'static str {
            $id
        }
        fn core(&self) -> &$crate::setup::step::StepCore {
            &self.core
        }
        fn core_mut(&mut self) -> &mut $crate::setup::step::StepCore {
            &mut self.core
        }
    };
}
pub(crate) use step_core;

/// Reboots the FC and waits for it to come back with its parameter table.
#[derive(Debug, Clone, Default)]
pub struct Reboot {
    sent_ms: Option<u64>,
    requested_params: bool,
}

impl Reboot {
    pub fn start(&mut self, cx: &mut Cx) {
        cx.out.reboot();
        self.sent_ms = Some(cx.now);
        self.requested_params = false;
    }

    pub fn active(&self) -> bool {
        self.sent_ms.is_some()
    }

    /// `true` once the FC is back and its parameters are downloaded again.
    pub fn tick(&mut self, cx: &mut Cx) -> bool {
        let Some(sent) = self.sent_ms else { return true };
        // A heartbeat after a 3 s quiet period is the rebooted FC.
        let back = cx.now.saturating_sub(sent) > 3000
            && cx.fc.heartbeat_ms.is_some_and(|t| t > sent + 3000);
        if !back {
            return false;
        }
        if !self.requested_params {
            self.requested_params = true;
            let mut msgs = Vec::new();
            cx.params.request_all(&mut msgs, cx.now);
            cx.out.msgs.extend(msgs);
            return false;
        }
        if cx.params.complete() {
            self.sent_ms = None;
            return true;
        }
        false
    }
}

/// Waits for queued parameter writes. `Some(Ok)` when all landed,
/// `Some(Err(names))` when some failed, `None` while in flight.
pub fn writes_settled(cx: &mut Cx) -> Option<Result<(), Vec<String>>> {
    if !cx.params.idle() {
        return None;
    }
    let failed = cx.params.take_failed();
    Some(if failed.is_empty() { Ok(()) } else { Err(failed) })
}
