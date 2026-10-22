//! The swarm configuration block, and the one place integer percentages become
//! real gains.
//!
//! Many configuration front ends have no float field, so every gain is
//! persisted as an INTEGER PERCENTAGE: `cohesion = 40` is 0.40,
//! `separation_gain = 150` is 1.50. That conversion happens exactly here, at the
//! boundary. A control law that divided by 100 itself would be one refactor away
//! from doing it twice.
//!
//! Every struct here derives `serde::Deserialize`, so a host can read the block
//! from YAML, TOML or JSON. Field names (`flock.cohesion`, `separation.hard_m`,
//! ...) are examples of one layout, not a required file format.

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

/// `swarm.flock.*` - gains as integer percentages, radii as integer metres.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FlockConfig {
    #[serde(default = "default_cohesion_pct")]
    /// Cohesion gain, integer percent.
    pub cohesion: i64,
    #[serde(default = "default_alignment_pct")]
    /// Alignment gain, integer percent.
    pub alignment: i64,
    #[serde(default = "default_separation_gain_pct")]
    /// Separation gain, integer percent.
    pub separation_gain: i64,
    #[serde(default = "default_flock_radius_m")]
    /// Flocking neighbourhood radius, metres.
    pub radius_m: i64,
    #[serde(default = "default_flock_neighbors")]
    /// Neighbours weighted by the flocking law.
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

/// `swarm.separation.*` - the safety layer's two radii.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SeparationConfig {
    #[serde(default = "default_separation_radius_m")]
    /// Soft separation radius, metres.
    pub radius_m: i64,
    #[serde(default = "default_separation_hard_m")]
    /// Hard separation floor, metres.
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

/// Task allocation settings. `assigned_task_id` and `bundle_position` are
/// status mirrors written by the runtime, not operator inputs. `None` until an
/// auction has run.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct TasksConfig {
    #[serde(default)]
    /// Whether task allocation runs.
    pub enabled: bool,
    #[serde(default)]
    /// The task this node currently holds, if any.
    pub assigned_task_id: Option<String>,
    #[serde(default)]
    /// Its position in this node's bundle, if any.
    pub bundle_position: Option<i64>,
}

/// The swarm control configuration block.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SwarmControlConfig {
    #[serde(default)]
    /// Master switch: nothing is commanded while false.
    pub enabled: bool,
    #[serde(default = "default_role")]
    /// Node role hint (`auto` by default).
    pub role: String,
    /// The operator-commandable behaviour mode. `hard-separation` and `operator`
    /// are NOT values here: they are precedence levels the arbitration derives,
    /// not modes anybody can ask for.
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_formation")]
    /// Formation used when the mode is `formation` (wire name).
    pub default_formation: String,
    #[serde(default = "default_spacing")]
    /// Formation spacing, metres.
    pub default_spacing: i64,
    #[serde(default)]
    /// Flocking gains.
    pub flock: FlockConfig,
    #[serde(default)]
    /// Separation radii.
    pub separation: SeparationConfig,
    #[serde(default)]
    /// Task allocation settings.
    pub tasks: TasksConfig,
    /// This node's fleet slot.
    ///
    /// Never deserialized from the swarm block: radio addressing usually owns
    /// the slot, so the host fills it in after loading. The control layer needs
    /// it for the slot-indexed deconfliction climb and for this drone's station
    /// in a formation table, so it is carried here and read once at the crate
    /// boundary rather than plumbed through every law.
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
    /// The slot set this node knows about: itself plus every slot it can hear.
    ///
    /// A drone has no fleet registry - that lives on the ground station - so its
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

    /// The configured formation, falling back to `line` for an unknown name.
    pub fn formation_name(&self) -> FormationName {
        FormationName::from_wire(&self.default_formation)
    }

    /// The configured mode, falling back to `hold` for an unknown name.
    pub fn swarm_mode(&self) -> SwarmMode {
        SwarmMode::from_wire(&self.mode)
    }

    /// Formation spacing in metres, sanitised.
    pub fn spacing_m(&self) -> f64 {
        metres(self.default_spacing, DEFAULT_SPACING_M)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_values() {
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
