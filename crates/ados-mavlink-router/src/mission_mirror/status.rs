//! Mission state, computed once on the agent from the frames the router sees,
//! so no surface ever infers it from a flight-mode name.
//!
//! Inputs: the vehicle HEARTBEAT (armed, numeric `custom_mode`),
//! `MISSION_CURRENT` with its extension fields, `MISSION_ITEM_REACHED`, a
//! client's `COMMAND_LONG` `DO_PAUSE_CONTINUE` (193) with the FC's
//! `COMMAND_ACK`, and upload transactions (a client `MISSION_COUNT` answered by
//! the FC's accepted `MISSION_ACK`). The result is one of
//! `none|loaded|active|paused|complete|returning`, first match wins:
//!
//! 1. `none`: `S` = NO_MISSION, `total` 0 or UINT16_MAX, or the mirror is empty.
//! 2. `complete`: `S` = COMPLETE, or (fallback) latched and the last item was
//!    reached; held until disarm or a new plan.
//! 3. `returning`: latched and the mode is an RTL/SMART_RTL/LAND mode.
//! 4. `active`: armed, in AUTO, not paused by 193, and (`S` = ACTIVE, or `S`
//!    absent and `seq` ≥ 1).
//! 5. `paused`: armed and (`S` = PAUSED, or in AUTO and paused by 193, or
//!    latched and in a hold mode).
//! 6. `loaded`: anything else with a mission present.
//!
//! `S` is `mission_state`, used only while `MISSION_CURRENT` is under 3 s old
//! and the field is non-zero; ArduPilot before 4.5 never fills it, and then the
//! fallback rules apply (`source: "fallback"`).

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::wire::MissionCurrent;

/// How long a `MISSION_CURRENT` counts as current.
const CURRENT_FRESH: Duration = Duration::from_secs(3);
/// How long a client's 193 waits for its ACK.
const PAUSE_ACK_WINDOW: Duration = Duration::from_secs(3);
/// How long a client upload waits for its closing ACK.
const UPLOAD_WINDOW: Duration = Duration::from_secs(30);

const S_NO_MISSION: u8 = 1;
const S_ACTIVE: u8 = 3;
const S_PAUSED: u8 = 4;
const S_COMPLETE: u8 = 5;

/// Which mode-number table the vehicle uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeTable {
    Copter,
    Plane,
    Px4,
    /// No mission mode table (rover, unknown): never in AUTO.
    Other,
}

impl ModeTable {
    pub(crate) fn in_auto(self, cm: u32) -> bool {
        match self {
            ModeTable::Copter => cm == 3 || cm == 27,
            ModeTable::Plane => cm == 10,
            ModeTable::Px4 => px4(cm) == (4, 4),
            ModeTable::Other => false,
        }
    }

    fn hold(self, cm: u32) -> bool {
        match self {
            ModeTable::Copter => matches!(cm, 5 | 16 | 17),
            ModeTable::Plane => matches!(cm, 12 | 19),
            ModeTable::Px4 => px4(cm) == (4, 3),
            ModeTable::Other => false,
        }
    }

    fn returning(self, cm: u32) -> bool {
        match self {
            ModeTable::Copter => matches!(cm, 6 | 21 | 9),
            ModeTable::Plane => matches!(cm, 11 | 21 | 20),
            // AUTO.RTL and AUTO.LAND.
            ModeTable::Px4 => matches!(px4(cm), (4, 5) | (4, 6)),
            ModeTable::Other => false,
        }
    }

    /// ArduPilot counts the home slot in the wire mission (seq 0) and
    /// excludes it from `MISSION_CURRENT.total`; PX4 has no home slot.
    pub fn home_slot(self) -> bool {
        !matches!(self, ModeTable::Px4)
    }
}

/// PX4 `(main, sub)` mode from `custom_mode`.
fn px4(cm: u32) -> (u32, u32) {
    ((cm >> 16) & 0xff, (cm >> 24) & 0xff)
}

/// The mission state as published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionState {
    None,
    Loaded,
    Active,
    Paused,
    Complete,
    Returning,
}

impl MissionState {
    pub fn as_str(self) -> &'static str {
        match self {
            MissionState::None => "none",
            MissionState::Loaded => "loaded",
            MissionState::Active => "active",
            MissionState::Paused => "paused",
            MissionState::Complete => "complete",
            MissionState::Returning => "returning",
        }
    }
}

/// The vehicle facts the rules read, sampled from the HEARTBEAT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vehicle {
    pub armed: bool,
    pub custom_mode: u32,
    pub table: ModeTable,
}

/// One computed mission status.
#[derive(Debug, Clone, PartialEq)]
pub struct MissionStatus {
    pub state: MissionState,
    pub seq: Option<u16>,
    pub total: Option<u16>,
    pub item_reached: Option<u16>,
    pub id: Option<u32>,
    pub from_mission_state: bool,
    pub age_ms: Option<u64>,
    pub landing_seq: bool,
}

impl MissionStatus {
    /// The mission-state wire object.
    pub fn to_json(&self) -> Value {
        json!({
            "state": self.state.as_str(),
            "seq": self.seq,
            "total": self.total,
            "item_reached": self.item_reached,
            "id": self.id,
            "source": if self.from_mission_state { "mission_state" } else { "fallback" },
            "age_ms": self.age_ms,
            "landing_seq": self.landing_seq,
        })
    }
}

/// The tracker: the latched facts the rules need between frames.
#[derive(Debug, Default)]
pub struct MissionTracker {
    current: Option<(MissionCurrent, Instant)>,
    item_reached: Option<u16>,
    ap_pause: bool,
    pending_pause: Option<(bool, Instant)>,
    latched: bool,
    complete: bool,
    upload_pending: Option<Instant>,
    uploads_seen: u32,
    last_vehicle: Option<Vehicle>,
}

impl MissionTracker {
    /// How many client uploads the FC accepted since the router started.
    pub fn uploads_seen(&self) -> u32 {
        self.uploads_seen
    }

    /// Forget the plan-scoped latches (a new plan reached the FC).
    fn new_plan(&mut self) {
        self.latched = false;
        self.complete = false;
        self.item_reached = None;
    }

    pub fn on_mission_current(&mut self, mc: MissionCurrent, now: Instant) {
        if let Some((prev, _)) = self.current {
            if prev.seq != mc.seq {
                self.ap_pause = false;
            }
            let id_changed = mc.id != 0 && prev.id != 0 && mc.id != prev.id;
            if prev.total != mc.total || id_changed {
                self.new_plan();
            }
        }
        if mc.state == S_NO_MISSION {
            self.new_plan();
        }
        self.current = Some((mc, now));
    }

    pub fn on_item_reached(&mut self, seq: u16) {
        self.item_reached = Some(seq);
    }

    /// A client sent `DO_PAUSE_CONTINUE` with `param1` (0 pause, 1 continue).
    pub fn on_client_pause_command(&mut self, param1: f32, now: Instant) {
        self.pending_pause = Some((param1 < 0.5, now));
    }

    /// The FC acknowledged a 193.
    pub fn on_pause_ack(&mut self, accepted: bool, now: Instant) {
        if let Some((pause, at)) = self.pending_pause.take() {
            if accepted && now.saturating_duration_since(at) <= PAUSE_ACK_WINDOW {
                self.ap_pause = pause;
            }
        }
    }

    /// A client began a main-mission upload (`MISSION_COUNT` toward the FC).
    pub fn on_client_upload_start(&mut self, now: Instant) {
        self.upload_pending = Some(now);
    }

    /// The FC sent a main-mission `MISSION_ACK`. Returns whether it closed a
    /// client upload the FC accepted.
    pub fn on_fc_mission_ack(&mut self, accepted: bool, now: Instant) -> bool {
        let Some(at) = self.upload_pending.take() else {
            return false;
        };
        if !accepted || now.saturating_duration_since(at) > UPLOAD_WINDOW {
            return false;
        }
        self.uploads_seen = self.uploads_seen.wrapping_add(1);
        self.new_plan();
        true
    }

    /// Apply the rules. `mirror_items` is the mirrored wire item count (with
    /// ArduPilot's home slot), `None` before any mirror exists.
    pub fn compute(&mut self, v: Vehicle, mirror_items: Option<usize>, now: Instant) -> MissionStatus {
        if let Some(prev) = self.last_vehicle {
            if prev.armed && !v.armed {
                self.latched = false;
                self.complete = false;
                self.ap_pause = false;
                self.item_reached = None;
            }
            if prev.custom_mode != v.custom_mode {
                self.ap_pause = false;
            }
        }
        self.last_vehicle = Some(v);
        if !v.armed {
            self.ap_pause = false;
        }

        let fresh = self
            .current
            .filter(|(_, at)| now.saturating_duration_since(*at) < CURRENT_FRESH);
        let mc = fresh.map(|(mc, _)| mc);
        let s = mc.map_or(0, |m| m.state);
        let total = mc.map(|m| m.total);
        let seq = mc.map(|m| m.seq);
        let cm = v.custom_mode;

        // The FC fills `total` only alongside `mission_state` (ArduPilot 4.5+,
        // PX4); without `S` a zero `total` is "not reported", not "empty".
        let mirror_empty = mirror_items.is_some_and(|n| n <= usize::from(v.table.home_slot()));
        let none = s == S_NO_MISSION
            || total == Some(u16::MAX)
            || (s != 0 && total == Some(0))
            || mirror_empty
            || (s == 0 && mirror_items.is_none() && total.unwrap_or(0) == 0);

        // The last mission seq: ArduPilot's `total` is the last seq (home is
        // seq 0, excluded from the count); PX4's last seq is `total - 1`.
        let last_seq = match total.filter(|t| *t != 0 && *t != u16::MAX) {
            Some(t) if v.table.home_slot() => Some(t),
            Some(t) => Some(t - 1),
            None => mirror_items
                .filter(|n| *n > 0)
                .map(|n| (n - 1) as u16),
        };

        let in_auto = v.table.in_auto(cm);
        let state = if none {
            self.new_plan();
            MissionState::None
        } else if s == S_COMPLETE
            || self.complete
            || (self.latched && last_seq.is_some() && self.item_reached == last_seq)
        {
            self.complete = true;
            MissionState::Complete
        } else if self.latched && v.table.returning(cm) {
            MissionState::Returning
        } else if v.armed
            && in_auto
            && !self.ap_pause
            && (s == S_ACTIVE || (s == 0 && seq.unwrap_or(0) >= 1))
        {
            self.latched = true;
            MissionState::Active
        } else if v.armed
            && (s == S_PAUSED || (in_auto && self.ap_pause) || (self.latched && v.table.hold(cm)))
        {
            MissionState::Paused
        } else {
            MissionState::Loaded
        };

        MissionStatus {
            state,
            seq,
            total,
            item_reached: self.item_reached,
            id: mc.map(|m| m.id).filter(|id| *id != 0),
            from_mission_state: s != 0,
            age_ms: fresh.map(|(_, at)| now.saturating_duration_since(at).as_millis() as u64),
            landing_seq: v.table == ModeTable::Copter && cm == 27,
        }
    }
}
