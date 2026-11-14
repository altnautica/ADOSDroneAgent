//! What the pilot commanded on its latest control tick of a skill goal or a
//! goto, and the shadow-mode hook: a planner run beside the one that flies
//! hands its commands to [`Pilot::set_shadow`], and they are published
//! (`command` and `shadow` in the pilot frame) but never flown.

use ados_protocol::pilot::{wire, PilotCommand, ShadowCommand, COMMAND_LIMITS};

use super::Pilot;
use crate::geo::V2;

/// Bits of [`Tick::limits`], one per [`COMMAND_LIMITS`] code at the same
/// index.
pub mod limit {
    pub const SPEED: u16 = 1 << 0;
    pub const NAV_CAP: u16 = 1 << 1;
    pub const HARD_STOP: u16 = 1 << 2;
    pub const OBSTACLE: u16 = 1 << 3;
    pub const OBSTACLE_CLOSING: u16 = 1 << 4;
    pub const SEEN_SPACE: u16 = 1 << 5;
    pub const CLEARANCE: u16 = 1 << 6;
    pub const PUSH: u16 = 1 << 7;
    pub const GEOFENCE: u16 = 1 << 8;
    pub const HOLD: u16 = 1 << 9;
}

/// A shadow command older than this is no longer published.
pub const SHADOW_MAX_AGE_MS: u64 = 1000;

/// One control tick of a skill goal or a goto, as flown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Tick {
    pub at_ms: u64,
    /// The point flown toward; `None` while holding.
    pub target_ne: Option<V2>,
    /// The horizontal velocity asked before any limit.
    pub ask: V2,
    /// The horizontal velocity the setpoint asks of the FC (its velocity
    /// plus the position-loop pull toward its position).
    pub cmd: V2,
    /// The setpoint's velocity field.
    pub sp_vel: [f32; 3],
    /// The goal's speed limit; 0 while holding.
    pub v_cap: f32,
    /// [`limit`] bits.
    pub limits: u16,
}

impl Tick {
    /// A hold at `at_ms`: no target, nothing asked, `cmd` and `sp_vel` as
    /// sent, with [`limit::HOLD`] and `extra`.
    pub fn hold(at_ms: u64, cmd: V2, sp_vel: [f32; 3], extra: u16) -> Self {
        Self {
            at_ms,
            target_ne: None,
            ask: [0.0; 2],
            cmd,
            sp_vel,
            v_cap: 0.0,
            limits: limit::HOLD | extra,
        }
    }
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

    /// The published `command`: the latest tick of a goal, if this tick
    /// flew one.
    pub(super) fn command_frame(&self) -> Option<PilotCommand> {
        let t = self.command?;
        let mut c = PilotCommand::new(t.at_ms + self.epoch_offset_ms);
        c.goal_ned = self.commanded_goal().map(|g| g.map(wire));
        c.target_ne = t.target_ne.map(ne);
        c.ask_vel_ne = ne(t.ask);
        c.cmd_vel_ne = ne(t.cmd);
        c.sp_vel_ned = t.sp_vel.map(wire);
        c.v_cap_mps = wire(t.v_cap);
        c.limits = COMMAND_LIMITS
            .iter()
            .enumerate()
            .filter(|(i, _)| t.limits & (1 << i) != 0)
            .map(|(_, n)| (*n).to_owned())
            .collect();
        Some(c)
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
