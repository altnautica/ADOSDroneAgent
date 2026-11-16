//! The subject brake: the subject a skill's goal names (`subject_ne`) is a
//! person on the ground, and the pilot keeps the vehicle clear of them
//! whatever the skill asks. Like the obstacle brake it has two tiers: a soft
//! one on the goal profile's acceleration slows what the goal asks before
//! the shaper ([`Pilot::subject_soft`]), and a hard one on the profile's
//! stick acceleration cuts what the shaper flies when the subject closes
//! faster than the goal profile can slow ([`Pilot::subject_cut`]). It needs
//! no obstacle sensor, so it holds with avoidance off too. While the subject
//! is near, the vehicle also climbs to fly clear over them.

use ados_protocol::pilot::{AvoidState, Owner};

use super::{progress, stop_cap, STOPPED_MPS};
use crate::core::Pilot;
use crate::geo::{self, V2};

/// The subject's body: a cylinder of this radius standing on the ground,
/// metres.
pub const SUBJECT_BODY_RADIUS_M: f32 = 0.4;
/// The subject's body height, metres.
pub const SUBJECT_BODY_HEIGHT_M: f32 = 2.0;
/// Room kept between the subject's body and the vehicle's, metres.
pub const SUBJECT_MARGIN_M: f32 = 0.5;
/// The kept distance grows by this many of the goal's `subject_sigma_m`.
pub const SUBJECT_SIGMAS: f32 = 2.0;
/// ... by at most this much, metres: a subject known no better is kept
/// clear by the push and the climb, not by an ever wider distance.
pub const SUBJECT_SIGMA_MAX_M: f32 = 1.5;
/// Inside the kept distance the vehicle backs away at this many m/s per
/// metre inside it, on top of the subject's own approach.
pub const SUBJECT_PUSH_PER_S: f32 = 1.0;
/// The soft tier keeps this much further off than the hard tier, metres:
/// the shaper's jerk-limited lag past the soft cap then ends before the
/// hard boundary, so an approach on the goal profile is never cut.
pub const SUBJECT_SOFT_EXTRA_M: f32 = 0.4;
/// Outside the kept distance the hard tier acts only within this of it,
/// metres; further off the soft tier alone slows the vehicle.
pub const SUBJECT_HARD_ZONE_M: f32 = 1.0;
/// The subject's velocity, taken from successive goals, is smoothed over
/// this long, seconds: a tracker's jitter on a standing subject reads as
/// little motion.
pub const SUBJECT_VEL_SMOOTH_S: f32 = 0.5;
/// Goals further apart than this give no velocity: it restarts from zero
/// (the subject stays named, so the brake stays armed).
const SUBJECT_VEL_GAP_MS: u64 = 500;
/// A subject stepping faster than this between goals, m/s, is no person
/// moving but a new subject (a re-designation or a re-acquisition): it is
/// named afresh, its velocity restarting from zero.
pub const SUBJECT_JUMP_MPS: f32 = 8.0;
/// The step is judged over at least this long, seconds (a skill's 10 Hz
/// goal period): two goals arriving close together never read as a jump.
const SUBJECT_JUMP_MIN_S: f32 = 0.1;
/// No braking for the subject, soft or hard, until this long after it is
/// named (its first goal after none, a step faster than
/// [`SUBJECT_JUMP_MPS`], a new lease or owner): the tracker's acquisition
/// settles first. The climb clear does not wait.
pub const SUBJECT_ARM_MS: u64 = 3000;
/// The vehicle climbs clear once the subject is within this of the kept
/// distance, metres.
pub const SUBJECT_CLIMB_ZONE_M: f32 = 3.0;
/// ... and holds the climb until the subject is this much further off,
/// metres.
pub const SUBJECT_CLIMB_HYST_M: f32 = 1.0;
/// While climbing clear, the vehicle's outermost extent flies at least this
/// high over the subject's body, metres.
pub const SUBJECT_CLEAR_M: f32 = 1.0;

/// What the pilot keeps of the subject across ticks: the lease whose goals
/// named it, the latest subject and its goal's arrival, its smoothed
/// velocity, when it was named, and, while the vehicle climbs clear, the
/// ground's down position taken when the climb began.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubjectTrack {
    lease_id: Option<u64>,
    last: Option<(V2, u64)>,
    vel: V2,
    named_ms: u64,
    climbing: bool,
    ground_d: Option<f32>,
}

/// One tick's subject: where it is, the horizontal distance kept from it
/// (centre to the vehicle's CG), its velocity, and whether the brake is
/// armed ([`SUBJECT_ARM_MS`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct Subject {
    pub ne: V2,
    pub keep: f32,
    pub vel: V2,
    pub armed: bool,
}

impl Pilot {
    /// The subject of the skill's current goal, if it names one, with the
    /// highest setpoint down position (NED) the climb clear allows while it
    /// is latched and the height above ground is known. Tracks its velocity
    /// over the goals that name it; anything else (no `subject_ne`, another
    /// owner or lease) clears the track.
    pub(super) fn track_subject(
        &mut self,
        pos: [f32; 3],
        now_ms: u64,
    ) -> Option<(Subject, Option<f32>)> {
        let named = match self.owner() {
            Owner::Skill => self.auth.lease().and_then(|l| {
                let g = l.goal.as_ref()?;
                Some((l.id, l.last_goal_ms, g.subject_ne?, g.subject_sigma_m))
            }),
            _ => None,
        };
        let Some((lease_id, at_ms, ne, sigma)) = named else {
            self.subject_track = SubjectTrack::default();
            return None;
        };
        let t = &mut self.subject_track;
        if t.lease_id != Some(lease_id) {
            *t = SubjectTrack {
                lease_id: Some(lease_id),
                ..SubjectTrack::default()
            };
        }
        match t.last {
            Some((_, prev_ms)) if at_ms == prev_ms => {}
            None => {
                t.vel = [0.0, 0.0];
                t.named_ms = at_ms;
            }
            Some((prev, prev_ms)) => {
                let dt_ms = at_ms.saturating_sub(prev_ms);
                let dt = dt_ms as f32 / 1000.0;
                let step = geo::dist(ne, prev);
                if step > SUBJECT_JUMP_MPS * dt.max(SUBJECT_JUMP_MIN_S) {
                    t.vel = [0.0, 0.0];
                    t.named_ms = at_ms;
                } else if dt_ms == 0 || dt_ms > SUBJECT_VEL_GAP_MS {
                    t.vel = [0.0, 0.0];
                } else {
                    let raw = geo::scale(geo::sub(ne, prev), 1.0 / dt);
                    let k = (dt / SUBJECT_VEL_SMOOTH_S).min(1.0);
                    t.vel = geo::add(t.vel, geo::scale(geo::sub(raw, t.vel), k));
                }
            }
        }
        t.last = Some((ne, at_ms));
        let armed = now_ms >= t.named_ms + SUBJECT_ARM_MS;
        let body = self.settings.pilot.body_radius_m();
        let keep = SUBJECT_BODY_RADIUS_M
            + body
            + SUBJECT_MARGIN_M
            + (SUBJECT_SIGMAS * sigma.unwrap_or(0.0).max(0.0)).min(SUBJECT_SIGMA_MAX_M);
        let d = geo::dist([pos[0], pos[1]], ne);
        let agl = self.agl(now_ms);
        let t = &mut self.subject_track;
        if d < keep + SUBJECT_CLIMB_ZONE_M {
            t.climbing = true;
        } else if d > keep + SUBJECT_CLIMB_ZONE_M + SUBJECT_CLIMB_HYST_M {
            t.climbing = false;
            t.ground_d = None;
        }
        // The ground is taken once, when the climb begins (or when a height
        // above ground first reads during it): a rangefinder stuck low then
        // raises the setpoint once, never a climb that follows the vehicle up.
        if t.climbing && t.ground_d.is_none() {
            t.ground_d = agl.map(|a| pos[2] + a);
        }
        let vel = t.vel;
        let floor_d = t
            .ground_d
            .map(|g| g - (SUBJECT_BODY_HEIGHT_M + body + SUBJECT_CLEAR_M));
        let subject = Subject {
            ne,
            keep,
            vel,
            armed,
        };
        Some((subject, floor_d))
    }

    /// The direction from `pos` to the subject, the most a velocity may go
    /// along it, and the distance to it. The cap is held relative to the
    /// subject's own speed along the line, as the obstacle brake holds a
    /// closing speed: outside `keep`, the subject's speed away (an approach
    /// counts against) plus what still stops the closing between them that
    /// far short of it, after the FC's lag, at `a` ([`stop_cap`]); inside
    /// it, backing away at the subject's approach (no credit for it walking
    /// away) plus [`SUBJECT_PUSH_PER_S`] per metre inside.
    fn subject_cap(&self, s: &Subject, pos: V2, keep: f32, a: f32, now_ms: u64) -> (V2, f32, f32) {
        let vel = self
            .fc
            .fresh_local(now_ms)
            .map_or([0.0; 2], |l| [l.vn, l.ve]);
        let rel = geo::sub(s.ne, pos);
        let d = geo::norm(rel);
        let u = if d > 1e-6 {
            geo::scale(rel, 1.0 / d)
        } else if geo::norm(vel) > 1e-6 {
            geo::scale(vel, 1.0 / geo::norm(vel))
        } else {
            [1.0, 0.0]
        };
        // The subject's speed along the line, positive moving away.
        let s_u = geo::dot(s.vel, u);
        let cap = if d > keep {
            let closing = (geo::dot(vel, u) - s_u).max(0.0);
            s_u + stop_cap(d, closing, a, keep)
        } else {
            s_u.min(0.0) - SUBJECT_PUSH_PER_S * (keep - d)
        };
        (u, cap, d)
    }

    /// The soft tier, before the shaper: what a goal asks (`want`, within
    /// `v_cap`) held to the subject cap ([`Pilot::subject_cap`]) at the goal
    /// limits' acceleration `a_h`, kept [`SUBJECT_SOFT_EXTRA_M`] further
    /// off than the hard tier's, so the shaped velocity slows and settles
    /// for the subject on the goal profile before the hard tier would have
    /// to cut it. `None` while unarmed or when nothing is cut.
    pub(super) fn subject_soft(
        &mut self,
        s: &Subject,
        pos: V2,
        want: V2,
        a_h: f32,
        v_cap: f32,
        now_ms: u64,
    ) -> Option<V2> {
        if !s.armed {
            return None;
        }
        let keep = s.keep + SUBJECT_SOFT_EXTRA_M;
        let (u, cap, _) = self.subject_cap(s, pos, keep, a_h, now_ms);
        if geo::dot(want, u) <= cap {
            return None;
        }
        let (held, _) = hold_along(want, u, cap, v_cap);
        // As the obstacle brake's soft tier: the avoid state says the brake
        // slowed the goal.
        self.brake_avoid = if progress(want, held) < STOPPED_MPS {
            AvoidState::Blocked
        } else {
            AvoidState::Detour
        };
        Some(held)
    }

    /// The hard tier, a safety cut on a goal setpoint flown from `pos`:
    /// `ask` is what the FC would fly (the shaped velocity `v` plus its
    /// position loop's pull). Both are held to the subject cap
    /// ([`Pilot::subject_cap`]) at the profile's stick acceleration, inside
    /// the kept distance and within [`SUBJECT_HARD_ZONE_M`] of it; further
    /// off only the soft tier acts. When either breaks the cap, `ask` with
    /// its motion toward the subject cut to the cap is returned, to fly from
    /// where the vehicle is: its sideways part is kept within `v_cap` (the
    /// vehicle slides round the subject), its motion away never is; with
    /// whether `v_cap` cut the slide. `None` while unarmed or when nothing
    /// is cut.
    pub(super) fn subject_cut(
        &mut self,
        s: &Subject,
        pos: V2,
        ask: V2,
        v: V2,
        v_cap: f32,
        now_ms: u64,
    ) -> Option<(V2, bool)> {
        if !s.armed {
            return None;
        }
        let a_h = self.limits_now(now_ms).a_h;
        let (u, cap, d) = self.subject_cap(s, pos, s.keep, a_h, now_ms);
        if d >= s.keep + SUBJECT_HARD_ZONE_M {
            return None;
        }
        if geo::dot(ask, u) <= cap && geo::dot(v, u) <= cap {
            return None;
        }
        let (cut, capped) = hold_along(ask, u, cap, v_cap);
        self.brake_avoid = if progress(ask, cut) < STOPPED_MPS {
            AvoidState::Blocked
        } else {
            AvoidState::Detour
        };
        Some((cut, capped))
    }
}

/// `v` with its part along `u` held to at most `cap` and to `v_cap` either
/// way, its sideways part kept within what `v_cap` leaves. With whether
/// `v_cap` cut it.
fn hold_along(v: V2, u: V2, cap: f32, v_cap: f32) -> (V2, bool) {
    let toward = geo::dot(v, u);
    let held = toward.min(cap);
    let along = held.clamp(-v_cap, v_cap);
    let side = geo::sub(v, geo::scale(u, toward));
    let room = (v_cap * v_cap - along * along).max(0.0).sqrt();
    let capped = geo::norm(side) > room + 1e-3 || held < -v_cap;
    (
        geo::add(geo::scale(u, along), geo::clamp_len(side, room)),
        capped,
    )
}
