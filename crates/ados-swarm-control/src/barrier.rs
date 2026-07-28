//! The separation layer's override half: a braking-aware closing-rate barrier.
//!
//! # Why a barrier and not just a stronger repulsive gain
//!
//! The plan fixes the repulsive force at `1.5·(1/d − 1/8)` m/s, which peaks at
//! 0.19 m/s as a neighbour reaches the 4 m floor. Any goal-seeking term — an
//! operator command, a virtual leader, a formation error — is metres per second
//! of intent, so a summed potential field CANNOT hold a 4 m floor: it is
//! outvoted by two orders of magnitude, and the vehicle sails through the floor
//! before the hard override even notices.
//!
//! The plan's own answer is that the separation layer is "applied LAST" and
//! "OVERRIDES flocking and formation". An override is not a summand. This module
//! implements it as a constraint: after every behaviour layer has had its say,
//! the component of the commanded velocity that closes on a neighbour is capped
//! at what the geometry can afford. Everything perpendicular to that neighbour
//! passes through untouched, so the vehicle still manoeuvres — it just cannot
//! manoeuvre INTO anything.
//!
//! # The guarantee, and why braking distance is in it
//!
//! A naive cap of `k·(d − hard)` on the COMMANDED rate is a discrete control
//! barrier function only if the plant reaches the commanded velocity within one
//! control period. A real multirotor does not: its velocity loop has a time
//! constant of a few hundred milliseconds, so a vehicle told to stop keeps
//! closing for roughly `v·τ` more metres. Cap the command alone and a drone
//! arriving at the speed ceiling coasts several metres past the floor — which is
//! exactly what it does, measured, before this term is added.
//!
//! So the allowance is computed on the BRAKING-ADJUSTED margin
//! `d − hard − v_close·`[`BARRIER_LAG_S`], where `v_close` is the ACTUAL relative
//! closing speed, not the commanded one. A vehicle whose stopping distance
//! already consumes the margin gets an allowance of zero, i.e. maximum braking,
//! while it is still metres clear. [`scan_radius`] widens the search far enough
//! that this happens before the floor rather than at it.
//!
//! The cap is reciprocal without any negotiation: both drones in a converging
//! pair see the same `d` and the same relative velocity, and each limits its own
//! half of the closure.
//!
//! # Trimming is not an emergency
//!
//! [`BarrierOutcome`] separates two things the arbitration must not conflate. A
//! command being TRIMMED is ordinary: it happens constantly in a dense lattice
//! and the behaviour layer stays in charge. A vehicle being PINNED — allowance at
//! zero while it still wants to close — is the deadlock that only the hard
//! override's deterministic vertical rule can break. Escalating on every trim
//! freezes a whole flock; escalating on none of them lets a pair sit nose to nose
//! forever.

use crate::geo::Ned;
use crate::neighbor::{NearestSet, NeighborState, MAX_NEAREST};
use crate::separation::SeparationTuning;
use crate::setpoint::MAX_COMMAND_SPEED_MPS;

/// Barrier gain, 1/s: allowed closing rate per metre of braking-adjusted margin.
pub const BARRIER_GAIN_HZ: f64 = 1.0;

/// Plant lag budget, seconds — how long a vehicle keeps its present velocity
/// after the command changes. One control period of dead time plus a
/// conservative small-multirotor velocity-loop time constant. Deliberately
/// generous: over-estimating the lag costs a little conservatism, under-estimating
/// it costs the floor.
pub const BARRIER_LAG_S: f64 = 0.5;

/// Allowance at or below which a vehicle counts as PINNED rather than merely
/// trimmed, m/s. Small enough that ordinary lattice traffic never trips it, large
/// enough that a genuine deadlock is recognised before the geometry gets worse.
pub const HARD_PIN_MPS: f64 = 0.25;

/// Projection passes per call.
///
/// One pass per neighbour is not a solution when several bind at once: enforcing
/// the nearest neighbour's cap can reintroduce closure toward another, which is
/// precisely what happens in a packed flock and is measurably what breaches the
/// floor there. Sweeping the constraint set a few times is Gauss-Seidel on the
/// feasible set — it converges quickly for a satisfiable geometry and the loop
/// exits early when a pass changes nothing.
pub const BARRIER_PASSES: usize = 4;

/// How far out the barrier looks, metres.
///
/// The soft separation radius, or the hard floor plus twice the braking distance
/// at the speed ceiling, whichever is larger. The braking term is what matters: a
/// vehicle arriving at 10 m/s needs to be decelerating well before the soft
/// radius, and a barrier that only looks 8 m ahead cannot ask for that.
pub fn scan_radius(t: &SeparationTuning) -> f64 {
    t.radius_m
        .max(t.hard_m + 2.0 * MAX_COMMAND_SPEED_MPS * BARRIER_LAG_S)
}

/// What the barrier did to a command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarrierOutcome {
    /// The command after the closing components were capped.
    pub command: Ned,
    /// The nearest neighbour whose cap TRIMMED the command. Ordinary; not an
    /// escalation.
    pub blocked_by: Option<u8>,
    /// Range to the trimming neighbour, metres.
    pub blocked_at_m: f64,
    /// The nearest neighbour that left this vehicle with no usable closing
    /// allowance at all while it still wanted to close. This is the deadlock
    /// signal the hard override escalates on.
    pub pinned_by: Option<u8>,
}

/// Maximum closing rate allowed toward a neighbour, m/s.
///
/// `d` is the current range, `closing_speed` the ACTUAL relative closing speed
/// (negative when opening). Zero once the braking distance has eaten the margin.
///
/// Deliberately never negative. The barrier is a CAP, and keeping it one is what
/// makes its two invariants hold: opening motion and motion perpendicular to a
/// neighbour pass through untouched, and a command is only ever trimmed, never
/// reversed. Pushing a vehicle backwards is the repulsive term's job
/// ([`crate::separation::repulsion`]) — folding it in here would give the same
/// physical effect two owners and two gains.
pub fn allowed_closing_rate(d: f64, closing_speed: f64, hard_m: f64, gain: f64) -> f64 {
    if !d.is_finite() || !closing_speed.is_finite() {
        return 0.0;
    }
    let margin = d - hard_m - closing_speed.max(0.0) * BARRIER_LAG_S;
    (gain * margin).max(0.0)
}

/// Cap the closing component of `command` toward every neighbour the barrier can
/// see.
///
/// Applied to the SUM of every behaviour layer plus the repulsive term, once, as
/// the last step before a setpoint is emitted. Constraints are swept
/// farthest-neighbour first so the tightest lands last, and the whole sweep is
/// repeated up to [`BARRIER_PASSES`] times so a geometry with several binding
/// neighbours ends up satisfying all of them rather than only the last one
/// applied.
pub fn constrain(
    command: Ned,
    own_vel: Ned,
    neighbors: &[NeighborState],
    t: &SeparationTuning,
    gain: f64,
) -> BarrierOutcome {
    let mut out = BarrierOutcome {
        command,
        blocked_by: None,
        blocked_at_m: f64::INFINITY,
        pinned_by: None,
    };
    if !command.is_finite() {
        out.command = Ned::ZERO;
        return out;
    }
    let set = NearestSet::build(neighbors, scan_radius(t), MAX_NEAREST);
    let mut ordered = [(0usize, 0.0f64, 0.0f64); MAX_NEAREST];
    let mut len = 0usize;
    for (i, d) in set.iter() {
        if d <= f64::EPSILON {
            // Coincident: no closing direction is defined. The geometric breach
            // test in the separation layer owns this case.
            continue;
        }
        let toward = neighbors[i].pos.scale(1.0 / d);
        let relative = own_vel - neighbors[i].vel;
        let closing_now = toward.n * relative.n + toward.e * relative.e + toward.d * relative.d;
        ordered[len] = (i, d, allowed_closing_rate(d, closing_now, t.hard_m, gain));
        len += 1;
    }
    let mut pinned_at = f64::INFINITY;
    for _ in 0..BARRIER_PASSES {
        let mut bound_any = false;
        for &(i, d, allowed) in ordered[..len].iter().rev() {
            let toward = neighbors[i].pos.scale(1.0 / d);
            let closing_cmd =
                out.command.n * toward.n + out.command.e * toward.e + out.command.d * toward.d;
            if closing_cmd <= allowed {
                continue;
            }
            bound_any = true;
            out.command = out.command - toward.scale(closing_cmd - allowed);
            if d < out.blocked_at_m {
                out.blocked_by = Some(neighbors[i].slot);
                out.blocked_at_m = d;
            }
            if allowed <= HARD_PIN_MPS && d < pinned_at {
                out.pinned_by = Some(neighbors[i].slot);
                pinned_at = d;
            }
        }
        if !bound_any {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::separation::{SEPARATION_HARD_M, SEPARATION_RADIUS_M};

    fn nbr(slot: u8, n: f64, e: f64, d: f64) -> NeighborState {
        NeighborState::new(slot, Ned::new(n, e, d), Ned::ZERO, 0)
    }

    fn moving(slot: u8, pos: Ned, vel: Ned) -> NeighborState {
        NeighborState::new(slot, pos, vel, 0)
    }

    fn t() -> SeparationTuning {
        SeparationTuning::default()
    }

    fn still(command: Ned, ns: &[NeighborState]) -> BarrierOutcome {
        constrain(command, Ned::ZERO, ns, &t(), BARRIER_GAIN_HZ)
    }

    #[test]
    fn the_allowance_is_a_cap_that_bottoms_out_at_zero() {
        let floor = SEPARATION_HARD_M;
        assert_eq!(
            allowed_closing_rate(floor, 0.0, floor, BARRIER_GAIN_HZ),
            0.0
        );
        // Inside the floor it clamps at zero rather than commanding a retreat:
        // reversing a command is the repulsive term's job, not the cap's.
        assert_eq!(allowed_closing_rate(1.0, 0.0, floor, BARRIER_GAIN_HZ), 0.0);
        assert_eq!(allowed_closing_rate(0.0, 5.0, floor, BARRIER_GAIN_HZ), 0.0);
        // Standing still 8 m out, the margin is the full 4 m.
        assert!(
            (allowed_closing_rate(SEPARATION_RADIUS_M, 0.0, floor, BARRIER_GAIN_HZ) - 4.0).abs()
                < 1e-12
        );
        assert_eq!(allowed_closing_rate(f64::NAN, 0.0, floor, 1.0), 0.0);
        assert_eq!(allowed_closing_rate(8.0, f64::NAN, floor, 1.0), 0.0);
    }

    #[test]
    fn braking_distance_eats_the_margin_so_a_fast_approach_is_stopped_early() {
        let floor = SEPARATION_HARD_M;
        // Standing still at 10 m: 6 m of margin.
        let idle = allowed_closing_rate(10.0, 0.0, floor, BARRIER_GAIN_HZ);
        assert!((idle - 6.0).abs() < 1e-12, "{idle}");
        // Arriving at the speed ceiling, 5 m of that margin is stopping distance.
        let fast = allowed_closing_rate(10.0, MAX_COMMAND_SPEED_MPS, floor, BARRIER_GAIN_HZ);
        assert!((fast - 1.0).abs() < 1e-12, "{fast}");
        assert_eq!(
            allowed_closing_rate(8.0, MAX_COMMAND_SPEED_MPS, floor, BARRIER_GAIN_HZ),
            0.0,
            "at 8 m and 10 m/s the vehicle cannot afford any closure at all"
        );
        // Opening is never charged a braking distance.
        assert!((allowed_closing_rate(8.0, -10.0, floor, BARRIER_GAIN_HZ) - 4.0).abs() < 1e-12);
    }

    #[test]
    fn the_scan_radius_reaches_past_the_soft_radius_by_the_braking_distance() {
        let r = scan_radius(&t());
        assert!(r > SEPARATION_RADIUS_M, "{r}");
        assert!(
            r >= SEPARATION_HARD_M + MAX_COMMAND_SPEED_MPS * BARRIER_LAG_S,
            "a vehicle at cruise must be inside the barrier before it can reach the floor: {r}"
        );
        // A hand-widened soft radius still governs when it is the larger.
        let wide = SeparationTuning {
            radius_m: 40.0,
            ..Default::default()
        };
        assert_eq!(scan_radius(&wide), 40.0);
    }

    #[test]
    fn a_command_that_closes_too_fast_is_capped_to_the_allowance() {
        // Neighbour 5 m north, both stationary: allowance 1 m/s. A 6 m/s northward
        // command is cut to exactly 1 m/s.
        let out = still(Ned::new(6.0, 0.0, 0.0), &[nbr(2, 5.0, 0.0, 0.0)]);
        assert!((out.command.n - 1.0).abs() < 1e-12, "{:?}", out.command);
        assert_eq!(out.blocked_by, Some(2));
        assert!((out.blocked_at_m - 5.0).abs() < 1e-12);
        assert_eq!(out.pinned_by, None, "1 m/s of headroom is not a deadlock");
    }

    #[test]
    fn a_command_inside_the_allowance_passes_through_untouched() {
        let cmd = Ned::new(0.5, 0.0, 0.0);
        let out = still(cmd, &[nbr(2, 5.0, 0.0, 0.0)]);
        assert_eq!(out.command, cmd);
        assert_eq!(out.blocked_by, None);
        assert_eq!(out.pinned_by, None);
    }

    #[test]
    fn opening_and_perpendicular_motion_is_never_restricted() {
        let ns = [nbr(2, 4.5, 0.0, 0.0)];
        // Straight away from the neighbour at full speed: untouched.
        let away = still(Ned::new(-10.0, 0.0, 0.0), &ns);
        assert_eq!(away.command, Ned::new(-10.0, 0.0, 0.0));
        assert_eq!(away.blocked_by, None);
        // Sideways at full speed: untouched. The drone must still manoeuvre.
        let across = still(Ned::new(0.0, 10.0, 0.0), &ns);
        assert!((across.command.e - 10.0).abs() < 1e-12);
        assert!(across.command.n.abs() < 1e-12);
        assert_eq!(across.blocked_by, None);
    }

    #[test]
    fn only_the_closing_component_is_removed() {
        // 45 degrees: half the command closes, half is across.
        let out = still(Ned::new(6.0, 6.0, 0.0), &[nbr(2, 5.0, 0.0, 0.0)]);
        assert!(
            (out.command.n - 1.0).abs() < 1e-12,
            "closing capped to 1 m/s"
        );
        assert!((out.command.e - 6.0).abs() < 1e-12, "across untouched");
    }

    #[test]
    fn at_the_floor_the_vehicle_is_pinned_not_merely_trimmed() {
        let out = still(
            Ned::new(9.0, 0.0, 0.0),
            &[nbr(2, SEPARATION_HARD_M - 0.5, 0.0, 0.0)],
        );
        assert!(out.command.n.abs() < 1e-12, "{:?}", out.command);
        assert_eq!(out.blocked_by, Some(2));
        assert_eq!(
            out.pinned_by,
            Some(2),
            "no closing allowance at all is the deadlock the vertical rule breaks"
        );
    }

    #[test]
    fn a_fast_approach_is_trimmed_hard_without_being_called_a_deadlock() {
        // 12 m out, closing at cruise: the allowance is 12 - 4 - 5 = 3 m/s, so the
        // command is cut but the vehicle is plainly not deadlocked.
        let ns = [moving(2, Ned::new(12.0, 0.0, 0.0), Ned::ZERO)];
        let out = constrain(
            Ned::new(10.0, 0.0, 0.0),
            Ned::new(10.0, 0.0, 0.0),
            &ns,
            &t(),
            BARRIER_GAIN_HZ,
        );
        assert!((out.command.n - 3.0).abs() < 1e-9, "{:?}", out.command);
        assert_eq!(out.blocked_by, Some(2));
        assert_eq!(
            out.pinned_by, None,
            "an ordinary trim must not freeze a flock"
        );
    }

    #[test]
    fn a_neighbour_fleeing_ahead_of_us_is_not_a_closure() {
        // 6 m ahead and running away at the same speed: relative closing is zero,
        // so the allowance is the full standing margin and a chase is allowed.
        let ns = [moving(2, Ned::new(6.0, 0.0, 0.0), Ned::new(8.0, 0.0, 0.0))];
        let out = constrain(
            Ned::new(8.0, 0.0, 0.0),
            Ned::new(8.0, 0.0, 0.0),
            &ns,
            &t(),
            BARRIER_GAIN_HZ,
        );
        assert!((out.command.n - 2.0).abs() < 1e-9, "{:?}", out.command);
        assert_eq!(out.pinned_by, None);
    }

    #[test]
    fn the_nearest_neighbour_wins_when_several_bind() {
        let ns = [nbr(2, 7.0, 0.0, 0.0), nbr(9, 4.2, 0.0, 0.0)];
        let out = still(Ned::new(8.0, 0.0, 0.0), &ns);
        assert!(
            (out.command.n - 0.2).abs() < 1e-9,
            "the 4.2 m cap must hold, got {:?}",
            out.command
        );
        assert_eq!(out.blocked_by, Some(9), "reported offender is the nearest");
        assert_eq!(out.pinned_by, Some(9), "0.2 m/s is under the pin threshold");
    }

    #[test]
    fn opposing_neighbours_leave_a_command_that_escapes_both() {
        let ns = [nbr(2, 4.5, 0.0, 0.0), nbr(3, -4.5, 0.0, 0.0)];
        let out = still(Ned::new(5.0, 2.0, 0.0), &ns);
        assert!(
            out.command.n.abs() < 0.51,
            "north closure capped: {:?}",
            out.command
        );
        assert!(
            out.command.n > -0.51,
            "not shoved into the southern neighbour"
        );
        assert!(
            (out.command.e - 2.0).abs() < 1e-12,
            "the escape lane stays open"
        );
    }

    #[test]
    fn neighbours_beyond_the_scan_radius_do_not_constrain() {
        let r = scan_radius(&t());
        let ns = [nbr(2, r + 0.1, 0.0, 0.0)];
        let out = still(Ned::new(10.0, 0.0, 0.0), &ns);
        assert_eq!(out.command, Ned::new(10.0, 0.0, 0.0));
        assert_eq!(out.blocked_by, None);
    }

    #[test]
    fn a_non_finite_command_is_zeroed_not_radiated() {
        let out = still(Ned::new(f64::NAN, 0.0, 0.0), &[nbr(2, 5.0, 0.0, 0.0)]);
        assert_eq!(out.command, Ned::ZERO);
    }

    #[test]
    fn a_coincident_neighbour_is_skipped_without_poisoning_the_command() {
        let out = still(Ned::new(3.0, 0.0, 0.0), &[nbr(2, 0.0, 0.0, 0.0)]);
        assert!(out.command.is_finite());
        assert_eq!(out.command, Ned::new(3.0, 0.0, 0.0));
    }

    #[test]
    fn the_barrier_holds_the_floor_against_a_lagging_plant() {
        // The guarantee, integrated, WITH the plant lag that breaks a naive cap:
        // two drones commanded at each other from 40 m at the speed ceiling, each
        // applying its own half of the barrier at 10 Hz, velocity tracking the
        // command with a 0.3 s time constant.
        let dt = 0.1;
        let tau = 0.3;
        let (mut a, mut b) = (0.0f64, 40.0f64);
        let (mut va, mut vb) = (0.0f64, 0.0f64);
        let mut min_gap = f64::INFINITY;
        for _ in 0..3000 {
            let gap = b - a;
            min_gap = min_gap.min(gap);
            let ca = constrain(
                Ned::new(MAX_COMMAND_SPEED_MPS, 0.0, 0.0),
                Ned::new(va, 0.0, 0.0),
                &[moving(2, Ned::new(gap, 0.0, 0.0), Ned::new(vb, 0.0, 0.0))],
                &t(),
                BARRIER_GAIN_HZ,
            )
            .command
            .n;
            let cb = constrain(
                Ned::new(-MAX_COMMAND_SPEED_MPS, 0.0, 0.0),
                Ned::new(vb, 0.0, 0.0),
                &[moving(1, Ned::new(-gap, 0.0, 0.0), Ned::new(va, 0.0, 0.0))],
                &t(),
                BARRIER_GAIN_HZ,
            )
            .command
            .n;
            let alpha = dt / tau;
            va += (ca - va) * alpha;
            vb += (cb - vb) * alpha;
            a += va * dt;
            b += vb * dt;
        }
        assert!(
            min_gap > SEPARATION_HARD_M,
            "barrier let a lagging plant through the floor: min gap {min_gap}"
        );
        // And it really converged onto the floor rather than stopping far out.
        assert!(min_gap < SEPARATION_HARD_M + 1.5, "min gap {min_gap}");
    }
}
