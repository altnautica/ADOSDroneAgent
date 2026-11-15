//! The `control:` and `pilot:` config slices, and persisting live changes.

use std::path::{Path, PathBuf};

use ados_protocol::pilot::{AvoidMode, ControlMode, PlaceClass, Profile};
use serde::{Deserialize, Serialize};

/// Default agent config path.
pub const CONFIG_PATH: &str = "/etc/ados/config.yaml";

/// The agent config path (`ADOS_CONFIG` overrides it, as for every service).
pub fn config_path() -> PathBuf {
    std::env::var_os("ADOS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(CONFIG_PATH))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlSection {
    pub mode: ControlMode,
    pub rc_ados_position: u8,
}

impl Default for ControlSection {
    fn default() -> Self {
        Self {
            mode: ControlMode::Ados,
            rc_ados_position: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeightRef {
    #[default]
    Agl,
    Ekf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkLossAction {
    #[default]
    Rth,
    Hold,
    Land,
}

impl LinkLossAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rth => "rth",
            Self::Hold => "hold",
            Self::Land => "land",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PilotSection {
    pub profile: Profile,
    pub avoid: AvoidMode,
    pub height_ref: HeightRef,
    pub obstacle_margin_m: f32,
    /// The kind of place flown: `open`, or `confined` (a margin from the
    /// propeller tips, the mission route check and the slow cap near walls).
    /// The operator's choice only.
    pub place_class: PlaceClass,
    /// The airframe's outermost rigid extent in the body x-y plane from the
    /// CG, metres: the largest of the propeller tips, guards or cage, and
    /// rigid mounts (a guarded frame enters the guard radius). A confined
    /// place needs it.
    pub prop_tip_radius_m: Option<f32>,
    pub max_alt_agl_m: f32,
    pub max_dist_m: f32,
    pub min_agl_fw_m: f32,
    pub link_loss_s: u32,
    pub link_loss_action: LinkLossAction,
    /// Horizontal speed cap (m/s) while the EKF's horizontal position does
    /// not come from GNSS.
    pub gnss_denied_max_speed_mps: f32,
    /// Horizontal acceleration cap (m/s²) under the same condition.
    pub gnss_denied_max_accel_mps2: f32,
    /// No progress to a mission waypoint (or on a return) over this window
    /// raises a stall.
    pub stall_window_s: u32,
    /// The progress (m) the window needs.
    pub stall_min_progress_m: f32,
    /// A stalled waypoint is skipped this long after the alert.
    pub stall_skip_after_s: u32,
    /// After this many automatic skips in a row the vehicle holds for the
    /// operator.
    pub stall_max_auto_skips: u32,
    /// A blocked return lands in place this long after the alert.
    pub return_blocked_land_after_s: u32,
    /// The announced countdown before the low-battery action.
    pub low_battery_countdown_s: u32,
    /// The declared flow and rangefinder mount, checked against the FC's
    /// parameters.
    pub fc_sensors: FcSensors,
}

/// The smallest obstacle margin in an open place, metres.
pub const OPEN_MARGIN_FLOOR_M: f32 = 0.5;
/// What a confined margin keeps beyond the propeller tips, metres.
pub const CONFINED_TIP_EXTRA_M: f32 = 0.1;
/// The body radius the brake assumes when `prop_tip_radius_m` is unset,
/// metres: the x500 class (0.5 m motor diagonal, 10" props) and smaller. A
/// confined place requires the key, so never flies this.
pub const DEFAULT_BODY_RADIUS_M: f32 = 0.4;

impl PilotSection {
    /// The vehicle's outermost rigid extent from the CG the brake keeps
    /// clear: `prop_tip_radius_m` when set, else [`DEFAULT_BODY_RADIUS_M`].
    pub fn body_radius_m(&self) -> f32 {
        self.prop_tip_radius_m.unwrap_or(DEFAULT_BODY_RADIUS_M)
    }

    /// The margin floor of the place class: 0.5 m in an open place, the
    /// propeller-tip radius plus 0.1 m in a confined one (0.5 m while the
    /// radius is unknown; the confined launch is refused then).
    pub fn margin_floor_m(&self) -> f32 {
        match (self.place_class, self.prop_tip_radius_m) {
            (PlaceClass::Confined, Some(r)) => r + CONFINED_TIP_EXTRA_M,
            _ => OPEN_MARGIN_FLOOR_M,
        }
    }

    /// `margin` held to the class floor: a skill's margin below it never
    /// flies.
    pub fn floored_margin_m(&self, margin: f32) -> f32 {
        margin.max(self.margin_floor_m())
    }

    /// The obstacle margin the pilot flies: in a confined place the
    /// propeller-tip radius plus 0.1 m, whatever the
    /// configured margin; otherwise `obstacle_margin_m`, the open margin, at
    /// least 0.5 m. The class alone switches between them, so a switch never
    /// needs the configured margin changed.
    pub fn effective_margin_m(&self) -> f32 {
        match (self.place_class, self.prop_tip_radius_m) {
            (PlaceClass::Confined, Some(r)) => r + CONFINED_TIP_EXTRA_M,
            _ => self.obstacle_margin_m.max(OPEN_MARGIN_FLOOR_M),
        }
    }
}

/// `pilot.fc_sensors`: where the flow module and the downward rangefinder sit,
/// body FRD metres from the CG. Every part is optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FcSensors {
    pub flow: Option<FlowMount>,
    pub rangefinder: Option<RangefinderMount>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowMount {
    pub pos_m: Option<[f64; 3]>,
    /// The flow sensor's yaw against the body, centidegrees (FLOW_ORIENT_YAW).
    pub orient_yaw_cdeg: Option<i32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RangefinderMount {
    pub pos_m: Option<[f64; 3]>,
}

impl Default for PilotSection {
    fn default() -> Self {
        Self {
            profile: Profile::Normal,
            avoid: AvoidMode::Brake,
            height_ref: HeightRef::Agl,
            obstacle_margin_m: 1.5,
            place_class: PlaceClass::Open,
            prop_tip_radius_m: None,
            max_alt_agl_m: 120.0,
            max_dist_m: 500.0,
            min_agl_fw_m: 30.0,
            link_loss_s: 10,
            link_loss_action: LinkLossAction::Rth,
            gnss_denied_max_speed_mps: 2.0,
            gnss_denied_max_accel_mps2: 1.5,
            stall_window_s: 15,
            stall_min_progress_m: 1.0,
            stall_skip_after_s: 45,
            stall_max_auto_skips: 2,
            return_blocked_land_after_s: 45,
            low_battery_countdown_s: 10,
            fc_sensors: FcSensors::default(),
        }
    }
}

/// Both slices.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub control: ControlSection,
    pub pilot: PilotSection,
}

impl Settings {
    /// Read both slices from the effective config (schema defaults overlaid
    /// with `config.yaml`). A malformed slice falls back to its default and is
    /// reported through the config-status sidecar.
    pub fn load(path: &Path) -> Self {
        let control = ados_config::schema::load_slice(path, "/control");
        let pilot = ados_config::schema::load_slice(path, "/pilot");
        let err = control.as_ref().err().or(pilot.as_ref().err()).cloned();
        ados_config::write_config_status("pilot", err.as_deref());
        Self {
            control: control.unwrap_or_default(),
            pilot: pilot.unwrap_or_default(),
        }
    }
}

/// Write `section.key = value` into the YAML config, preserving every other
/// key, through a temp file and rename.
pub fn persist(path: &Path, section: &str, key: &str, value: &str) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mut doc: serde_norway::Value = if text.trim().is_empty() {
        serde_norway::Value::Mapping(Default::default())
    } else {
        serde_norway::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?
    };
    let root = doc.as_mapping_mut().ok_or("config root is not a mapping")?;
    let entry = root
        .entry(serde_norway::Value::from(section))
        .or_insert_with(|| serde_norway::Value::Mapping(Default::default()));
    if !entry.is_mapping() {
        *entry = serde_norway::Value::Mapping(Default::default());
    }
    if let Some(m) = entry.as_mapping_mut() {
        // Integer-valued keys (the RC slot) are written as numbers.
        let v = value
            .parse::<i64>()
            .map(serde_norway::Value::from)
            .unwrap_or_else(|_| serde_norway::Value::from(value));
        m.insert(serde_norway::Value::from(key), v);
    }
    let out = serde_norway::to_string(&doc).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("yaml.pilot-tmp");
    std::fs::write(&tmp, out).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_match_the_schema_defaults() {
        ados_config::schema::assert_slice_conforms::<ControlSection>("/control");
        ados_config::schema::assert_slice_conforms::<PilotSection>("/pilot");
    }

    #[test]
    fn persist_keeps_unrelated_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(
            &p,
            "agent:\n  name: test\ncontrol:\n  rc_ados_position: 3\n",
        )
        .unwrap();
        persist(&p, "control", "mode", "fc_manual").unwrap();
        persist(&p, "pilot", "avoid", "off").unwrap();
        persist(&p, "control", "rc_ados_position", "4").unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.control.mode, ControlMode::FcManual);
        assert_eq!(s.control.rc_ados_position, 4);
        assert_eq!(s.pilot.avoid, AvoidMode::Off);
        assert!(std::fs::read_to_string(&p).unwrap().contains("name: test"));
    }

    #[test]
    fn the_declared_sensor_mount_loads() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(
            &p,
            "pilot:\n  fc_sensors:\n    flow:\n      pos_m: [0.05, 0.0, 0.02]\n      orient_yaw_cdeg: 9000\n",
        )
        .unwrap();
        let s = Settings::load(&p);
        let flow = s.pilot.fc_sensors.flow.expect("flow mount");
        assert_eq!(flow.pos_m, Some([0.05, 0.0, 0.02]));
        assert_eq!(flow.orient_yaw_cdeg, Some(9000));
        assert_eq!(s.pilot.fc_sensors.rangefinder, None);
    }

    #[test]
    fn a_confined_place_flies_the_tip_radius_plus_a_tenth() {
        let mut p = PilotSection {
            obstacle_margin_m: 0.3,
            ..PilotSection::default()
        };
        assert_eq!(p.effective_margin_m(), OPEN_MARGIN_FLOOR_M);
        p.obstacle_margin_m = 1.5;
        assert_eq!(p.effective_margin_m(), 1.5);
        // Confined: the tip radius plus 0.1 m, whatever the configured open one.
        p.place_class = PlaceClass::Confined;
        p.prop_tip_radius_m = Some(0.377);
        assert!((p.effective_margin_m() - 0.477).abs() < 1e-6);
        p.obstacle_margin_m = 0.6;
        assert!((p.effective_margin_m() - 0.477).abs() < 1e-6);
        // A skill's own margin is held to the floor, never lowered by it.
        assert!((p.floored_margin_m(0.2) - 0.477).abs() < 1e-6);
        assert_eq!(p.floored_margin_m(2.0), 2.0);
        // Confined without a radius keeps the open margin (the launch is
        // refused then).
        p.prop_tip_radius_m = None;
        assert_eq!(p.effective_margin_m(), 0.6);
        // Back to open: the configured margin again.
        p.place_class = PlaceClass::Open;
        p.prop_tip_radius_m = Some(0.377);
        assert_eq!(p.effective_margin_m(), 0.6);
    }

    #[test]
    fn the_place_class_switches_without_touching_the_margin() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(
            &p,
            "pilot:\n  place_class: confined\n  prop_tip_radius_m: 0.377\n  obstacle_margin_m: 1.5\n",
        )
        .unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.pilot.place_class, PlaceClass::Confined);
        assert_eq!(s.pilot.prop_tip_radius_m, Some(0.377));
        assert!((s.pilot.effective_margin_m() - 0.477).abs() < 1e-6);
        persist(&p, "pilot", "place_class", "open").unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.pilot.place_class, PlaceClass::Open);
        assert_eq!(s.pilot.obstacle_margin_m, 1.5);
        assert_eq!(s.pilot.effective_margin_m(), 1.5);
    }
}
