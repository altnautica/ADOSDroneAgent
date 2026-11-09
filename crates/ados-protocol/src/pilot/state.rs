//! The pilot's published state: the nav-stream `pilot` and `obstacles` kinds.

use serde::{Deserialize, Serialize};

/// Who currently commands motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    Idle,
    Pilot,
    Skill,
    Goto,
    Swarm,
    MissionFc,
    FcMode,
    Tuning,
}

/// Where operator sticks go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    #[default]
    Ados,
    FcManual,
    Elrs,
}

impl ControlMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ados => "ados",
            Self::FcManual => "fc_manual",
            Self::Elrs => "elrs",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ados" => Some(Self::Ados),
            "fc_manual" => Some(Self::FcManual),
            "elrs" => Some(Self::Elrs),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Cine,
    #[default]
    Normal,
    Sport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvoidMode {
    #[default]
    Brake,
    Bypass,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvoidState {
    #[default]
    Clear,
    Detour,
    Blocked,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VehicleClass {
    #[default]
    Multirotor,
    Fixedwing,
    Vtol,
}

/// The skill that holds the motion lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRef {
    pub plugin_id: String,
    pub skill_id: String,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Block,
    Warn,
}

/// One pre-flight readiness item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readiness {
    pub id: String,
    pub ok: bool,
    pub severity: Severity,
    pub msg: String,
}

/// FC mission state, computed by the router (`mission` of the vehicle state)
/// and copied here. `seq`, `total` and `age_ms` are null until the FC has
/// reported MISSION_CURRENT, as on the router's wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mission {
    pub state: String,
    pub seq: Option<u32>,
    pub total: Option<u32>,
    pub item_reached: Option<u32>,
    pub id: Option<u32>,
    pub source: String,
    pub age_ms: Option<u64>,
    pub landing_seq: bool,
}

/// The surface whose sticks are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSession {
    pub src: String,
    pub session: u32,
    pub label: String,
}

/// The centred-sticks handover gate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub pending: bool,
    pub progress: f64,
}

/// Progress of a `launch` sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    /// `checks`, `arm`, `takeoff`, `after` or `done`.
    pub step: String,
    /// `hold`, `skill`, `mission` or `goto`.
    pub after: String,
    pub error: Option<String>,
}

/// A transient full-width announcement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Banner {
    pub text_key: String,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub until_ms: u64,
}

/// The nav-stream `pilot` kind with the handover, launch and banner fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PilotState {
    pub owner: Owner,
    pub owner_label: String,
    pub skill: Option<SkillRef>,
    pub vehicle_class: VehicleClass,
    pub control_mode: ControlMode,
    pub profile: Profile,
    pub avoid: AvoidMode,
    pub fc_mode: u32,
    pub fc_mode_name: String,
    pub armed: bool,
    pub landed: bool,
    /// A VTOL between hover and forward flight (the FC's EXTENDED_SYS_STATE
    /// says it is transitioning either way).
    pub vtol_transitioning: bool,
    pub input_src: Option<String>,
    pub input_age_ms: Option<u64>,
    pub hold_reason: Option<String>,
    pub avoid_state: AvoidState,
    pub min_clearance_m: Option<f64>,
    pub goal_ned: Option<[f64; 3]>,
    pub plan_ned: Vec<[f64; 2]>,
    pub setpoint_ned: Option<[f64; 3]>,
    pub readiness: Vec<Readiness>,
    pub link_loss_in_s: Option<f64>,
    /// What the pilot does when the control link stays lost: `rth`, `hold`
    /// or `land` (the configured `pilot.link_loss_action`).
    pub link_loss_action: String,
    pub mission: Option<Mission>,
    pub input_session: Option<InputSession>,
    pub gate: Option<Gate>,
    pub sequence: Option<Sequence>,
    pub banner: Option<Banner>,
    pub rth_countdown_s: Option<f64>,
}

/// One camera's obstacle (or depth) grid: 32×18 cells, row-major, row 0 at
/// the top of the frame. `near` is base64 of one byte per cell (0 unknown,
/// 1..=255 log-coded distance, see [`decode_distance`]); `measured` is base64
/// of a bitmask, bit `i` = cell `i`, byte `i >> 3`, LSB first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraGrid {
    pub cols: u16,
    pub rows: u16,
    pub near: String,
    pub measured: String,
}

/// Grid width in cells.
pub const GRID_COLS: usize = 32;
/// Grid height in cells.
pub const GRID_ROWS: usize = 18;
/// Number of 5° body sectors.
pub const SECTORS: usize = 72;

/// The nav-stream `obstacles` kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObstaclesFrame {
    pub v: u8,
    pub ts_ms: u64,
    /// 72 sectors of 5°, body yaw 0 = nose, clockwise; `None` = no return.
    pub sectors_m: Vec<Option<f64>>,
    pub cameras: std::collections::BTreeMap<String, CameraGrid>,
}

/// A published float: millimetre precision, as f64 so JSON carries the
/// decimal value rather than its f32 expansion.
pub fn wire(v: f32) -> f64 {
    (f64::from(v) * 1000.0).round() / 1000.0
}

/// Distance in metres of a grid code (0 = unknown → `None`).
pub fn decode_distance(code: u8) -> Option<f32> {
    if code == 0 {
        return None;
    }
    Some(0.2 * ((f32::from(code) - 1.0) * 300f32.ln() / 254.0).exp())
}

/// The grid code of a distance in metres, clamped to 0.2–60 m.
pub fn encode_distance(d_m: f32) -> u8 {
    let d = d_m.clamp(0.2, 60.0);
    let v = 1.0 + 254.0 * (d / 0.2).ln() / 300f32.ln();
    v.round().clamp(1.0, 255.0) as u8
}
