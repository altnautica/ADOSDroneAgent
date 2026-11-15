//! The route as the FC flies it: the router's mirrored navigation items in
//! the FC's local NED frame, cut into legs, and each leg's verdict against
//! the occupancy map.

use ados_protocol::pilot::RouteState;
use serde_json::Value;

use crate::geo::V2;
use crate::obstacles::Obstacles;

/// MAV_CMD_NAV_TAKEOFF.
const CMD_TAKEOFF: u16 = 22;
/// Seen obstacles this far beyond the required clearance still name the
/// tightest point.
const REPORT_EXTRA_M: f32 = 0.5;

/// One mirrored navigation item.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteItem {
    pub seq: u32,
    pub cmd: u16,
    pub ned: Option<[f32; 3]>,
    /// Latitude and longitude 0: ArduPilot flies the item where the vehicle
    /// is (a land or waypoint "here").
    pub here: bool,
}

/// The mission feed's route (`route`, `items_truncated`, `speeds` of a
/// [`crate::router_state::mission_of`] payload).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteFeed {
    pub items: Vec<RouteItem>,
    /// The mission has more navigation items than the feed carries.
    pub truncated: bool,
    /// The mission's horizontal DO_CHANGE_SPEED items: `(seq, m/s)`.
    pub speeds: Vec<(u32, f32)>,
}

/// The route of a mission feed payload.
pub fn route_of(payload: &Value) -> RouteFeed {
    let list = |k: &str| {
        payload
            .get(k)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    let seq = |v: &Value| u32::try_from(v.get("seq")?.as_u64()?).ok();
    let items = list("route")
        .filter_map(|i| {
            let cmd = u16::try_from(i.get("cmd")?.as_u64()?).ok()?;
            let ned = i.get("ned").and_then(Value::as_array).and_then(|a| {
                let v: Vec<f32> = a
                    .iter()
                    .filter_map(|x| x.as_f64().map(|x| x as f32))
                    .collect();
                (v.len() == 3).then(|| [v[0], v[1], v[2]])
            });
            Some(RouteItem {
                seq: seq(i)?,
                cmd,
                ned,
                here: i.get("here").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    let speeds = list("speeds")
        .filter_map(|s| Some((seq(s)?, s.get("mps")?.as_f64()? as f32)))
        .collect();
    RouteFeed {
        items,
        truncated: payload.get("items_truncated").and_then(Value::as_bool) == Some(true),
        speeds,
    }
}

/// A leg to check: `(from_seq, from, to_seq, to)`, local NED.
pub type Leg = (u32, [f32; 3], u32, [f32; 3]);

/// Where the FC starts an item from when it is not the previous item: the
/// vehicle's position when the pilot put the FC in AUTO at that item (after
/// a skip or a hold, ArduPilot flies the item from where it stands).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Origin {
    /// The leg's label start: the last item flown before `to_seq`.
    pub from_seq: u32,
    /// The item the FC was set to; the leg ends at the first placed item
    /// from it on.
    pub to_seq: u32,
    pub ned: [f32; 3],
}

/// The legs ahead and the items they cannot place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Legs {
    pub legs: Vec<Leg>,
    /// Items with no position (not "here", not a leading take-off): the
    /// legs to and from them are not checked.
    pub unchecked: Vec<u32>,
}

/// The legs the FC flies from item `from` on (all, with none), skipping the
/// `skipped` items. A take-off without a position starts at `home` at the
/// next item's height; an item "here" sits where the item before it does.
/// With an `origin`, the first item at or after its `to_seq` is flown from
/// the origin and nothing before it counts.
pub fn legs_of(
    items: &[RouteItem],
    home: V2,
    skipped: &dyn Fn(u32) -> bool,
    from: Option<u32>,
    origin: Option<&Origin>,
) -> Legs {
    let mut out = Legs::default();
    let mut prev: Option<(u32, [f32; 3])> = None;
    // An item before `from` with no position after the last placed one: the
    // leg into `from` is not checked.
    let mut unplaced_before: Option<u32> = None;
    let mut takeoff: Option<u32> = None;
    let mut started = false;
    let mut origin = origin.copied();
    let from = origin.map(|o| o.to_seq).or(from);
    for it in items.iter().filter(|i| !skipped(i.seq)) {
        let pos = match (it.ned, prev) {
            (Some(p), _) => Some(p),
            (None, Some((_, p))) if it.here => Some(p),
            _ => None,
        };
        let Some(p) = pos else {
            if it.cmd == CMD_TAKEOFF && !started {
                takeoff = Some(it.seq);
            } else if from.is_some_and(|f| it.seq < f) {
                unplaced_before = Some(it.seq);
            } else {
                out.unchecked.push(it.seq);
            }
            prev = None;
            continue;
        };
        started = true;
        if let Some(o) = origin.filter(|o| it.seq >= o.to_seq) {
            // The FC flies this item from the origin, unless it first flies
            // to an item with no position on the way (already unchecked).
            // Nothing before `to_seq` was kept.
            prev = out.unchecked.is_empty().then_some((o.from_seq, o.ned));
            unplaced_before = None;
            origin = None;
        } else if let Some(s) = takeoff.take() {
            prev = Some((s, [home[0], home[1], p[2]]));
        }
        if from.is_none_or(|f| it.seq >= f) {
            match prev {
                Some((ps, pp)) => out.legs.push((ps, pp, it.seq, p)),
                None => out.unchecked.extend(unplaced_before.take()),
            }
        } else {
            unplaced_before = None;
        }
        prev = Some((it.seq, p));
    }
    out
}

/// One leg's verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegCheck {
    pub from_seq: u32,
    pub to_seq: u32,
    pub state: RouteState,
    /// The point of the leg nearest a seen obstacle: local NED and the
    /// clearance (m).
    pub pinch: Option<([f32; 3], f32)>,
}

/// Check `legs` against `obs` with `required` m of clearance. The clearance
/// is a lower bound (see [`crate::obstacles::Obstacles::probe_leg`]).
pub fn check_legs(obs: &Obstacles, legs: &[Leg], required: f32, now_ms: u64) -> Vec<LegCheck> {
    legs.iter()
        .map(|&(from_seq, a, to_seq, b)| {
            let probe = obs.probe_leg(
                [a[0], a[1]],
                [b[0], b[1]],
                required,
                required + REPORT_EXTRA_M,
                now_ms,
            );
            let pinch = probe.nearest.map(|(t, q, d)| {
                let len = (b[0] - a[0]).hypot(b[1] - a[1]);
                let f = if len > 1e-6 {
                    (t / len).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                ([q[0], q[1], a[2] + f * (b[2] - a[2])], d)
            });
            let state = if pinch.is_some_and(|(_, d)| d < required) {
                RouteState::Infeasible
            } else if probe.unseen {
                RouteState::Unverified
            } else {
                RouteState::Feasible
            };
            LegCheck {
                from_seq,
                to_seq,
                state,
                pinch,
            }
        })
        .collect()
}
