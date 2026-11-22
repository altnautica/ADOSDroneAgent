//! A guard's back-away on a forward-looking tier (`depth`, `camera`). The
//! subject brake and the person guard back the vehicle straight away from
//! someone near, mostly the way it came; on a forward camera's map that way
//! was seen as the vehicle flew it, so the seen-space hold lets the
//! back-away through, though anyone may have walked into it since (a
//! crossing walker last tracked tens of seconds before, backed into at
//! 1 m/s with the motion 160° off the nose), and backing along a walker's
//! own path only keeps them coming. A back-away along a way the camera has
//! not looked along lately (more than [`VIEW_HALF_DEG`] off every heading
//! the nose held over the last [`RECENT_MS`]) is flown instead along the
//! way looked along that keeps everyone near clearest over [`HORIZON_S`]
//! (their own motion counted), never toward anyone near and no further than
//! the seen-clear range there allows (a sidestep in view); only
//! [`RETREAT_CREEP_MPS`] of the back-away leaves those ways. Meanwhile the
//! nose turns at the profile's own yaw rate ([`Pilot::look_limits`]) to the
//! clearest way all round, so the camera sees it and the back-away flies on
//! along it, and keeps looking along the back-away while it flies on; once
//! the back-away ends the nose turns back to whoever the guard acts on.

use ados_protocol::pilot::{AvoidMode, SensingTier};

use super::mover_guard::LOOK_ZONE_M;
use super::subject_guard::Subject;
use crate::core::{limit, Pilot};
use crate::geo::{self, V2};
use crate::model::GoalLimits;

/// The most a back-away flies along a way the camera has not looked along
/// lately, m/s.
pub const RETREAT_CREEP_MPS: f32 = 0.3;
/// How far off a heading the camera looks along a way, degrees: inside an
/// 87° camera's half view, with the body's width to spare.
const VIEW_HALF_DEG: f32 = 40.0;
/// How long a way the camera looked along counts as seen, ms: a walker
/// outside the view then crosses no more than about 1.4 m of it since.
pub const RECENT_MS: u64 = 1000;
/// The headings logged for that, one per this many ms.
const LOG_EVERY_MS: u64 = 100;
const LOG_LEN: usize = (RECENT_MS / LOG_EVERY_MS) as usize + 1;
/// The ways tried all round (10° apart).
const WAYS: usize = 36;
/// The least speed a sidestep may fly, m/s (under the goal's cap): a
/// sideways step clears a walker's path at least this fast.
const ESCAPE_MPS: f32 = 1.5;
/// How far ahead the clearance from everyone near is judged, s ...
const HORIZON_S: f32 = 2.0;
/// ... from this far ahead, s: judged from now, a vehicle already inside
/// the kept distance finds every way as near as it is now, and the
/// clearest no clearer than standing still.
const LEAD_S: f32 = 0.5;
/// How long the nose keeps looking along the way after the last tick that
/// held a back-away, ms.
const RETREAT_LOOK_MS: u64 = 1000;
/// How long after that the nose still turns at the profile's yaw rate, ms:
/// back to the way it flies or whoever it faces (a goal's rate left it
/// looking aside for seconds while a trajectory flew on).
const LOOK_RETURN_MS: u64 = 2000;

/// The headings the nose held lately, one per [`LOG_EVERY_MS`], with when.
#[derive(Debug, Clone, Default)]
pub(in crate::core) struct ViewLog {
    log: [(f32, u64); LOG_LEN],
    next: usize,
}

impl ViewLog {
    /// Log `heading` at `now_ms`, once per [`LOG_EVERY_MS`].
    fn record(&mut self, heading: f32, now_ms: u64) {
        let last = self.log[(self.next + LOG_LEN - 1) % LOG_LEN].1;
        if last == 0 || now_ms >= last + LOG_EVERY_MS {
            self.log[self.next] = (heading, now_ms.max(1));
            self.next = (self.next + 1) % LOG_LEN;
        }
    }

    /// The camera looked along bearing `b` (within `half` of a heading)
    /// over the last [`RECENT_MS`].
    fn looked(&self, b: f32, half: f32, now_ms: u64) -> bool {
        self.log.iter().any(|&(h, at)| {
            at != 0 && now_ms <= at + RECENT_MS && geo::wrap_pi(b - h).abs() <= half
        })
    }
}

impl Pilot {
    /// `v`, a velocity the subject brake or the person guard set (`limits`
    /// carries [`limit::SUBJECT`] or [`limit::PERSON`]; `subject` the goal's
    /// subject), held to the ways the camera looked along lately on a
    /// forward-looking tier's live map, at most `v_cap`, and the look set
    /// ([`Pilot::retreat_look`]); `None` when nothing was held (looked
    /// along, slow, no back-away, another tier). Logs the nose each tick.
    pub(super) fn retreat_view(
        &mut self,
        v: V2,
        subject: Option<&Subject>,
        limits: u16,
        v_cap: f32,
        now_ms: u64,
    ) -> Option<V2> {
        let nose = self.yaw(now_ms);
        self.view_log.record(nose, now_ms);
        if limits & (limit::SUBJECT | limit::PERSON) == 0
            || self.settings.pilot.avoid == AvoidMode::Off
            || !self.world.live_3d(now_ms)
            || geo::norm(v) <= RETREAT_CREEP_MPS
        {
            return None;
        }
        let tier = self.world.tier();
        if !matches!(tier, SensingTier::Depth | SensingTier::Camera) {
            return None;
        }
        let pos = self.pos(now_ms)?;
        let p = [pos[0], pos[1]];
        let half = VIEW_HALF_DEG.to_radians();
        let way_v = geo::bearing_of(v);
        if self.view_log.looked(way_v, half, now_ms) {
            // Flown on along a way looked along lately, out of view now:
            // the nose keeps looking along it while the look lasts.
            if geo::wrap_pi(way_v - nose).abs() > half && self.retreat_look(now_ms).is_some() {
                self.retreat_look = Some((way_v, now_ms + RETREAT_LOOK_MS));
            }
            return None;
        }
        let looked = |b: f32| self.view_log.looked(b, half, now_ms);
        // Everyone near: the goal's subject and the people the guard keeps
        // clear of, within the look zone of the distance kept from them.
        let gap = |g: &Subject| geo::dist(p, g.ne) - g.keep;
        let people = || {
            subject
                .into_iter()
                .chain(self.mover_guards.people())
                .filter(move |g| gap(g) <= LOOK_ZONE_M)
        };
        let toward = |d: V2| people().any(|g| geo::dot(d, geo::sub(g.ne, p)) > 0.0);
        // How far inside the kept distance flying `u` brings anyone near
        // over the horizon (0 when it keeps clear of all of them).
        let inside = |u: V2| {
            people()
                .map(|g| miss_m(geo::sub(g.ne, p), g.vel, u) - g.keep)
                .fold(0.0, f32::min)
        };
        let speed = v_cap.min(geo::norm(v).max(ESCAPE_MPS));
        // Along the ways looked along: the clearest, the nearest to `v`
        // among equals. All round, for the look: the clearest, else `v`'s.
        let mut best = ([0.0; 2], inside([0.0; 2]));
        let mut way = (
            geo::bearing_of(v),
            inside(geo::scale(v, speed / geo::norm(v))),
        );
        for k in 0..WAYS {
            let b = std::f32::consts::TAU * k as f32 / WAYS as f32;
            let d = geo::bearing_unit(b);
            if toward(d) {
                continue;
            }
            let c = inside(geo::scale(d, speed));
            if c > way.1 + 1e-3 {
                way = (b, c);
            }
            if !looked(b) {
                continue;
            }
            let (_, max) = self.seen_rule(tier, self.world_clear_m(pos, d));
            for s in [geo::dot(v, d), speed] {
                let u = geo::scale(d, s.clamp(0.0, max));
                let c = inside(u);
                let nearer = geo::dist(u, v) < geo::dist(best.0, v);
                if c > best.1 + 1e-3 || (c > best.1 - 1e-3 && nearer) {
                    best = (u, c);
                }
            }
        }
        let out = geo::add(
            best.0,
            geo::clamp_len(geo::sub(v, best.0), RETREAT_CREEP_MPS),
        );
        self.retreat_look = Some((way.0, now_ms + RETREAT_LOOK_MS));
        Some(out)
    }

    /// The heading [`Pilot::retreat_view`] turns the nose to while it holds
    /// a back-away (and [`RETREAT_LOOK_MS`] after).
    pub(in crate::core) fn retreat_look(&self, now_ms: u64) -> Option<f32> {
        self.retreat_look
            .filter(|&(_, until)| now_ms < until)
            .map(|(b, _)| b)
    }

    /// The heading the nose turns to for the guards on a forward-looking
    /// tier: along a held back-away's way ([`Pilot::retreat_look`]), else
    /// to the person the guard acts on ([`Pilot::mover_look`]).
    pub(super) fn guard_look(&self, pos: V2, now_ms: u64) -> Option<f32> {
        self.retreat_look(now_ms)
            .or_else(|| self.mover_look(pos, now_ms))
    }

    /// The goal limits `gl` the nose turns at: while it looks along a
    /// held back-away's way ([`Pilot::retreat_look`]) and [`LOOK_RETURN_MS`]
    /// after, at the profile's own yaw rate (a goal's would take 6 s to look
    /// behind and as long to look back).
    pub(super) fn look_limits(&self, gl: &GoalLimits, now_ms: u64) -> GoalLimits {
        let mut l = *gl;
        if self
            .retreat_look
            .is_some_and(|(_, until)| now_ms < until + LOOK_RETURN_MS)
        {
            let dps = self.limits_now(now_ms).yaw_dps;
            l.yaw_dps = l.yaw_dps.max(dps);
            l.yaw_dps2 = l.yaw_dps2.max(2.0 * dps);
        }
        l
    }
}

/// The nearest a person at `r` from the vehicle, moving at `w`, comes from
/// [`LEAD_S`] to [`HORIZON_S`] ahead while the vehicle flies `u`, m.
fn miss_m(r: V2, w: V2, u: V2) -> f32 {
    let dv = geo::sub(w, u);
    let vv = geo::dot(dv, dv);
    let t = if vv < 1e-6 {
        LEAD_S
    } else {
        (-geo::dot(r, dv) / vv).clamp(LEAD_S, HORIZON_S)
    };
    geo::norm(geo::add(r, geo::scale(dv, t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_way_counts_as_looked_along_for_a_while() {
        let deg = |d: f32| d.to_radians();
        let half = deg(VIEW_HALF_DEG);
        let mut log = ViewLog::default();
        assert!(!log.looked(0.0, half, 1_000));
        // Looking north, then turning east over a second.
        for k in 0..=10 {
            log.record(deg(9.0 * k as f32), 1_000 + 100 * k);
        }
        let now = 2_000;
        assert!(log.looked(deg(-30.0), half, now));
        assert!(log.looked(deg(120.0), half, now));
        assert!(!log.looked(deg(180.0), half, now));
        assert!(!log.looked(deg(-60.0), half, now));
        // North fades once it was last looked along RECENT_MS ago ...
        assert!(log.looked(deg(-35.0), half, 1_000 + RECENT_MS));
        assert!(!log.looked(deg(-35.0), half, 1_001 + RECENT_MS));
        // ... east, logged last, a second later.
        assert!(log.looked(deg(120.0), half, 2_000 + RECENT_MS));
        // Ticks closer than the log's step add nothing.
        log.record(deg(180.0), 2_050);
        assert!(!log.looked(deg(180.0), half, 2_050));
    }

    #[test]
    fn a_walker_is_missed_by_stepping_off_their_path() {
        // A walker 4 m north walking south at 1.4 m/s: standing, they pass
        // within 1.2 m in the horizon; a step east at 1.5 m/s keeps 2.9 m.
        let (r, w) = ([4.0, 0.0], [-1.4, 0.0]);
        let still = miss_m(r, w, [0.0, 0.0]);
        let side = miss_m(r, w, [0.0, 1.5]);
        assert!((still - 1.2).abs() < 1e-4, "{still}");
        assert!(side > 2.9 && side < 3.0, "{side}");
        // Walking away from the vehicle, nearest at the lead.
        assert!((miss_m(r, [1.4, 0.0], [0.0, 0.0]) - 4.7).abs() < 1e-4);
        // A standing person backed away from, the faster the clearer.
        let slow = miss_m([1.0, 0.0], [0.0; 2], [-0.5, 0.0]);
        assert!(miss_m([1.0, 0.0], [0.0; 2], [-1.5, 0.0]) > slow && slow > 1.0);
    }
}
