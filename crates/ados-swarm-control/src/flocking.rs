//! Olfati-Saber α-lattice flocking.
//!
//! Four terms, summed into one velocity command:
//!
//! * COHESION toward the centroid of neighbours FARTHER than the separation
//!   radius.
//! * ALIGNMENT toward the mean neighbour velocity.
//! * SEPARATION, [`crate::separation::repulsion`] reused verbatim.
//! * A VIRTUAL LEADER pulling toward the operator-set target.
//!
//! # Why cohesion is gated at the separation radius
//!
//! Olfati-Saber's α-lattice comes from a SINGLE action function whose zero
//! crossing is the desired lattice constant. Splitting it into an independent
//! cohesion gain and an independent repulsion gain — which is what a
//! `cohesion = 0.4` / `separation_gain = 1.5` pair is — destroys that property:
//! solving `0.4·d = 1.5·(1/d − 1/8)` puts the equilibrium spacing at 1.65 m,
//! well inside the 4 m safety floor. Restricting cohesion to neighbours beyond
//! the separation radius (Reynolds' original formulation, where the three rules
//! act on their own neighbourhoods) restores it: cohesion pulls only from
//! outside the lattice constant, repulsion pushes only from inside, and the
//! equilibrium lands exactly ON [`crate::separation::SEPARATION_RADIUS_M`]. Both
//! gains stay at the plan's values.
//!
//! # Why the virtual leader saturates
//!
//! A proportional pull `FLOCK_TARGET · (target − p)` differs between two drones
//! by `FLOCK_TARGET · d` along the line joining them — 6.4 m/s of compression on
//! an 8 m lattice against a repulsive term of 0 at that range. The lattice
//! collapses along-track before it ever reaches the target. Saturating the term
//! makes it a UNIFORM field while the flock is far from the target: every drone
//! gets the same vector, so the flock TRANSLATES instead of compressing, and the
//! term only differentiates inside `cruise / FLOCK_TARGET` metres of the goal.

use crate::geo::Ned;
use crate::neighbor::{NearestSet, NeighborState};
use crate::separation::{repulsion_over, SeparationTuning};

/// Neighbours beyond this range are ignored by the flocking terms.
pub const FLOCK_RADIUS_M: f64 = 30.0;

/// How many nearest neighbours the flocking terms weight.
pub const FLOCK_NEIGHBORS: usize = 7;

/// Cohesion gain, 1/s.
pub const FLOCK_COHESION: f64 = 0.4;

/// Alignment gain, dimensionless.
pub const FLOCK_ALIGNMENT: f64 = 0.6;

/// Virtual-leader gain, 1/s.
pub const FLOCK_TARGET: f64 = 0.8;

/// The flocking gains, as configured. Defaults are the plan's constants; the
/// `swarm.flock.*` config block overrides them (integer percentages there, real
/// gains here — the conversion happens once, in [`crate::config`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlockTuning {
    pub radius_m: f64,
    pub neighbors: usize,
    pub cohesion: f64,
    pub alignment: f64,
    pub target: f64,
    pub separation: SeparationTuning,
}

impl Default for FlockTuning {
    fn default() -> Self {
        Self {
            radius_m: FLOCK_RADIUS_M,
            neighbors: FLOCK_NEIGHBORS,
            cohesion: FLOCK_COHESION,
            alignment: FLOCK_ALIGNMENT,
            target: FLOCK_TARGET,
            separation: SeparationTuning::default(),
        }
    }
}

impl FlockTuning {
    pub fn sanitised(mut self) -> Self {
        self.separation = self.separation.sanitised();
        if !self.radius_m.is_finite() || self.radius_m <= self.separation.radius_m {
            self.radius_m = FLOCK_RADIUS_M.max(self.separation.radius_m * 2.0);
        }
        if self.neighbors == 0 {
            self.neighbors = FLOCK_NEIGHBORS;
        }
        for g in [&mut self.cohesion, &mut self.alignment, &mut self.target] {
            if !g.is_finite() || *g < 0.0 {
                *g = 0.0;
            }
        }
        self
    }
}

/// The four flocking terms, kept separable so a test can pin one at a time and
/// so the diagnostics can show WHICH term is driving.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FlockTerms {
    pub cohesion: Ned,
    pub alignment: Ned,
    pub separation: Ned,
    pub target: Ned,
    /// How many neighbours were inside the flocking radius.
    pub neighbors_used: usize,
}

impl FlockTerms {
    pub fn sum(&self) -> Ned {
        self.cohesion + self.alignment + self.separation + self.target
    }
}

/// The flocking velocity command, m/s NED.
///
/// `own_vel` is this drone's velocity, `target` the operator-set goal as a local
/// NED offset (`None` = free flocking with no leader), and `cruise` the
/// saturation the virtual-leader term is capped at.
pub fn command(
    own_vel: Ned,
    target: Option<Ned>,
    neighbors: &[NeighborState],
    t: &FlockTuning,
    cruise: f64,
) -> Ned {
    terms(own_vel, target, neighbors, t, cruise).sum()
}

/// [`command`] with the four terms broken out.
pub fn terms(
    own_vel: Ned,
    target: Option<Ned>,
    neighbors: &[NeighborState],
    t: &FlockTuning,
    cruise: f64,
) -> FlockTerms {
    // One search at the widest radius any term needs; the repulsive term filters
    // it down by range itself.
    let set = NearestSet::build(neighbors, t.radius_m, t.neighbors);
    let mut out = FlockTerms {
        neighbors_used: set.len(),
        separation: repulsion_over(neighbors, &set, &t.separation),
        ..FlockTerms::default()
    };

    let mut far_sum = Ned::ZERO;
    let mut far_count = 0usize;
    let mut vel_sum = Ned::ZERO;
    for (i, d) in set.iter() {
        vel_sum = vel_sum + neighbors[i].vel;
        if d > t.separation.radius_m {
            far_sum = far_sum + neighbors[i].pos;
            far_count += 1;
        }
    }

    if far_count > 0 {
        // `pos` is already relative to this drone, so the neighbour centroid IS
        // the error vector — no subtraction of own position needed.
        let centroid = far_sum.scale(1.0 / far_count as f64);
        out.cohesion = centroid.scale(t.cohesion);
    }
    if !set.is_empty() {
        let mean_vel = vel_sum.scale(1.0 / set.len() as f64);
        out.alignment = (mean_vel - own_vel).scale(t.alignment);
    }
    if let Some(goal) = target {
        out.target = goal.scale(t.target).clamp_norm(cruise);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::separation::SEPARATION_RADIUS_M;

    const CRUISE: f64 = 10.0;

    fn nbr(slot: u8, pos: Ned, vel: Ned) -> NeighborState {
        NeighborState::new(slot, pos, vel, 0)
    }

    #[test]
    fn cohesion_pulls_toward_far_neighbours_only() {
        let t = FlockTuning::default();
        // One neighbour 20 m north: beyond the separation radius, so cohesion acts.
        let far = terms(
            Ned::ZERO,
            None,
            &[nbr(2, Ned::new(20.0, 0.0, 0.0), Ned::ZERO)],
            &t,
            CRUISE,
        );
        assert!((far.cohesion.n - FLOCK_COHESION * 20.0).abs() < 1e-12);
        assert_eq!(far.neighbors_used, 1);

        // One neighbour 5 m north: INSIDE the separation radius, so cohesion is
        // silent and only repulsion speaks. This is the gate that puts the lattice
        // equilibrium on the separation radius instead of at 1.65 m.
        let near = terms(
            Ned::ZERO,
            None,
            &[nbr(2, Ned::new(5.0, 0.0, 0.0), Ned::ZERO)],
            &t,
            CRUISE,
        );
        assert_eq!(near.cohesion, Ned::ZERO);
        assert!(near.separation.n < 0.0);
    }

    #[test]
    fn the_lattice_equilibrium_sits_on_the_separation_radius() {
        let t = FlockTuning::default();
        // A pair: sweep the spacing and find where the net radial term changes
        // sign. It must be the separation radius, not the 1.65 m an ungated
        // cohesion term would give.
        let radial = |d: f64| {
            terms(
                Ned::ZERO,
                None,
                &[nbr(2, Ned::new(d, 0.0, 0.0), Ned::ZERO)],
                &t,
                CRUISE,
            )
            .sum()
            .n
        };
        assert!(
            radial(SEPARATION_RADIUS_M + 0.1) > 0.0,
            "beyond: pulls together"
        );
        assert!(
            radial(SEPARATION_RADIUS_M - 0.1) < 0.0,
            "inside: pushes apart"
        );
        // And the 1.65 m collapse point of the ungated formulation is firmly
        // repulsive here.
        assert!(radial(1.65) < 0.0);
    }

    #[test]
    fn alignment_matches_the_mean_neighbour_velocity() {
        let t = FlockTuning::default();
        let ns = [
            nbr(2, Ned::new(20.0, 0.0, 0.0), Ned::new(6.0, 0.0, 0.0)),
            nbr(3, Ned::new(0.0, 20.0, 0.0), Ned::new(4.0, 0.0, 0.0)),
        ];
        // Mean neighbour velocity 5 m/s north; this drone is stationary.
        let out = terms(Ned::ZERO, None, &ns, &t, CRUISE);
        assert!((out.alignment.n - FLOCK_ALIGNMENT * 5.0).abs() < 1e-12);
        // Already matched: no alignment demand at all.
        let matched = terms(Ned::new(5.0, 0.0, 0.0), None, &ns, &t, CRUISE);
        assert!(matched.alignment.norm() < 1e-12, "{:?}", matched.alignment);
    }

    #[test]
    fn the_virtual_leader_is_a_uniform_field_beyond_the_saturation_radius() {
        let t = FlockTuning::default();
        // Two drones 8 m apart on the line to a target 500 m away get target
        // terms that differ by essentially nothing — that is what stops the
        // lattice compressing along-track.
        let lead = terms(Ned::ZERO, Some(Ned::new(500.0, 0.0, 0.0)), &[], &t, CRUISE).target;
        let trail = terms(Ned::ZERO, Some(Ned::new(508.0, 0.0, 0.0)), &[], &t, CRUISE).target;
        assert!((lead.norm() - CRUISE).abs() < 1e-12, "saturated at cruise");
        assert!((trail.norm() - CRUISE).abs() < 1e-12);
        assert!(
            (lead - trail).norm() < 1e-9,
            "differential compression {:?}",
            lead - trail
        );
        // An UNSATURATED proportional pull would differ by 0.8 * 8 = 6.4 m/s —
        // the failure mode this test exists to pin.
        let raw_lead = Ned::new(500.0, 0.0, 0.0).scale(FLOCK_TARGET);
        let raw_trail = Ned::new(508.0, 0.0, 0.0).scale(FLOCK_TARGET);
        assert!(((raw_trail - raw_lead).norm() - 6.4).abs() < 1e-9);
    }

    #[test]
    fn the_virtual_leader_decays_to_zero_at_the_target() {
        let t = FlockTuning::default();
        let mut last = f64::INFINITY;
        for r in [12.5, 8.0, 4.0, 1.0, 0.1] {
            let m = terms(Ned::ZERO, Some(Ned::new(r, 0.0, 0.0)), &[], &t, CRUISE)
                .target
                .norm();
            assert!(
                m < last,
                "pull must shrink as the target is reached: {m} >= {last}"
            );
            assert!(
                (m - FLOCK_TARGET * r).abs() < 1e-12,
                "proportional inside saturation"
            );
            last = m;
        }
        assert_eq!(
            terms(Ned::ZERO, Some(Ned::ZERO), &[], &t, CRUISE).target,
            Ned::ZERO
        );
        assert_eq!(terms(Ned::ZERO, None, &[], &t, CRUISE).target, Ned::ZERO);
    }

    #[test]
    fn an_empty_neighbourhood_leaves_only_the_leader_term() {
        let t = FlockTuning::default();
        let out = terms(
            Ned::new(1.0, 2.0, 3.0),
            Some(Ned::new(0.0, 100.0, 0.0)),
            &[],
            &t,
            CRUISE,
        );
        assert_eq!(out.cohesion, Ned::ZERO);
        assert_eq!(out.alignment, Ned::ZERO);
        assert_eq!(out.separation, Ned::ZERO);
        assert_eq!(out.neighbors_used, 0);
        assert!((out.target.e - CRUISE).abs() < 1e-12);
    }

    #[test]
    fn neighbours_beyond_the_flock_radius_are_invisible() {
        let t = FlockTuning::default();
        let out = terms(
            Ned::ZERO,
            None,
            &[nbr(
                2,
                Ned::new(FLOCK_RADIUS_M + 1.0, 0.0, 0.0),
                Ned::new(9.0, 0.0, 0.0),
            )],
            &t,
            CRUISE,
        );
        assert_eq!(out.neighbors_used, 0);
        assert_eq!(out.sum(), Ned::ZERO);
    }

    #[test]
    fn command_is_the_sum_of_the_terms() {
        let t = FlockTuning::default();
        let ns = [
            nbr(2, Ned::new(20.0, 0.0, 0.0), Ned::new(1.0, 0.0, 0.0)),
            nbr(3, Ned::new(-5.0, 0.0, 0.0), Ned::new(2.0, 0.0, 0.0)),
        ];
        let target = Some(Ned::new(0.0, 40.0, -5.0));
        let terms = terms(Ned::new(0.5, 0.0, 0.0), target, &ns, &t, CRUISE);
        let cmd = command(Ned::new(0.5, 0.0, 0.0), target, &ns, &t, CRUISE);
        assert_eq!(cmd, terms.sum());
        assert!(cmd.is_finite());
    }

    #[test]
    fn tuning_sanitises_absurd_gains_without_disabling_the_layer() {
        let t = FlockTuning {
            radius_m: 2.0, // narrower than the separation radius
            neighbors: 0,
            cohesion: f64::NAN,
            alignment: -1.0,
            target: f64::INFINITY,
            separation: SeparationTuning::default(),
        }
        .sanitised();
        assert!(t.radius_m > t.separation.radius_m);
        assert_eq!(t.neighbors, FLOCK_NEIGHBORS);
        assert_eq!(t.cohesion, 0.0);
        assert_eq!(t.alignment, 0.0);
        assert_eq!(t.target, 0.0);
        // A zeroed gain is inert, never NaN-propagating.
        assert!(command(Ned::ZERO, Some(Ned::new(100.0, 0.0, 0.0)), &[], &t, CRUISE).is_finite());
    }
}
