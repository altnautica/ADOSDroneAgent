//! What the pilot commits to of a trajectory plan: its sure part (`exec` to
//! the commit time, then the backup it stops on) must lie in seen-free
//! space, keep the obstacle margin, stay inside a forward view, and keep
//! clear of every mover where the mover is predicted at the plan's own time
//! (a time-indexed obstacle, [`crate::obstacles::Mover::plan_keep_m`]).

use ados_planning::{Cell, Limits, PlanarQuery};

use super::map2d::{PlanMap, OWN_M};
use crate::geo::{self, V2};
use crate::obstacles::CELL_M;

/// What a plan flies for sure (`exec` to its commit time, and the backup)
/// is sampled this often, s.
const KEEP_DT_S: f32 = 0.05;
/// One step of the grid's clearance field (a chamfer unit), m: a sample may
/// read this much under the start without being nearer.
const CLEAR_STEP_M: f32 = CELL_M / 3.0 + 1e-3;

/// How far off the heading the forward view lets a plan's sure part go,
/// rad: a forward depth camera's half view (40-45°) and the 45° a grid
/// path's first step may take; a step sideways of that, into space seen
/// from elsewhere, brushes what the camera never saw.
pub(super) const FORWARD_VIEW_HALF_RAD: f32 = 50.0 * std::f32::consts::PI / 180.0;
/// Within this of a plan's start a sample has not moved anywhere yet, m:
/// the rounding allowance the map itself gives the start's own point
/// ([`OWN_M`]). A plan from rest takes its first centimetres along its
/// smoothest start, which may point anywhere (seen 1-2 cm at 65-77° off
/// the way it then goes); judged by bearing, every plan from a hover was
/// turned down. The allowance stays a fraction of a voxel: a plan whose
/// sure part keeps inside it stops inside it, so plan after plan may step
/// the vehicle sideways by no more than that each.
const VIEW_SLACK_M: f32 = OWN_M;
/// Out of the forward view, a plan's sure part may also stray this far from
/// its start on the way, m, so long as it comes to rest in view (or within
/// [`carried_m`], or within this when the plan's way leads off in view):
/// the planner's path runs from the start cell's centre, up to half a cell
/// off the start, and a smooth trajectory out of a hover sets off toward
/// it. Judged at [`VIEW_SLACK_M`], 4-7 cm of that turned plan after plan
/// down while a vehicle hovered in a street walkers crossed, or beside a
/// trunk on the margin's edge. A plan that only steps sideways is still
/// refused, so no plan is made to creep the vehicle out of view.
const WIGGLE_M: f32 = CELL_M / 2.0;

/// How far from a plan's start its samples may lie in any direction before
/// the forward view judges them, m: the way the start's own motion carries
/// the vehicle braking as hard as the limits let it (any acceleration
/// undone at the jerk limit, the braking ramped in at it, then held at the
/// acceleration limit), plus [`VIEW_SLACK_M`]. That much is no plan's
/// choice: a hovering vehicle's drift of a few cm/s goes every way, and
/// judging its first centimetres by bearing turns every plan down. Any more
/// is: a plan that steers sideways leaves it, and one that brakes a
/// sideways drift at the limits stays inside it, so no plan adds sideways
/// speed for the next to be allowed more.
fn carried_m(v0: f32, a0: f32, lim: &Limits) -> f32 {
    let a = lim.a_max.max(0.1);
    let j = lim.j_max.max(0.1);
    let v = v0 + a0 * a0 / (2.0 * j);
    let stop = if v <= a * a / (2.0 * j) {
        // Stopped while the braking still ramps in.
        2.0 / 3.0 * v * (2.0 * v / j).sqrt()
    } else {
        let t1 = a / j;
        let v1 = v - a * t1 / 2.0;
        v * t1 - j * t1 * t1 * t1 / 6.0 + v1 * v1 / (2.0 * a)
    };
    VIEW_SLACK_M + stop
}

/// The pilot commits only to a plan whose sure part (`exec` to the commit
/// time, and the backup it stops on) lies in seen-free space, keeps the
/// margin, or, from a start already inside the margin, comes no nearer
/// than the start is (an escape, never deeper in: the planner lets a start
/// inside the margin end a little nearer still, which replan after replan
/// walks the vehicle onto what it is inside the margin of), and, with a
/// forward view (`heading`), stays inside it: a camera never sees beside
/// the vehicle, and a sideways step into space it saw from elsewhere
/// brushes what it never saw. Every sample also keeps
/// [`crate::obstacles::Mover::plan_keep_m`] from each of the map's movers
/// where it is predicted at that sample's time, or, from a start already
/// nearer, comes no nearer to it than the start is. `from_s` into the plan
/// is where the check starts: 0 for a new plan, now for one kept on.
pub(super) fn keeps_out(
    plan: &ados_planning::Plan,
    map: &PlanMap<'_>,
    margin: f32,
    heading: Option<f32>,
    lim: &Limits,
    from_s: f32,
) -> bool {
    let movers = map.movers;
    let at = |p: [f32; 3]| [p[0], p[1]];
    let (p0, v0, a0) = plan.sample(from_s, true);
    let start = at(p0);
    let c0 = map.clearance(start);
    let need = if c0 >= margin {
        margin
    } else {
        c0 - CLEAR_STEP_M
    };
    let carried = carried_m(geo::norm(at(v0)), geo::norm(at(a0)), lim);
    let in_view = |p: V2| within(start, heading, carried.max(WIGGLE_M), p);
    // The plan's own clock: seconds into it as ADOS time.
    let ms = |t: f32| plan.made_ms + (t.max(0.0) * 1000.0) as u64;
    let d0: Vec<f32> = movers
        .iter()
        .map(|m| geo::dist(start, m.at(ms(from_s))))
        .collect();
    let clear_of_movers = |p: V2, t: f32| {
        movers.iter().zip(&d0).all(|(m, &d0)| {
            let when = ms(t);
            let d = geo::dist(p, m.at(when));
            d >= m.plan_keep_m(when) || d >= d0 - CLEAR_STEP_M
        })
    };
    let ok = |p: [f32; 3], t: f32| {
        let p = at(p);
        map.state(p) == Cell::Free
            && map.clearance(p) >= need
            && in_view(p)
            && clear_of_movers(p, t)
    };
    // `tr` sampled from `from` to `end` of its own time, starting `off`
    // seconds into the plan.
    let sure = |tr: &ados_planning::Trajectory, from: f32, end: f32, off: f32| {
        let n = (((end - from) / KEEP_DT_S).ceil() as usize).max(1);
        (0..=n).all(|i| {
            let t = from + (end - from) * i as f32 / n as f32;
            ok(tr.sample(t).0, off + t)
        })
    };
    let t_c = plan.t_commit.min(plan.exec.duration());
    let dur = plan.backup.duration();
    let rest = rests_in_view(
        start,
        heading,
        carried,
        at(plan.backup.sample(dur).0),
        at(plan.exec.sample(plan.exec.duration()).0),
    );
    rest && if from_s <= t_c {
        sure(&plan.exec, from_s, t_c, 0.0) && sure(&plan.backup, 0.0, dur, t_c)
    } else {
        sure(&plan.backup, (from_s - t_c).min(dur), dur, t_c)
    }
}

/// `p` lies within `slack` of `start`, or inside the forward view along
/// `heading` from it (anywhere, without a forward view).
fn within(start: V2, heading: Option<f32>, slack: f32, p: V2) -> bool {
    let rel = geo::sub(p, start);
    heading.is_none_or(|h| {
        geo::norm(rel) <= slack
            || geo::wrap_pi(geo::bearing_of(rel) - h).abs() <= FORWARD_VIEW_HALF_RAD
    })
}

/// Where a plan's sure part comes to rest (`rest`) is in view, or no
/// further off `start` than its own motion carries it (`carried`); or, for
/// a plan whose way leads off in view (its exec ending at `exec_end` past
/// [`WIGGLE_M`]), within the wiggle (a backup branching early stops on the
/// way's first centimetres). A plan that only steps sideways is refused.
fn rests_in_view(start: V2, heading: Option<f32>, carried: f32, rest: V2, exec_end: V2) -> bool {
    let leads_off =
        geo::dist(exec_end, start) > WIGGLE_M && within(start, heading, carried, exec_end);
    within(start, heading, carried, rest) || (leads_off && geo::dist(rest, start) <= WIGGLE_M)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_rests_in_view_or_near_its_start_only_when_its_way_leads_off_in_view() {
        let (o, h, carried) = ([0.0, 0.0], Some(0.0), 0.05);
        let back = [-0.1, 0.0];
        let ahead = [5.0, 0.0];
        let beside = [0.0, 5.0];
        // Ahead: in view.
        assert!(rests_in_view(o, h, carried, ahead, ahead));
        // Stopping 10 cm behind on the way's first centimetres, the way
        // leading off ahead: allowed.
        assert!(rests_in_view(o, h, carried, back, ahead));
        // The same stop with a way that only steps sideways: refused.
        assert!(!rests_in_view(o, h, carried, back, beside));
        assert!(!rests_in_view(o, h, carried, back, back));
        // Past the wiggle behind: refused whatever the way.
        assert!(!rests_in_view(o, h, carried, [-0.2, 0.0], ahead));
        // A sideways step within what the start carries: allowed.
        assert!(rests_in_view(o, h, carried, [0.0, 0.04], [0.0, 0.04]));
        // Without a forward view nothing is judged.
        assert!(rests_in_view(o, None, carried, beside, beside));
        assert!(within(o, h, WIGGLE_M, [-0.1, 0.0]));
        assert!(!within(o, h, carried, [-0.1, 0.0]));
    }
}
