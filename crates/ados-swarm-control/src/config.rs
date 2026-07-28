//! The `swarm:` config slice, and the one place integer percentages become real
//! gains.
//!
//! The Mission Control config primitives have no float field, so every gain is
//! persisted as an INTEGER PERCENTAGE: `cohesion = 40` is 0.40,
//! `separation_gain = 150` is 1.50. That conversion happens exactly here, at the
//! boundary. A control law that divided by 100 itself would be one refactor away
//! from doing it twice.

use serde::Deserialize;

use crate::controller::SwarmMode;
use crate::flocking::FlockTuning;
use crate::formation::{FormationName, DEFAULT_SPACING_M};
use crate::separation::SeparationTuning;

fn default_role() -> String {
    "auto".to_string()
}
fn default_mode() -> String {
    "hold".to_string()
}
fn default_formation() -> String {
    "line".to_string()
}
fn default_spacing() -> i64 {
    10
}
fn default_cohesion_pct() -> i64 {
    40
}
fn default_alignment_pct() -> i64 {
    60
}
fn default_separation_gain_pct() -> i64 {
    150
}
fn default_flock_radius_m() -> i64 {
    30
}
fn default_flock_neighbors() -> i64 {
    7
}
fn default_separation_radius_m() -> i64 {
    8
}
fn default_separation_hard_m() -> i64 {
    4
}

/// Percentage-to-gain conversion. Clamped to a sane band so a hand-edited config
/// cannot hand a control law a gain of ten thousand.
fn pct_to_gain(pct: i64) -> f64 {
    pct.clamp(0, 1_000) as f64 / 100.0
}

/// Metres from an integer config field, falling back when out of range.
fn metres(value: i64, fallback: f64) -> f64 {
    if value > 0 && value <= 10_000 {
        value as f64
    } else {
        fallback
    }
}

/// `swarm.flock.*` — gains as integer percentages, radii as integer metres.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FlockConfig {
    #[serde(default = "default_cohesion_pct")]
    pub cohesion: i64,
    #[serde(default = "default_alignment_pct")]
    pub alignment: i64,
    #[serde(default = "default_separation_gain_pct")]
    pub separation_gain: i64,
    #[serde(default = "default_flock_radius_m")]
    pub radius_m: i64,
    #[serde(default = "default_flock_neighbors")]
    pub neighbors: i64,
}

impl Default for FlockConfig {
    fn default() -> Self {
        Self {
            cohesion: default_cohesion_pct(),
            alignment: default_alignment_pct(),
            separation_gain: default_separation_gain_pct(),
            radius_m: default_flock_radius_m(),
            neighbors: default_flock_neighbors(),
        }
    }
}

/// `swarm.separation.*` — the safety layer's two radii.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SeparationConfig {
    #[serde(default = "default_separation_radius_m")]
    pub radius_m: i64,
    #[serde(default = "default_separation_hard_m")]
    pub hard_m: i64,
}

impl Default for SeparationConfig {
    fn default() -> Self {
        Self {
            radius_m: default_separation_radius_m(),
            hard_m: default_separation_hard_m(),
        }
    }
}

/// `swarm.tasks.*`. `assigned_task_id` and `bundle_position` are AGENT-WRITTEN
/// status mirrors — this runtime fills them, the settings page renders them
/// read-only. Null until an auction has run.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct TasksConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub assigned_task_id: Option<String>,
    #[serde(default)]
    pub bundle_position: Option<i64>,
}

/// The `swarm:` block of `/etc/ados/config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SwarmControlConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_role")]
    pub role: String,
    /// The operator-commandable behaviour mode. `hard-separation` and `operator`
    /// are NOT values here: they are precedence levels the arbitration derives,
    /// not modes anybody can ask for.
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_formation")]
    pub default_formation: String,
    #[serde(default = "default_spacing")]
    pub default_spacing: i64,
    #[serde(default)]
    pub flock: FlockConfig,
    #[serde(default)]
    pub separation: SeparationConfig,
    #[serde(default)]
    pub tasks: TasksConfig,
    /// This node's fleet slot, read from `video.wfb.fleet_slot` — NOT from the
    /// `swarm:` block, and not settable there.
    ///
    /// Radio addressing owns it (`ados_radio::config::WfbConfig`) because it is
    /// what keys the node's transmitters, and the ground station's fleet registry
    /// assigns it at pair time. The control layer needs it for two things it
    /// cannot do without: the slot-indexed deconfliction climb and this drone's
    /// station in a formation table. Carried here so the field is read once at the
    /// crate boundary rather than plumbed through every law.
    #[serde(skip)]
    pub fleet_slot: u8,
}

impl Default for SwarmControlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            role: default_role(),
            mode: default_mode(),
            default_formation: default_formation(),
            default_spacing: default_spacing(),
            flock: FlockConfig::default(),
            separation: SeparationConfig::default(),
            tasks: TasksConfig::default(),
            fleet_slot: 0,
        }
    }
}

impl SwarmControlConfig {
    /// Load the `swarm:` block plus this node's `video.wfb.fleet_slot`.
    ///
    /// A malformed block surfaces on the Health view through the config-status
    /// sidecar rather than silently defaulting the autonomy layer off — a
    /// genuinely-disabled swarm and a mis-parsed one must not look identical.
    pub fn load_from(path: &std::path::Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            swarm: SwarmControlConfig,
            #[serde(default)]
            video: VideoSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct VideoSection {
            #[serde(default)]
            wfb: WfbSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct WfbSection {
            #[serde(default)]
            fleet_slot: u8,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let (raw, err) = ados_config::yaml_reporting::<RawConfig>(&text, "swarm-control");
        ados_config::write_config_status("swarm-control", err.as_deref());
        let mut cfg = raw.swarm;
        cfg.fleet_slot = raw.video.wfb.fleet_slot;
        cfg
    }

    /// The slot set this node knows about: itself plus every slot it can hear.
    ///
    /// A drone has no fleet registry — that lives on the ground station — so its
    /// formation table is generated from the fleet it can actually SEE. That is
    /// the right answer for a decentralized layer: a drone cannot hold station
    /// relative to an aircraft it is not hearing, and a table sized for a fleet
    /// member that has gone home would leave a permanent hole in the shape.
    pub fn visible_slots(&self, heard: impl IntoIterator<Item = u8>) -> Vec<u8> {
        let mut slots: Vec<u8> = heard.into_iter().collect();
        slots.push(self.fleet_slot);
        slots.sort_unstable();
        slots.dedup();
        slots
    }

    /// Separation gains, sanitised.
    pub fn separation_tuning(&self) -> SeparationTuning {
        SeparationTuning {
            radius_m: metres(
                self.separation.radius_m,
                crate::separation::SEPARATION_RADIUS_M,
            ),
            hard_m: metres(self.separation.hard_m, crate::separation::SEPARATION_HARD_M),
            gain: pct_to_gain(self.flock.separation_gain),
            neighbors: crate::separation::SEPARATION_NEIGHBORS,
        }
        .sanitised()
    }

    /// Flocking gains, sanitised. Reuses [`Self::separation_tuning`] so the
    /// repulsive term inside the flocking law and the safety layer can never
    /// disagree about the radii.
    pub fn flock_tuning(&self) -> FlockTuning {
        FlockTuning {
            radius_m: metres(self.flock.radius_m, crate::flocking::FLOCK_RADIUS_M),
            neighbors: self.flock.neighbors.clamp(1, 64) as usize,
            cohesion: pct_to_gain(self.flock.cohesion),
            alignment: pct_to_gain(self.flock.alignment),
            target: crate::flocking::FLOCK_TARGET,
            separation: self.separation_tuning(),
        }
        .sanitised()
    }

    pub fn formation_name(&self) -> FormationName {
        FormationName::from_wire(&self.default_formation)
    }

    pub fn swarm_mode(&self) -> SwarmMode {
        SwarmMode::from_wire(&self.mode)
    }

    pub fn spacing_m(&self) -> f64 {
        metres(self.default_spacing, DEFAULT_SPACING_M)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn defaults_match_the_python_config_model() {
        let c = SwarmControlConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.role, "auto");
        assert_eq!(c.mode, "hold");
        assert_eq!(c.default_formation, "line");
        assert_eq!(c.default_spacing, 10);
        assert_eq!(c.flock.cohesion, 40);
        assert_eq!(c.flock.alignment, 60);
        assert_eq!(c.flock.separation_gain, 150);
        assert_eq!(c.flock.radius_m, 30);
        assert_eq!(c.flock.neighbors, 7);
        assert_eq!(c.separation.radius_m, 8);
        assert_eq!(c.separation.hard_m, 4);
        assert!(!c.tasks.enabled);
        assert_eq!(c.tasks.assigned_task_id, None);
        assert_eq!(c.tasks.bundle_position, None);
    }

    #[test]
    fn default_percentages_become_the_plans_gains() {
        let c = SwarmControlConfig::default();
        let f = c.flock_tuning();
        assert!((f.cohesion - crate::flocking::FLOCK_COHESION).abs() < 1e-12);
        assert!((f.alignment - crate::flocking::FLOCK_ALIGNMENT).abs() < 1e-12);
        assert!((f.separation.gain - crate::separation::SEPARATION_GAIN).abs() < 1e-12);
        assert_eq!(f.radius_m, crate::flocking::FLOCK_RADIUS_M);
        assert_eq!(f.neighbors, crate::flocking::FLOCK_NEIGHBORS);
        let s = c.separation_tuning();
        assert_eq!(s.radius_m, crate::separation::SEPARATION_RADIUS_M);
        assert_eq!(s.hard_m, crate::separation::SEPARATION_HARD_M);
        assert_eq!(c.spacing_m(), DEFAULT_SPACING_M);
    }

    #[test]
    fn a_missing_file_or_block_is_the_default_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            SwarmControlConfig::load_from(&dir.path().join("absent.yaml")),
            SwarmControlConfig::default()
        );
        let p = write(
            dir.path(),
            "other.yaml",
            "video:\n  wfb:\n    channel: 149\n",
        );
        assert_eq!(
            SwarmControlConfig::load_from(&p),
            SwarmControlConfig::default()
        );
    }

    #[test]
    fn the_block_is_read_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "swarm.yaml",
            "swarm:\n  enabled: true\n  mode: formation\n  default_formation: wedge\n  default_spacing: 15\n  flock:\n    cohesion: 25\n    alignment: 90\n    separation_gain: 200\n    radius_m: 45\n    neighbors: 3\n  separation:\n    radius_m: 12\n    hard_m: 5\n  tasks:\n    enabled: true\n",
        );
        let c = SwarmControlConfig::load_from(&p);
        assert!(c.enabled);
        assert_eq!(c.swarm_mode(), SwarmMode::Formation);
        assert_eq!(c.formation_name(), FormationName::Wedge);
        assert_eq!(c.spacing_m(), 15.0);
        assert!(c.tasks.enabled);
        let f = c.flock_tuning();
        assert!((f.cohesion - 0.25).abs() < 1e-12);
        assert!((f.alignment - 0.90).abs() < 1e-12);
        assert!((f.separation.gain - 2.0).abs() < 1e-12);
        assert_eq!(f.radius_m, 45.0);
        assert_eq!(f.neighbors, 3);
        assert_eq!(f.separation.radius_m, 12.0);
        assert_eq!(f.separation.hard_m, 5.0);
    }

    #[test]
    fn a_partial_block_keeps_the_other_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "partial.yaml",
            "swarm:\n  flock:\n    cohesion: 5\n",
        );
        let c = SwarmControlConfig::load_from(&p);
        assert_eq!(c.flock.cohesion, 5);
        assert_eq!(c.flock.alignment, 60, "untouched sibling keeps its default");
        assert_eq!(c.separation.hard_m, 4);
        assert_eq!(c.mode, "hold");
    }

    #[test]
    fn a_malformed_block_degrades_to_defaults_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "bad.yaml",
            "swarm:\n  flock:\n    cohesion: not-a-number\n",
        );
        let c = SwarmControlConfig::load_from(&p);
        assert_eq!(c, SwarmControlConfig::default());
    }

    #[test]
    fn absurd_percentages_and_radii_are_clamped_not_flown() {
        assert_eq!(pct_to_gain(-500), 0.0);
        assert_eq!(pct_to_gain(999_999), 10.0);
        assert_eq!(pct_to_gain(150), 1.5);
        assert_eq!(metres(0, 8.0), 8.0);
        assert_eq!(metres(-3, 8.0), 8.0);
        assert_eq!(metres(99_999, 8.0), 8.0);
        assert_eq!(metres(12, 8.0), 12.0);

        // An inverted safety pair is repaired, and the HARD radius is the one
        // that survives.
        let c = SwarmControlConfig {
            separation: SeparationConfig {
                radius_m: 3,
                hard_m: 9,
            },
            flock: FlockConfig {
                neighbors: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let s = c.separation_tuning();
        assert_eq!(s.hard_m, 9.0);
        assert!(s.radius_m > s.hard_m);
        // The flocking radius is widened past the repaired separation radius.
        let f = c.flock_tuning();
        assert!(f.radius_m > f.separation.radius_m, "{f:?}");
        assert_eq!(
            f.neighbors, 1,
            "a zero neighbour count is clamped, not honoured"
        );
    }

    #[test]
    fn an_unknown_mode_or_formation_falls_back_to_the_documented_default() {
        let c = SwarmControlConfig {
            mode: "murmuration".into(),
            default_formation: "diamond".into(),
            ..Default::default()
        };
        assert_eq!(
            c.swarm_mode(),
            SwarmMode::Hold,
            "an unknown mode must not fly"
        );
        assert_eq!(c.formation_name(), FormationName::Line);
    }
}
