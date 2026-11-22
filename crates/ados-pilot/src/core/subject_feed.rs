//! The designated subject from the vision engine (`vision.subject.v1`,
//! read through the plugin event export as JSON): where it is, how it moves
//! and where it will be, the predictions a `follow_intent` goal's view is
//! planned over ([`crate::planner::ViewPlanner`]).
//!
//! The record's `stamp_ns` is on the vision engine's frame clock, never
//! the pilot's: a record is placed in time by when it arrived and how old
//! its fix was then.

use serde_json::Value;

use crate::geo::V2;
use crate::planner::{Prediction, SubjectView};

/// A record is used this long after it arrives, ms: its predictions reach
/// 3 s ahead.
pub const FEED_USE_MS: u64 = 3_000;

/// The subject's height when a record names none, metres.
const DEFAULT_HEIGHT_M: f32 = 1.7;

/// One subject state as it arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedSubject {
    pub id: u32,
    pub arrived_ms: u64,
    pub fix_age_ms: u32,
    /// The feet, local NED.
    pub pos_ned: [f32; 3],
    pub vel_ne: V2,
    /// The coordinated-turn mode's weight.
    pub turn_prob: f32,
    pub height_m: f32,
    /// Offsets from the record's stamp.
    pub preds: Vec<Prediction>,
}

/// A record: a state, or the subject with this id released.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    State(FeedSubject),
    Released(u32),
}

fn f32s<const N: usize>(v: Option<&Value>) -> Option<[f32; N]> {
    let a = v?.as_array()?;
    if a.len() != N {
        return None;
    }
    let mut out = [0.0f32; N];
    for (o, x) in out.iter_mut().zip(a) {
        let f = x.as_f64()? as f32;
        if !f.is_finite() {
            return None;
        }
        *o = f;
    }
    Some(out)
}

/// Read one `vision.subject.v1` payload that arrived at `now_ms`; `None`
/// when it is not one.
pub fn parse(payload: &Value, now_ms: u64) -> Option<Record> {
    let id = u32::try_from(payload.get("id")?.as_u64()?).ok()?;
    if payload.get("released").and_then(Value::as_bool) == Some(true) {
        return Some(Record::Released(id));
    }
    let pos_ned = f32s::<3>(payload.get("pos_ned"))?;
    let vel = f32s::<3>(payload.get("vel_ned")).unwrap_or([0.0; 3]);
    let modes = f32s::<3>(payload.get("mode_probs")).unwrap_or([1.0, 0.0, 0.0]);
    let fix_age_ms = payload
        .get("fix_age_ms")
        .and_then(Value::as_u64)
        .map_or(0, |a| u32::try_from(a).unwrap_or(u32::MAX));
    let height_m = payload
        .get("height_m")
        .and_then(Value::as_f64)
        .map(|h| h as f32)
        .filter(|h| h.is_finite() && *h > 0.5 && *h < 2.5)
        .unwrap_or(DEFAULT_HEIGHT_M);
    let mut preds: Vec<Prediction> = payload
        .get("predictions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let dt_s = p.get("dt_s")?.as_f64()? as f32;
                    let ne = f32s::<3>(p.get("pos_ned"))?;
                    (dt_s.is_finite() && dt_s > 0.0).then_some(Prediction {
                        dt_s,
                        ne: [ne[0], ne[1]],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    preds.sort_by(|a, b| a.dt_s.total_cmp(&b.dt_s));
    Some(Record::State(FeedSubject {
        id,
        arrived_ms: now_ms,
        fix_age_ms,
        pos_ned,
        vel_ne: [vel[0], vel[1]],
        turn_prob: modes[1],
        height_m,
        preds,
    }))
}

impl FeedSubject {
    /// The subject as it most probably is at `now_ms`, with what is still
    /// predicted ahead of it; `None` once the record is older than
    /// [`FEED_USE_MS`].
    pub fn view(&self, now_ms: u64) -> Option<SubjectView> {
        let since = now_ms.saturating_sub(self.arrived_ms);
        if since > FEED_USE_MS {
            return None;
        }
        let e = since as f32 / 1000.0;
        let whole = SubjectView {
            ne: [self.pos_ned[0], self.pos_ned[1]],
            vel: self.vel_ne,
            ground_d: Some(self.pos_ned[2]),
            height_m: self.height_m,
            preds: self.preds.clone(),
            turn_prob: self.turn_prob,
            unseen_s: self.fix_age_ms as f32 / 1000.0 + e,
        };
        Some(SubjectView {
            ne: whole.at(e),
            preds: whole
                .preds
                .iter()
                .filter(|p| p.dt_s > e)
                .map(|p| Prediction {
                    dt_s: p.dt_s - e,
                    ne: p.ne,
                })
                .collect(),
            ..whole
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> Value {
        json!({"v": 1, "camera_id": "front", "id": 7, "stamp_ns": 5, "fix_age_ms": 100,
            "pos_ned": [10.0, 0.0, 0.2], "cov": [0.1, 0, 0, 0, 0.1, 0, 0, 0, 0.1],
            "vel_ned": [1.0, 0.0, 0.0], "mode_probs": [0.7, 0.2, 0.1], "height_m": 1.8,
            "predictions": [{"dt_s": 1.0, "pos_ned": [11.0, 0.0, 0.2], "cov": []},
                            {"dt_s": 0.5, "pos_ned": [10.5, 0.0, 0.2], "cov": []}],
            "sources": ["box"]})
    }

    #[test]
    fn a_state_reads_and_moves_on_with_its_predictions() {
        let Some(Record::State(f)) = parse(&state(), 1_000) else {
            panic!("a state")
        };
        assert_eq!(f.id, 7);
        assert_eq!(f.preds[0].dt_s, 0.5, "sorted");
        assert_eq!(f.turn_prob, 0.2);
        let v = f.view(1_500).expect("fresh");
        assert!((v.ne[0] - 10.5).abs() < 1e-4, "{v:?}");
        assert_eq!(v.preds.len(), 1);
        assert!((v.preds[0].dt_s - 0.5).abs() < 1e-4);
        assert!((v.unseen_s - 0.6).abs() < 1e-4);
        assert_eq!(v.ground_d, Some(0.2));
        assert!(f.view(1_000 + FEED_USE_MS + 1).is_none());
    }

    #[test]
    fn a_release_and_junk_read_as_such() {
        let r = json!({"v": 1, "camera_id": "front", "id": 7, "stamp_ns": 5, "released": true});
        assert_eq!(parse(&r, 0), Some(Record::Released(7)));
        assert_eq!(parse(&json!({"id": 7}), 0), None);
        let mut bad = state();
        bad["pos_ned"] = json!([1.0, "x", 0.0]);
        assert_eq!(parse(&bad, 0), None);
    }
}
