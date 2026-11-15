//! The no-progress rule: an FC mission that comes less than
//! `pilot.stall_min_progress_m` closer to its active waypoint in
//! `pilot.stall_window_s`, or a return that moves less than that, holds the
//! vehicle and asks the operator. A mission waypoint is skipped after
//! `pilot.stall_skip_after_s` with no answer and recorded as a coverage gap;
//! after `pilot.stall_max_auto_skips` automatic skips in a row the vehicle
//! holds until the operator answers. A blocked return lands in place after
//! `pilot.return_blocked_land_after_s`.

use std::collections::VecDeque;

use ados_protocol::mavlink::ardupilotmega::{MavMessage, MISSION_SET_CURRENT_DATA};
use ados_protocol::pilot::{MissionGap, Owner};
use serde_json::json;

use super::super::Pilot;
use super::supervisor::{wp_arg, Resume};
use crate::flows::banner;

/// `hold_reason` while a stalled mission holds.
pub const HOLD_MISSION_STALLED: &str = "mission-stalled";
/// `hold_reason` while a blocked return holds.
pub const HOLD_RETURN_BLOCKED: &str = "return-blocked";
/// Progress samples are kept this often.
const SAMPLE_MS: u64 = 100;
/// MAV_CMDs whose own checks cover them: takeoff and landing.
const EXEMPT_CMDS: [u16; 4] = [21, 22, 84, 85];
/// Loiter MAV_CMDs: no progress is expected inside their radius.
const LOITER_CMDS: [u16; 4] = [17, 18, 19, 31];
/// A loiter item without a radius counts as reached within this.
const LOITER_REACHED_M: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mission,
    Return,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mission => "mission",
            Self::Return => "return",
        }
    }
}

/// A stall the operator is asked about.
#[derive(Debug, Clone)]
pub struct Active {
    pub kind: Kind,
    pub seq: Option<u32>,
    pub since_ms: u64,
    /// The automatic skip (mission) or landing (return); `None` holds for
    /// the operator.
    pub deadline_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    at_ms: u64,
    /// Distance to the waypoint (a mission), m.
    dist: f32,
    pos: [f32; 3],
}

#[derive(Debug, Default)]
pub struct Stall {
    /// What progress is measured on, and since when.
    watch: Option<(Kind, Option<u32>, u64)>,
    samples: VecDeque<Sample>,
    pub active: Option<Active>,
    /// Automatic skips in a row.
    auto_skips: u32,
    /// The item a skip went to: reaching another one is progress again.
    skip_to: Option<u32>,
    /// Skipped items of this mission.
    pub gaps: Vec<MissionGap>,
    /// The mission the gaps belong to (`id`, `total`).
    gaps_of: Option<(Option<u32>, Option<u32>)>,
}

impl Stall {
    /// Forget the watch and any stall (the gaps and skip count stay).
    pub fn clear(&mut self) {
        self.watch = None;
        self.samples.clear();
        self.active = None;
    }

    /// A different mission on the FC starts with no gaps.
    pub fn on_mission(&mut self, m: Option<&ados_protocol::pilot::Mission>) {
        let Some(m) = m else { return };
        if m.total.is_none() {
            return;
        }
        let key = (m.id, m.total);
        if self.gaps_of.is_some_and(|k| k != key) {
            self.gaps.clear();
            self.auto_skips = 0;
        }
        self.gaps_of = Some(key);
    }
}

fn dist3(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

impl Pilot {
    /// Measure progress on the watched flight and raise, count down and act
    /// on a stall.
    pub(super) fn watch_progress(&mut self, watch: Resume, now_ms: u64) {
        let Some(l) = self.fc.fresh_local(now_ms) else {
            return;
        };
        let pos = [l.n, l.e, l.d];
        let (kind, seq, dist) = match watch {
            Resume::Mission => {
                let seq = self.mission.as_ref().and_then(|m| m.seq);
                (Kind::Mission, seq, self.waypoint_distance(pos))
            }
            Resume::Return => (Kind::Return, None, Some(0.0)),
        };
        if let Some(a) = &self.sup.stall.active {
            if a.kind != kind || a.seq != seq {
                // The FC moved on by itself: the stall is over.
                self.sup.stall.active = None;
            }
        }
        let Some(dist) = dist else {
            // An item without a position, a takeoff or landing, or a loiter
            // inside its radius: no progress is expected.
            if self.sup.stall.active.is_none() {
                self.sup.stall.watch = None;
                self.sup.stall.samples.clear();
            }
            return;
        };
        let st = &mut self.sup.stall;
        if st.watch.is_none_or(|(k, s, _)| k != kind || s != seq) {
            // Reaching an item past the one a skip went to is progress: the
            // automatic skips are no longer in a row.
            if let (Some(s), Some(to)) = (seq, st.skip_to) {
                if kind == Kind::Mission && s > to {
                    st.auto_skips = 0;
                    st.skip_to = None;
                }
            }
            st.watch = Some((kind, seq, now_ms));
            st.samples.clear();
        }
        if st
            .samples
            .back()
            .is_none_or(|s| now_ms.saturating_sub(s.at_ms) >= SAMPLE_MS)
        {
            st.samples.push_back(Sample {
                at_ms: now_ms,
                dist,
                pos,
            });
        }
        let window_ms = u64::from(self.settings.pilot.stall_window_s) * 1000;
        while st
            .samples
            .get(1)
            .is_some_and(|s| now_ms.saturating_sub(s.at_ms) >= window_ms)
        {
            st.samples.pop_front();
        }
        if let Some(a) = st.active.clone() {
            if a.deadline_ms.is_some_and(|d| now_ms >= d) {
                match a.kind {
                    Kind::Mission => {
                        let _ = self.mission_skip(false, now_ms);
                    }
                    Kind::Return => {
                        tracing::warn!(target: "ados_pilot", reason = "return_blocked", "pilot_return_land");
                        self.land_in_place(now_ms);
                    }
                }
            }
            return;
        }
        let Some(&(_, _, since)) = st.watch.as_ref() else {
            return;
        };
        let Some(old) = st.samples.front().copied() else {
            return;
        };
        if now_ms.saturating_sub(since) < window_ms || now_ms.saturating_sub(old.at_ms) < window_ms
        {
            return;
        }
        let progress_m = match kind {
            Kind::Mission => old.dist - dist,
            Kind::Return => dist3(old.pos, pos),
        };
        if progress_m >= self.settings.pilot.stall_min_progress_m {
            return;
        }
        self.raise_stall(kind, seq, dist, progress_m, now_ms);
    }

    /// The 3-D distance to the active waypoint, or `None` while the item is
    /// exempt from the rule.
    fn waypoint_distance(&self, pos: [f32; 3]) -> Option<f32> {
        let m = self.mission.as_ref()?;
        let t = m.target.as_ref().filter(|t| Some(t.seq) == m.seq)?;
        if EXEMPT_CMDS.contains(&t.cmd) {
            return None;
        }
        let ned = t.ned?;
        let target = [ned[0] as f32, ned[1] as f32, ned[2] as f32];
        let d = dist3(target, pos);
        if LOITER_CMDS.contains(&t.cmd) {
            let r = t.radius_m.map_or(LOITER_REACHED_M, |r| r as f32);
            let horizontal = (target[0] - pos[0]).hypot(target[1] - pos[1]);
            if horizontal <= r {
                return None;
            }
        }
        Some(d)
    }

    fn raise_stall(&mut self, kind: Kind, seq: Option<u32>, d_m: f32, progress_m: f32, now_ms: u64) {
        let cfg = &self.settings.pilot;
        let window_s = cfg.stall_window_s;
        let deadline_ms = match kind {
            Kind::Mission if self.sup.stall.auto_skips >= cfg.stall_max_auto_skips => None,
            Kind::Mission => Some(now_ms + u64::from(cfg.stall_skip_after_s) * 1000),
            Kind::Return => Some(now_ms + u64::from(cfg.return_blocked_land_after_s) * 1000),
        };
        let (resume, reason) = match kind {
            Kind::Mission => (Resume::Mission, HOLD_MISSION_STALLED),
            Kind::Return => (Resume::Return, HOLD_RETURN_BLOCKED),
        };
        self.supervise_hold(resume, reason, now_ms);
        self.sup.stall.active = Some(Active {
            kind,
            seq,
            since_ms: now_ms,
            deadline_ms,
        });
        let in_s = deadline_ms.map(|d| d.saturating_sub(now_ms) / 1000);
        self.banner = Some(match (kind, in_s) {
            (Kind::Mission, Some(in_s)) => banner(
                "pilot.banner.mission_stalled",
                &[("wp", wp_arg(seq)), ("in_s", json!(in_s))],
                now_ms,
            ),
            (Kind::Mission, None) => banner(
                "pilot.banner.mission_stalled_hold",
                &[("wp", wp_arg(seq))],
                now_ms,
            ),
            (Kind::Return, in_s) => banner(
                "pilot.banner.return_blocked",
                &[("in_s", json!(in_s.unwrap_or(0)))],
                now_ms,
            ),
        });
        tracing::warn!(
            target: "ados_pilot",
            kind = kind.as_str(),
            seq,
            d_m,
            progress_m,
            window_s,
            "pilot_mission_stall"
        );
    }

    /// Skip the stalled waypoint: the FC's current item becomes the next one
    /// and AUTO carries on, with the skipped item recorded as a coverage gap.
    /// The last item has no next one: the vehicle returns.
    pub(in crate::core) fn mission_skip(&mut self, by_operator: bool, now_ms: u64) -> Result<(), String> {
        if !self.fc.armed {
            return Err("disarmed".into());
        }
        let Some(seq) = self
            .sup
            .stall
            .active
            .as_ref()
            .filter(|a| a.kind == Kind::Mission)
            .and_then(|a| a.seq)
        else {
            return Err("no_stall".into());
        };
        let total = self.mission.as_ref().and_then(|m| m.total);
        let by = if by_operator { "operator" } else { "timeout" };
        let st = &mut self.sup.stall;
        st.gaps.push(MissionGap {
            seq,
            reason: format!("stall_{by}"),
            at_ms: now_ms + self.epoch_offset_ms,
        });
        st.auto_skips = if by_operator { 0 } else { st.auto_skips + 1 };
        self.end_supervision();
        tracing::info!(target: "ados_pilot", seq, by, "pilot_mission_skip");
        let next = seq + 1;
        self.banner = Some(banner(
            "pilot.banner.mission_wp_skipped",
            &[("wp", wp_arg(Some(seq)))],
            now_ms,
        ));
        if total.is_some_and(|t| next >= t) {
            self.revoke("rth");
            self.command_return("mission_end", now_ms);
            self.set_owner(Owner::FcMode, "rth");
            return Ok(());
        }
        let tx = self.tx();
        self.mav(MavMessage::MISSION_SET_CURRENT(MISSION_SET_CURRENT_DATA {
            seq: u16::try_from(next).unwrap_or(u16::MAX),
            target_system: tx.sysid,
            target_component: tx.compid,
        }));
        self.sup.stall.skip_to = Some(next);
        let auto = self.family().auto();
        self.set_mode(auto, now_ms);
        self.set_owner(Owner::MissionFc, "mission_skip");
        self.mission_paused = false;
        Ok(())
    }
}
