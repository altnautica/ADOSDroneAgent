//! A `follow_intent` goal in the control loop. Every
//! [`VIEW_PERIOD_MS`] the view planner ([`crate::planner::ViewPlanner`])
//! picks the viewpoint over the subject's predictions (the vision engine's
//! `vision.subject.v1` record the intent names, else the intent's own
//! subject and velocity flown on in a line), and the intent becomes the
//! follow goal flown: the viewpoint, the subject's velocity fed forward,
//! the ring round the subject at the viewpoint's standoff and bearing for
//! the grid planner's detours, and the intent's subject, margin and
//! heading. Both planners and the subject keep-out then fly it as any
//! follow goal. While the trajectory planner flies, the view its committed
//! trajectory keeps of the subject is scored back to the view planner.
//! With no viewpoint picked (no subject, or every viewpoint inside an
//! obstacle's margin) the intent's own point is flown.

use ados_protocol::pilot::{BearingFrame, Candidates, Goal, GoalKind, Owner, VelMode};
use ados_protocol::vision_subject::PREDICTION_HORIZONS_S;

use super::super::sensing::planar_seen_m;
use super::super::subject_guard::subject_keep;
use crate::core::subject_feed::{self, FeedSubject, Record};
use crate::core::Pilot;
use crate::geo::{self, V2};
use crate::obstacles::Obstacles;
use crate::planner::{
    sees_subject, Prediction, Sight, SubjectView, ViewAsk, ViewChoice, ViewPlanner, VIEW_PERIOD_MS,
};
use crate::world::VoxWorld;

/// An intent naming no standoff band keeps this far either side of its own
/// point's distance from the subject, m.
const BAND_HALF_M: f32 = 1.0;
/// The band's near edge keeps this beyond the subject keep-out, m: a
/// viewpoint is never one the keep-out bars.
const BAND_KEEP_ROOM_M: f32 = 0.5;
/// A route keeps this far inside the near edge of an intent's standoff
/// band clear of the subject, m: its way to a viewpoint round the subject
/// never closes in on them past it.
const INTENT_KEEP_INSIDE_M: f32 = 1.5;
/// The committed trajectory's view of the subject is checked this far
/// ahead, s.
const PLAN_LOOK_S: [f32; 4] = [0.5, 1.0, 1.5, 2.0];
/// The ring a flown intent gives the grid planner spans this, degrees.
const RING_SPAN_DEG: f32 = 90.0;
/// A 3D line of sight is sampled this often, m.
const VOX_STEP_M: f32 = 0.1;
/// The subject's height when only the intent names it, m.
const SUBJECT_HEIGHT_M: f32 = 1.7;
/// The subject's velocity is fed forward whole within this of the
/// viewpoint, m ...
const FF_WHOLE_M: f32 = 1.0;
/// ... and only along the way to it from this far off it, m.
const FF_ALONG_M: f32 = 3.0;

/// The view planner's state in the pilot.
#[derive(Debug, Default)]
pub(in crate::core) struct FollowView {
    /// The latest subject record from the vision engine.
    feed: Option<FeedSubject>,
    planner: ViewPlanner,
    /// The goal flown for the intent, and when it was made.
    flown: Option<(Goal, u64)>,
    choice: Option<ViewChoice>,
    view_ms: Option<u64>,
}

impl FollowView {
    /// A `vision.subject.v1` record that arrived at `now_ms`.
    pub fn on_subject(&mut self, payload: &serde_json::Value, now_ms: u64) {
        match subject_feed::parse(payload, now_ms) {
            Some(Record::State(s)) => self.feed = Some(s),
            Some(Record::Released(id)) if self.feed.as_ref().is_some_and(|f| f.id == id) => {
                self.feed = None
            }
            _ => {}
        }
    }

    fn clear(&mut self) {
        self.planner.clear();
        self.flown = None;
        self.choice = None;
        self.view_ms = None;
    }
}

/// The 2D obstacle grid at flight height, the scans as of the pilot clock's
/// `.1` ms.
struct GridSight<'a>(&'a Obstacles, u64);

impl Sight for GridSight<'_> {
    fn clearance(&self, ne: V2, _d: f32) -> f32 {
        self.0.grid.clearance(ne).unwrap_or(f32::INFINITY)
    }

    fn sees(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        self.0.line_of_sight([from[0], from[1]], [to[0], to[1]])
    }

    /// Within the planar scans' seen range along the way there.
    fn seen(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        let (a, b) = ([from[0], from[1]], [to[0], to[1]]);
        let len = geo::dist(a, b);
        len < 0.1 || len <= planar_seen_m(self.0, a, geo::scale(geo::sub(b, a), 1.0 / len), self.1)
    }
}

/// The live voxel map: the line of sight through its occupied voxels (an
/// unobserved voxel hides nothing), the clearance from its slab at flight
/// height (the grid).
struct VoxSight<'a> {
    obs: &'a Obstacles,
    world: &'a VoxWorld,
}

impl Sight for VoxSight<'_> {
    fn clearance(&self, ne: V2, _d: f32) -> f32 {
        self.obs.grid.clearance(ne).unwrap_or(f32::INFINITY)
    }

    fn sees(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        let n = (len / VOX_STEP_M).ceil().max(1.0) as usize;
        (0..=n).all(|k| {
            let s = k as f32 / n as f32;
            !self
                .world
                .occupied([from[0] + d[0] * s, from[1] + d[1] * s, from[2] + d[2] * s])
        })
    }

    /// The map observed the viewpoint's voxel free.
    fn seen(&self, _from: [f32; 3], to: [f32; 3]) -> bool {
        self.world.known_free(to)
    }
}

/// The goal flown for `intent`: its viewpoint `c` (when one was picked)
/// round subject `s`, else its own point. The subject's velocity is fed
/// forward whole while the vehicle at `from` holds its viewpoint; the
/// further it is off it, the more of the feed-forward goes along the way to
/// it ([`ff_toward`]).
fn flown_goal(
    intent: &Goal,
    c: Option<&ViewChoice>,
    s: Option<&SubjectView>,
    from: Option<V2>,
) -> Goal {
    let mut g = Goal::new(
        GoalKind::Follow,
        [intent.n, intent.e, intent.d],
        intent.max_speed_mps,
    );
    let vel = s.map_or([intent.vn, intent.ve], |s| s.vel);
    let vel = match (c, from) {
        (Some(c), Some(p)) => ff_toward(vel, p, c.ne),
        _ => vel,
    };
    g.vn = vel[0];
    g.ve = vel[1];
    g.vd = intent.vd;
    g.vel_mode = VelMode::FeedForward;
    g.yaw = intent.yaw;
    g.yaw_rate = intent.yaw_rate;
    g.subject_ne = intent.subject_ne.or(s.map(|s| s.ne));
    g.subject_sigma_m = intent.subject_sigma_m;
    g.max_speed_mps = intent.max_speed_mps;
    g.margin_m = intent.margin_m;
    if let (Some(c), Some(s)) = (c, s) {
        g.n = c.ne[0];
        g.e = c.ne[1];
        g.d = c.d;
        g.candidates = Some(Candidates {
            ring_center_ne: s.ne,
            radius_m: c.radius,
            angle_deg: c.bearing.to_degrees(),
            angle_span_deg: RING_SPAN_DEG,
        });
    }
    g
}

/// The feed-forward `vel` for a vehicle at `from` flying to its viewpoint
/// `to`: whole within [`FF_WHOLE_M`] of it; from [`FF_ALONG_M`] off it only
/// its part along the way there (none backward), so a subject walking
/// across the way does not push the vehicle sideways into what lines it;
/// blended between.
fn ff_toward(vel: V2, from: V2, to: V2) -> V2 {
    let off = geo::sub(to, from);
    let dist = geo::norm(off);
    let w = ((dist - FF_WHOLE_M) / (FF_ALONG_M - FF_WHOLE_M)).clamp(0.0, 1.0);
    if w <= 0.0 {
        return vel;
    }
    let dir = geo::scale(off, 1.0 / dist);
    let along = geo::scale(dir, geo::dot(vel, dir).max(0.0));
    geo::add(geo::scale(vel, 1.0 - w), geo::scale(along, w))
}

impl Pilot {
    /// The skill's lease goal, when it is a `follow_intent`.
    fn intent_ref(&self) -> Option<&Goal> {
        if self.owner() != Owner::Skill {
            return None;
        }
        let g = self.auth.lease()?.goal.as_ref()?;
        (g.kind == GoalKind::FollowIntent).then_some(g)
    }

    /// The goal the skill's lease flies: its own, or the one made for its
    /// intent ([`Pilot::view_tick`]).
    pub(in crate::core) fn skill_goal(&self) -> Option<&Goal> {
        let g = self.auth.lease()?.goal.as_ref()?;
        if g.kind == GoalKind::FollowIntent {
            self.follow_view.flown.as_ref().map(|f| &f.0)
        } else {
            Some(g)
        }
    }

    /// How long ago the goal the skill's lease flies was made, ms.
    pub(in crate::core) fn skill_goal_age_ms(&self, now_ms: u64) -> u64 {
        let Some(l) = self.auth.lease() else {
            return 0;
        };
        let made = match (&l.goal, &self.follow_view.flown) {
            (Some(g), Some((_, at))) if g.kind == GoalKind::FollowIntent => *at,
            _ => l.last_goal_ms,
        };
        now_ms.saturating_sub(made)
    }

    /// The viewpoint the view planner picked last, while an intent flies.
    pub fn view_choice(&self) -> Option<ViewChoice> {
        self.follow_view.choice
    }

    /// The distance from the subject a route keeps while the lease goal is
    /// a `follow_intent` with a standoff band ([`INTENT_KEEP_INSIDE_M`]
    /// inside its near edge); 0 otherwise.
    pub(in crate::core) fn intent_keep_m(&self) -> f32 {
        self.intent_ref()
            .and_then(|g| g.standoff_band_m)
            .map_or(0.0, |b| b[0].min(b[1]) - INTENT_KEEP_INSIDE_M)
    }

    /// Every [`VIEW_PERIOD_MS`] while the lease goal is a `follow_intent`:
    /// score the committed trajectory's view, pick the viewpoint and make
    /// the goal flown. Anything else forgets the view.
    pub(in crate::core) fn view_tick(&mut self, now_ms: u64) {
        if self.intent_ref().is_none() {
            self.follow_view.clear();
            return;
        }
        let due = self
            .follow_view
            .view_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= VIEW_PERIOD_MS);
        if !due && self.follow_view.flown.is_some() {
            return;
        }
        let Some(intent) = self.intent_ref().cloned() else {
            return;
        };
        self.follow_view.view_ms = Some(now_ms);
        let pos = self.pos(now_ms);
        let s = pos.and_then(|p| self.subject_view(&intent, p, now_ms));
        let choice = match (pos, &s) {
            (Some(p), Some(s)) => {
                self.note_traj_view(s, p[2], now_ms);
                self.pick_view(&intent, s, p, now_ms)
            }
            _ => None,
        };
        self.follow_view.choice = choice;
        let from = pos.map(|p| [p[0], p[1]]);
        let flown = flown_goal(&intent, choice.as_ref(), s.as_ref(), from);
        self.follow_view.flown = Some((flown, now_ms));
    }

    /// The subject now: the vision engine's record the intent names, while
    /// it is recent, else the intent's subject flown on at its velocity.
    fn subject_view(&self, g: &Goal, pos: [f32; 3], now_ms: u64) -> Option<SubjectView> {
        let feed = self
            .follow_view
            .feed
            .as_ref()
            .filter(|f| g.subject_ref.is_none_or(|id| id == f.id))
            .and_then(|f| f.view(now_ms));
        if feed.is_some() {
            return feed;
        }
        let ne = g.subject_ne.filter(|n| n.iter().all(|x| x.is_finite()))?;
        let vel = [g.vn, g.ve];
        let since = self
            .auth
            .lease()
            .map_or(0.0, |l| {
                now_ms.saturating_sub(l.last_goal_ms) as f32 / 1000.0
            })
            .min(0.5);
        let ne = geo::add(ne, geo::scale(vel, since));
        Some(SubjectView {
            ne,
            vel,
            ground_d: self.agl(now_ms).map(|a| pos[2] + a),
            height_m: SUBJECT_HEIGHT_M,
            preds: PREDICTION_HORIZONS_S
                .iter()
                .map(|&t| Prediction {
                    dt_s: t,
                    ne: geo::add(ne, geo::scale(vel, t)),
                })
                .collect(),
            turn_prob: 0.0,
            unseen_s: 0.0,
        })
    }

    /// The viewpoint for `intent` round `s`, the vehicle at `pos`.
    fn pick_view(
        &mut self,
        intent: &Goal,
        s: &SubjectView,
        pos: [f32; 3],
        now_ms: u64,
    ) -> Option<ViewChoice> {
        let ground_d = self.agl(now_ms).map(|a| pos[2] + a);
        let keep = subject_keep(self.settings.pilot.body_radius_m(), intent.subject_sigma_m)
            + BAND_KEEP_ROOM_M;
        let band = intent.standoff_band_m.unwrap_or_else(|| {
            let r = geo::dist([intent.n, intent.e], s.ne);
            [r - BAND_HALF_M, r + BAND_HALF_M]
        });
        let near = band[0].min(band[1]).max(keep);
        let band = [near, band[0].max(band[1]).max(near)];
        let own_h = ground_d.map_or(0.0, |g| g - intent.d);
        let ask = ViewAsk {
            band,
            bearing_pref: intent
                .bearing_pref_deg
                .filter(|b| b.is_finite())
                .map(f32::to_radians),
            bearing_world: intent.bearing_frame == BearingFrame::World,
            heights: intent.height_band_m.unwrap_or([own_h, own_h]),
            d_pref: intent.d,
            ground_d,
            margin: self.margin(),
            require_los: intent.require_los,
        };
        let v = &mut self.follow_view.planner;
        if self.world.live_3d(now_ms) {
            let sight = VoxSight {
                obs: &self.obstacles,
                world: &self.world,
            };
            v.choose(s, &ask, pos, &sight, now_ms)
        } else {
            v.choose(s, &ask, pos, &GridSight(&self.obstacles, now_ms), now_ms)
        }
    }

    /// While the trajectory planner flies the intent, the share of the
    /// subject's predictions its committed trajectory sees, scored back to
    /// the view planner.
    fn note_traj_view(&mut self, s: &SubjectView, d: f32, now_ms: u64) {
        if !self.traj_flies() || self.follow_view.choice.is_none() {
            return;
        }
        let ground_d = self.agl(now_ms).map(|a| d + a);
        let mut seen = 0usize;
        for t in PLAN_LOOK_S {
            let Some(r) = self.traj.sample(now_ms + (t * 1000.0) as u64) else {
                return;
            };
            let from = [r.p[0], r.p[1], d];
            let ok = if self.world.live_3d(now_ms) {
                let sight = VoxSight {
                    obs: &self.obstacles,
                    world: &self.world,
                };
                sees_subject(&sight, s, s.at(t), from, ground_d)
            } else {
                sees_subject(
                    &GridSight(&self.obstacles, now_ms),
                    s,
                    s.at(t),
                    from,
                    ground_d,
                )
            };
            seen += usize::from(ok);
        }
        let vis = seen as f32 / PLAN_LOOK_S.len() as f32;
        self.follow_view.planner.note_plan_visibility(vis);
    }
}
