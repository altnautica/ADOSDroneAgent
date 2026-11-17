//! What the pilot commanded on its latest control tick of a skill goal, a
//! goto or the operator's sticks, and the shadow-mode hook: a planner run
//! beside the one that flies hands its commands to [`Pilot::set_shadow`],
//! and they are published (`command` and `shadow` in the pilot frame) but
//! never flown.

use ados_protocol::pilot::{
    wire, PilotCommand, ProfileLimits, ShadowCommand, StickCommand, COMMAND_LIMITS,
};

use super::Pilot;
use crate::geo::V2;
use crate::model::{StickAsk, StickLimits};

/// Bits of [`Tick::limits`], one per [`COMMAND_LIMITS`] code at the same
/// index.
pub mod limit {
    pub const SPEED: u16 = 1 << 0;
    pub const NAV_CAP: u16 = 1 << 1;
    pub const SHAPER: u16 = 1 << 2;
    pub const HARD_STOP: u16 = 1 << 3;
    pub const OBSTACLE: u16 = 1 << 4;
    pub const OBSTACLE_CLOSING: u16 = 1 << 5;
    pub const SEEN_SPACE: u16 = 1 << 6;
    pub const CLEARANCE: u16 = 1 << 7;
    pub const PUSH: u16 = 1 << 8;
    pub const SUBJECT: u16 = 1 << 9;
    pub const GEOFENCE: u16 = 1 << 10;
    pub const HOLD: u16 = 1 << 11;
}

/// A shadow command older than this is no longer published.
pub const SHADOW_MAX_AGE_MS: u64 = 1000;

/// What a goal asks on a control tick, before any limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ask {
    /// The point the goal aims at (its own point or the detour waypoint);
    /// `None` with no goal.
    pub target_ne: Option<V2>,
    /// The goal's feed-forward plus the FC's position-loop pull toward
    /// `target_ne`.
    pub vel: V2,
    /// The goal's speed limit; 0 with no goal.
    pub v_cap: f32,
}

impl Ask {
    /// Nothing to aim at.
    pub const NONE: Self = Self {
        target_ne: None,
        vel: [0.0; 2],
        v_cap: 0.0,
    };
}

/// One control tick of a skill goal or a goto, as flown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Tick {
    pub at_ms: u64,
    /// What the goal asked, kept while the pilot holds.
    pub ask: Ask,
    /// The horizontal velocity the setpoint asks of the FC (its velocity
    /// plus the position-loop pull toward its position).
    pub cmd: V2,
    /// The setpoint's velocity field.
    pub sp_vel: [f32; 3],
    /// The setpoint's horizontal acceleration field.
    pub sp_acc: V2,
    /// [`limit`] bits.
    pub limits: u16,
    /// The [`limit`] bit of the speed limit that bound this tick
    /// ([`limit::SPEED`] or [`limit::NAV_CAP`]), 0 when none did; it is
    /// published first.
    pub bound_by: u16,
}

impl Tick {
    /// A hold at `at_ms` of a goal that asked `ask`: `cmd`, `sp_vel` and
    /// `sp_acc` as sent, with [`limit::HOLD`] and `extra`.
    pub fn hold(at_ms: u64, ask: Ask, cmd: V2, sp_vel: [f32; 3], sp_acc: V2, extra: u16) -> Self {
        Self {
            at_ms,
            ask,
            cmd,
            sp_vel,
            sp_acc,
            limits: limit::HOLD | extra,
            bound_by: 0,
        }
    }
}

/// One stick tick, as flown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StickTick {
    pub at_ms: u64,
    /// What the sticks asked, before shaping.
    pub ask: StickAsk,
    /// The reference sent: horizontal velocity and acceleration, down
    /// velocity, yaw rate (rad/s).
    pub v: V2,
    pub a: V2,
    pub vd: f32,
    pub yaw_rate: f32,
    /// The horizontal velocity the setpoint asks of the FC (its velocity
    /// plus the position-loop pull toward its position).
    pub cmd: V2,
    /// The reference brakes to a hold.
    pub braking: bool,
    /// The stick frame flown: its age and `seq`.
    pub input_age_ms: u32,
    pub input_seq: u32,
    /// [`limit`] bits.
    pub limits: u16,
    /// The limits flown.
    pub lim: StickLimits,
}

impl StickTick {
    /// The published stick command.
    pub fn wire(&self) -> StickCommand {
        StickCommand {
            ask_ne_mps: self.ask.ne,
            ask_vz_mps: -self.ask.vd,
            ask_yaw_dps: self.ask.yaw_rate.to_degrees(),
            v_ne_mps: self.v,
            a_ne_mps2: self.a,
            vz_mps: -self.vd,
            yaw_rate_dps: self.yaw_rate.to_degrees(),
            braking: self.braking,
            input_age_ms: self.input_age_ms,
            input_seq: self.input_seq,
        }
    }
}

/// The limits and hold reasons of every goal tick since the pilot frame was
/// last published, so a cut shorter than the frame period reaches the log.
#[derive(Debug, Default)]
pub(crate) struct Latch {
    limits: u16,
    holds: Vec<String>,
}

/// A command of a planner run in shadow mode, on the pilot's clock and in
/// the FC's local NED. The pilot logs it and never flies it.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowCmd {
    /// The pilot clock (the `now_ms` of [`Pilot::tick`]) it was made at.
    pub at_ms: u64,
    pub pos_ne: V2,
    pub vel_ne: V2,
    pub acc_ne: V2,
    /// The planned path from here, at most 16 points (more are dropped).
    pub path_ne: Vec<V2>,
    /// It flies a committed fallback rather than its exploratory plan.
    pub committed: bool,
    pub reason: Option<&'static str>,
}

/// Most path points a shadow command publishes.
const SHADOW_PATH_MAX: usize = 16;

fn ne(v: V2) -> [f64; 2] {
    [wire(v[0]), wire(v[1])]
}

impl Pilot {
    /// Log the command of a planner running in shadow mode as `source`
    /// (`None` clears it). It is published with the pilot frame while it is
    /// under [`SHADOW_MAX_AGE_MS`] old and is never sent to the FC.
    pub fn set_shadow(&mut self, source: &'static str, cmd: Option<ShadowCmd>) {
        self.shadow = cmd.map(|c| (source, c));
    }

    /// The skill's own goal (or the goto target), before any detour.
    pub(super) fn commanded_goal(&self) -> Option<[f32; 3]> {
        self.auth
            .lease()
            .and_then(|l| l.goal.as_ref())
            .map(|g| [g.n, g.e, g.d])
            .or(self.goto)
    }

    /// Fold this tick's limits and hold reason into the latch, at the end of
    /// every control tick that flew a goal or the sticks.
    pub(super) fn latch_tick(&mut self) {
        let bits = match (self.command, self.stick) {
            (Some(t), _) => t.limits,
            (None, Some(s)) => s.limits,
            (None, None) => return,
        };
        self.latch.limits |= bits;
        if let Some(r) = &self.hold_reason {
            if !self.latch.holds.contains(r) {
                self.latch.holds.push(r.clone());
            }
        }
    }

    /// The pilot frame built by [`Pilot::state`] was published: the next
    /// frame's `command` latches the ticks from here.
    pub fn state_published(&mut self) {
        self.latch.limits = 0;
        self.latch.holds.clear();
    }

    /// The published `command`: the latest tick of a goal or of the sticks,
    /// if this tick flew one, with the limits and hold reasons latched since
    /// the last published frame.
    pub(super) fn command_frame(&self) -> Option<PilotCommand> {
        let (bits, first, mut c) = match (self.command, self.stick) {
            (Some(t), _) => {
                let mut c = PilotCommand::new(t.at_ms + self.epoch_offset_ms);
                c.goal_ned = self.commanded_goal().map(|g| g.map(wire));
                c.target_ne = t.ask.target_ne.map(ne);
                c.ask_vel_ne = ne(t.ask.vel);
                c.cmd_vel_ne = ne(t.cmd);
                c.sp_vel_ned = t.sp_vel.map(wire);
                c.sp_acc_ne = ne(t.sp_acc);
                c.v_cap_mps = wire(t.ask.v_cap);
                let bits = t.limits | self.latch.limits;
                (bits, bits & t.bound_by, c)
            }
            (None, Some(s)) => {
                let mut c = PilotCommand::new(s.at_ms + self.epoch_offset_ms);
                c.ask_vel_ne = ne(s.ask.ne);
                c.cmd_vel_ne = ne(s.cmd);
                c.sp_vel_ned = [s.v[0], s.v[1], s.vd].map(wire);
                c.sp_acc_ne = ne(s.a);
                c.v_cap_mps = wire(s.lim.v_h);
                c.stick = Some(s.wire());
                c.profile_limits = Some(self.profile_limits_of(&s.lim));
                (s.limits | self.latch.limits, 0, c)
            }
            (None, None) => return None,
        };
        c.limits = COMMAND_LIMITS
            .iter()
            .enumerate()
            .filter(|(i, _)| first & (1 << i) != 0)
            .chain(
                COMMAND_LIMITS
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| bits & !first & (1 << i) != 0),
            )
            .map(|(_, n)| (*n).to_owned())
            .collect();
        c.hold_reasons = self.latch.holds.clone();
        if let Some(r) = &self.hold_reason {
            if !c.hold_reasons.contains(r) {
                c.hold_reasons.push(r.clone());
            }
        }
        Some(c)
    }

    /// The published form of the stick limits `lim` of the profile in use.
    pub(super) fn profile_limits_of(&self, lim: &StickLimits) -> ProfileLimits {
        ProfileLimits {
            profile: self.settings.pilot.profile,
            v_h_mps: lim.v_h,
            v_up_mps: lim.v_up,
            v_down_mps: lim.v_down,
            yaw_dps: lim.yaw_dps,
            a_acc_mps2: lim.a_acc,
            a_brk_mps2: lim.a_brk,
            jerk_mps3: lim.jerk,
            a_v_mps2: lim.a_v,
            yaw_dps2: lim.yaw_dps2,
            expo_h: lim.expo_h,
            expo_v: lim.expo_v,
            expo_yaw: lim.expo_yaw,
            stop_m: ProfileLimits::stop_distance_m(lim.v_h, lim.a_brk, lim.jerk),
            margin_m: self.settings.pilot.effective_margin_m(),
        }
    }

    /// The published `shadow`: the shadow planner's latest command while
    /// it is fresh.
    pub(super) fn shadow_frame(&self, now_ms: u64) -> Option<ShadowCommand> {
        let (source, s) = self.shadow.as_ref()?;
        if now_ms.saturating_sub(s.at_ms) > SHADOW_MAX_AGE_MS {
            return None;
        }
        let mut c = ShadowCommand::new(*source, s.at_ms + self.epoch_offset_ms);
        c.pos_ne = ne(s.pos_ne);
        c.vel_ne = ne(s.vel_ne);
        c.acc_ne = ne(s.acc_ne);
        c.path_ne = s
            .path_ne
            .iter()
            .take(SHADOW_PATH_MAX)
            .map(|p| ne(*p))
            .collect();
        c.committed = s.committed;
        c.reason = s.reason.map(str::to_owned);
        c.live = self.command.is_some();
        Some(c)
    }
}
