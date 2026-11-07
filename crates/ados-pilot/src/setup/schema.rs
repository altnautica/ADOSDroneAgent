//! Wire shapes of the flight-controller setup engine.
//!
//! Every surface renders these generically: a step carries its text keys,
//! image key, live values, actions and blockers, and the surface draws them
//! without knowing what the step does. Text keys resolve through the cockpit
//! spec's `setup-strings.json`; image keys name `setup-images/<key>.svg`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Wizard identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WizardId {
    Ground,
    Air,
}

impl WizardId {
    pub fn as_str(self) -> &'static str {
        match self {
            WizardId::Ground => "ground",
            WizardId::Air => "air",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ground" => Some(WizardId::Ground),
            "air" => Some(WizardId::Air),
            _ => None,
        }
    }
}

/// State of one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Running,
    WaitingUser,
    Done,
    Failed,
    Skipped,
}

impl StepState {
    /// A finished step: the wizard moves past it.
    pub fn is_terminal(self) -> bool {
        matches!(self, StepState::Done | StepState::Skipped | StepState::Failed)
    }
}

/// State of one wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WizardState {
    NotStarted,
    Running,
    Done,
    Failed,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStyle {
    Primary,
    Secondary,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirm {
    None,
    Hold,
    HoldPropsOff,
}

/// One button a step offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub label_key: String,
    pub style: ActionStyle,
    pub confirm: Confirm,
}

impl Action {
    pub fn new(id: &str, style: ActionStyle, confirm: Confirm) -> Self {
        Action {
            id: id.to_string(),
            label_key: format!("setup.action.{id}"),
            style,
            confirm,
        }
    }

    pub fn primary(id: &str) -> Self {
        Self::new(id, ActionStyle::Primary, Confirm::None)
    }

    pub fn secondary(id: &str) -> Self {
        Self::new(id, ActionStyle::Secondary, Confirm::None)
    }

    pub fn danger(id: &str) -> Self {
        Self::new(id, ActionStyle::Danger, Confirm::Hold)
    }
}

/// The step frame every surface renders (contract C9 step schema).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepView {
    pub wizard: WizardId,
    pub step: String,
    pub title_key: String,
    pub body_key: String,
    pub args: Map<String, Value>,
    pub image: Option<String>,
    pub state: StepState,
    pub progress: Option<f32>,
    pub live: Map<String, Value>,
    pub actions: Vec<Action>,
    pub blockers: Vec<String>,
    pub result: Option<Map<String, Value>>,
}

/// One wizard as `GET /api/v1/fc-setup` lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WizardView {
    pub id: WizardId,
    pub state: WizardState,
    pub available: bool,
    pub blockers: Vec<String>,
    pub completed_at: Option<String>,
    pub steps: Vec<StepView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Running {
    pub wizard: WizardId,
    pub step: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRef {
    pub file: String,
    pub ts: String,
}

/// The per-aircraft setup report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub date: Option<String>,
    pub firmware: Option<String>,
    pub firmware_version: Option<String>,
    pub vehicle_class: Option<String>,
    pub steps: Map<String, Value>,
    pub snapshots: Vec<SnapshotRef>,
}

/// Body of `GET /api/v1/fc-setup`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupView {
    pub v: u32,
    pub uid: Option<String>,
    pub firmware: Option<String>,
    pub firmware_version: Option<String>,
    pub vehicle_class: Option<String>,
    pub running: Option<Running>,
    pub wizards: Vec<WizardView>,
    pub report: Report,
}

/// One frame of `WS /api/v1/fc-setup/stream`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamFrame {
    Step { ts_ms: u64, data: StepView },
    Wizard { ts_ms: u64, data: WizardView },
    Keepalive { ts_ms: u64 },
}

/// Builds a JSON object from `(key, value)` pairs.
pub fn obj<I, K>(pairs: I) -> Map<String, Value>
where
    I: IntoIterator<Item = (K, Value)>,
    K: Into<String>,
{
    pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()
}

/// An editable input a step offers (`args.inputs.<name>`).
pub fn input_number(value: f64, min: f64, max: f64, step: f64, unit: &str, label_key: &str) -> Value {
    serde_json::json!({
        "value": value, "kind": "number", "min": min, "max": max, "step": step,
        "unit": unit, "label_key": label_key,
    })
}

/// An enum input: `options` are the values, labelled by `<label_key>.<option>`.
pub fn input_enum(value: Value, options: Vec<Value>, label_key: &str) -> Value {
    serde_json::json!({ "value": value, "kind": "enum", "options": options, "label_key": label_key })
}
