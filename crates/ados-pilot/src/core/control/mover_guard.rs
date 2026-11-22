//! The separation guard: every person the pilot tracks (and every mover of
//! unknown kind, which may be one, [`crate::obstacles::Movers`]) is kept at
//! least [`PERSON_SEP_M`] off horizontally, its uncertainty and
//! [`GUARD_ROOM_M`] on top, unless the vehicle flies [`OVER_HEAD_M`] over
//! their head. It is the subject brake ([`super::subject_guard`]) applied to
//! each of them: a soft tier on the goal profile before the shaper, a hard
//! tier on the stick acceleration after it, each holding the speed toward
//! the person to what still stops outside the kept distance given their own
//! motion, and backing away inside it. It needs no planner: whatever the
//! goal or the plan asks, the guard cuts it.

use ados_protocol::pilot::{Owner, SensingTier};

use super::subject_guard::Subject;
use crate::core::Pilot;
use crate::geo::{self, V2};
use crate::obstacles::{Mover, MoverFlight, OVER_HEAD_M, PERSON_H_M, PERSON_SEP_M};

/// A forward-only sensor turns to a person once they are this near the
/// distance the guard keeps from them, m.
pub const LOOK_ZONE_M: f32 = 2.0;

/// Room kept beyond the separation, m: the tracker's lag and the FC's
/// tracking error.
pub const GUARD_ROOM_M: f32 = 0.3;
/// The kept distance grows by this many of the mover's one-sigma ...
const GUARD_SIGMAS: f32 = 2.0;
/// ... by at most this much, m.
const GUARD_SIGMA_MAX_M: f32 = 1.0;

/// The horizontal distance the guard keeps from `m` at `now_ms`, m.
pub fn guard_keep_m(m: &Mover, now_ms: u64) -> f32 {
    PERSON_SEP_M + GUARD_ROOM_M + (GUARD_SIGMAS * m.sigma_at(now_ms)).min(GUARD_SIGMA_MAX_M)
}

/// The people the guard keeps clear of this tick
/// ([`Pilot::mover_guards()`]), held in one buffer the pilot reuses.
#[derive(Debug, Default)]
pub(crate) struct MoverGuards(Vec<Subject>);

impl MoverGuards {
    /// The people kept clear of this tick.
    pub(super) fn people(&self) -> &[Subject] {
        &self.0
    }
}

impl Pilot {
    /// Before each grid rebuild: the margin the planners keep and whether
    /// the vehicle flies over a person's head (its height above ground less
    /// a person's [`PERSON_H_M`] at least [`OVER_HEAD_M`]; unknown, it does
    /// not).
    pub(super) fn mover_flight(&mut self, now_ms: u64) {
        let over_head = self
            .agl(now_ms)
            .is_some_and(|a| a - PERSON_H_M >= OVER_HEAD_M);
        // The skill's own subject is kept by the subject brake: its camera
        // track when the goal names it, else where the goal puts it.
        let goal = (self.owner() == Owner::Skill)
            .then(|| self.auth.lease()?.goal.as_ref())
            .flatten();
        let subject = goal.and_then(|g| g.subject_ne);
        let subject_track = goal.and_then(|g| g.subject_ref);
        self.obstacles.movers.flight = MoverFlight {
            margin: self.margin(),
            over_head,
            subject,
            subject_track,
        };
    }

    /// With a forward-only sensing tier (`depth`, `camera`), the bearing to
    /// the nearest person within [`LOOK_ZONE_M`] of the distance the guard
    /// keeps from them: the sensor turns to whoever the guard acts on, so
    /// their track stays measured. `None` otherwise.
    pub(super) fn mover_look(&self, pos: V2, now_ms: u64) -> Option<f32> {
        let tier = self.sensing.map(|s| s.tier)?;
        if !matches!(tier, SensingTier::Depth | SensingTier::Camera) {
            return None;
        }
        self.obstacles
            .movers
            .guarded(now_ms)
            .filter(|m| m.is_person())
            .map(|m| (m.at(now_ms), guard_keep_m(m, now_ms)))
            .map(|(p, keep)| (p, geo::dist(p, pos) - keep))
            .filter(|&(_, d)| d <= LOOK_ZONE_M)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(p, _)| geo::bearing_of(geo::sub(p, pos)))
    }

    /// The people the guard keeps clear of now, as subjects of the subject
    /// brake (armed at once: a tracker's person needs no settling), into
    /// [`MoverGuards`] for this tick's [`Pilot::movers_soft`] and
    /// [`Pilot::movers_cut`].
    pub(super) fn mover_guards(&mut self, now_ms: u64) {
        self.mover_flight(now_ms);
        let guards = &mut self.mover_guards.0;
        guards.clear();
        guards.extend(
            self.obstacles
                .movers
                .guarded(now_ms)
                .filter(|m| m.is_person())
                .map(|m| Subject {
                    ne: m.at(now_ms),
                    keep: guard_keep_m(m, now_ms),
                    vel: m.vel,
                    armed: true,
                }),
        );
    }

    /// The soft tier for every guarded person in turn: `want` held so the
    /// shaped velocity slows for each on the goal profile. `None` when
    /// nothing was held.
    pub(super) fn movers_soft(
        &mut self,
        pos: V2,
        want: V2,
        a_h: f32,
        v_cap: f32,
        now_ms: u64,
    ) -> Option<V2> {
        let mut out = None;
        for i in 0..self.mover_guards.0.len() {
            let s = self.mover_guards.0[i];
            let w = out.unwrap_or(want);
            if let Some(held) = self.subject_soft(&s, pos, w, a_h, v_cap, now_ms) {
                out = Some(held);
            }
        }
        out
    }

    /// The hard tier for every guarded person in turn: `ask` (what the FC
    /// would fly) and the shaped `v` cut toward each, to fly from where the
    /// vehicle is; with whether `v_cap` cut a slide. `None` when nothing was
    /// cut.
    pub(super) fn movers_cut(
        &mut self,
        pos: V2,
        ask: V2,
        v: V2,
        v_cap: f32,
        now_ms: u64,
    ) -> Option<(V2, bool)> {
        let mut out: Option<(V2, bool)> = None;
        for i in 0..self.mover_guards.0.len() {
            let s = self.mover_guards.0[i];
            let (a, vv) = out.map_or((ask, v), |(c, _)| (c, c));
            if let Some((cut, capped)) = self.subject_cut(&s, pos, a, vv, v_cap, now_ms) {
                out = Some((cut, capped || out.is_some_and(|o| o.1)));
            }
        }
        out
    }
}
