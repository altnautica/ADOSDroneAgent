//! The subject brake: the subject a skill's goal names (`subject_ne`) is a
//! person on the ground, and the pilot keeps the vehicle clear of them at
//! the profile's stick acceleration whatever the skill asks. It needs no
//! obstacle sensor, so it holds with avoidance off too. While the subject is
//! near, the vehicle also climbs to fly clear over them.

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
/// The subject's velocity, taken from successive goals, is smoothed over
/// this long, seconds: a tracker's jitter on a standing subject reads as
/// little motion.
pub const SUBJECT_VEL_SMOOTH_S: f32 = 0.5;
/// Goals further apart than this give no velocity: it restarts from zero.
const SUBJECT_VEL_GAP_MS: u64 = 500;
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
/// velocity, and whether the vehicle is climbing clear.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubjectTrack {
    lease_id: Option<u64>,
    last: Option<(V2, u64)>,
    vel: V2,
    climbing: bool,
}

/// One tick's subject: where it is, the horizontal distance kept from it
/// (centre to the vehicle's CG), and its velocity.
#[derive(Debug, Clone, Copy)]
pub(super) struct Subject {
    pub ne: V2,
    pub keep: f32,
    pub vel: V2,
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
            Some((prev, prev_ms)) if at_ms > prev_ms && at_ms - prev_ms <= SUBJECT_VEL_GAP_MS => {
                let dt = (at_ms - prev_ms) as f32 / 1000.0;
                let raw = geo::scale(geo::sub(ne, prev), 1.0 / dt);
                let k = (dt / SUBJECT_VEL_SMOOTH_S).min(1.0);
                t.vel = geo::add(t.vel, geo::scale(geo::sub(raw, t.vel), k));
                t.last = Some((ne, at_ms));
            }
            _ => {
                t.vel = [0.0, 0.0];
                t.last = Some((ne, at_ms));
            }
        }
        let body = self.settings.pilot.body_radius_m();
        let keep = SUBJECT_BODY_RADIUS_M
            + body
            + SUBJECT_MARGIN_M
            + (SUBJECT_SIGMAS * sigma.unwrap_or(0.0).max(0.0)).min(SUBJECT_SIGMA_MAX_M);
        let d = geo::dist([pos[0], pos[1]], ne);
        let t = &mut self.subject_track;
        if d < keep + SUBJECT_CLIMB_ZONE_M {
            t.climbing = true;
        } else if d > keep + SUBJECT_CLIMB_ZONE_M + SUBJECT_CLIMB_HYST_M {
            t.climbing = false;
        }
        let vel = t.vel;
        let floor_d = if t.climbing {
            let floor = SUBJECT_BODY_HEIGHT_M + body + SUBJECT_CLEAR_M;
            self.agl(now_ms).map(|a| pos[2] + a - floor)
        } else {
            None
        };
        Some((Subject { ne, keep, vel }, floor_d))
    }

    /// The subject brake on a goal setpoint flown from `pos`: `ask` is what
    /// the FC would fly (the shaped velocity `v` plus its position loop's
    /// pull). Along the line to the subject, both are held relative to the
    /// subject's own speed along it, as the obstacle brake holds a closing
    /// speed: outside [`Subject::keep`], to the subject's speed away (an
    /// approach counts against) plus what still stops the closing between
    /// them that far short of it, after the FC's lag, at the profile's stick
    /// acceleration ([`stop_cap`]); inside it, to backing away at the
    /// subject's approach (no credit for it walking away) plus
    /// [`SUBJECT_PUSH_PER_S`] per metre inside. When either breaks the
    /// cap, `ask` with its motion toward the subject cut to the cap is
    /// returned, to fly from where the vehicle is: its sideways part is kept
    /// within `v_cap` (the vehicle slides round the subject), its motion
    /// away never is; with whether `v_cap` cut the slide.
    pub(super) fn subject_cut(
        &mut self,
        s: &Subject,
        pos: V2,
        ask: V2,
        v: V2,
        v_cap: f32,
        now_ms: u64,
    ) -> Option<(V2, bool)> {
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
        let cap = if d > s.keep {
            let closing = (geo::dot(vel, u) - s_u).max(0.0);
            s_u + stop_cap(d, closing, self.limits_now(now_ms).a_h, s.keep)
        } else {
            s_u.min(0.0) - SUBJECT_PUSH_PER_S * (s.keep - d)
        };
        let toward = geo::dot(ask, u);
        if toward <= cap && geo::dot(v, u) <= cap {
            return None;
        }
        let along = toward.min(cap).min(v_cap);
        let side = geo::sub(ask, geo::scale(u, toward));
        let room = (v_cap * v_cap - along * along).max(0.0).sqrt();
        let capped = geo::norm(side) > room + 1e-3;
        let cut = geo::add(geo::scale(u, along), geo::clamp_len(side, room));
        self.brake_avoid = if progress(ask, cut) < STOPPED_MPS {
            AvoidState::Blocked
        } else {
            AvoidState::Detour
        };
        Some((cut, capped))
    }
}
