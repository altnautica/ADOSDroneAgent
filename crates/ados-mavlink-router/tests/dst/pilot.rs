//! The pilot daemon's `state` stream as the router's pilot link receives it,
//! the aux radio its downlink leaves on, and the oracle a ground station
//! checks each downlinked state against.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use ados_mavlink_router::pilot_link::{StateEgress, DOWNLINK_MAX_BODY};
use rmpv::Value;

use crate::rng::Rng;

/// The aux radio's egress: refuses while `refuse` is set (the radio pair is
/// down), else queues the payload for the air link.
#[derive(Default)]
pub struct Radio {
    pub refuse: bool,
    pub out: Vec<Vec<u8>>,
}

pub struct SimEgress(pub Arc<StdMutex<Radio>>);

impl StateEgress for SimEgress {
    fn send_state<'a>(
        &'a self,
        payload: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let mut r = self.0.lock().unwrap();
        let res = if r.refuse {
            Err("aux link down".to_string())
        } else {
            r.out.push(payload.to_vec());
            Ok(())
        };
        Box::pin(std::future::ready(res))
    }
}

/// The fields the generated states carry that a ground surface reads: the
/// downlink keeps every one of them, however far it reduces a state.
const GROUND: [&str; 12] = [
    "op",
    "owner",
    "owner_label",
    "control_mode",
    "armed",
    "landed",
    "fc_mode",
    "input_age_ms",
    "hold_reason",
    "input_session",
    "plan_ned",
    "readiness",
];
/// Onboard diagnostics: never downlinked.
const DIAG: [&str; 3] = ["command", "shadow", "stick"];

fn s(x: &str) -> Value {
    Value::from(x)
}

/// The pilot's `n`th state: mostly small, sometimes a long plan or readiness
/// list that only fits reduced, now and then one that cannot fit at all.
pub fn state(rng: &mut Rng, n: u64) -> Value {
    let size = rng.unit();
    let plan_len = if size < 0.7 {
        rng.between(0, 10)
    } else {
        rng.between(30, 80)
    };
    let plan: Vec<Value> = (0..plan_len)
        .map(|i| {
            Value::Array(vec![
                Value::from(i as f64 * 1.5),
                Value::from(-(i as f64)),
                Value::from(-12.0),
            ])
        })
        .collect();
    let readiness: Vec<Value> = (0..rng.between(0, 8))
        .map(|i| {
            Value::Map(vec![
                (s("id"), Value::from(format!("check_{i}"))),
                (s("ok"), Value::from(rng.chance(0.6))),
                (s("severity"), s("warn")),
                (
                    s("msg"),
                    Value::from("x".repeat(rng.between(10, 60) as usize)),
                ),
            ])
        })
        .collect();
    let hold = if size > 0.97 {
        "h".repeat(1_300)
    } else {
        String::new()
    };
    let diag = |n: u64| {
        Value::Map(vec![
            (s("t_ms"), Value::from(n)),
            (s("v"), Value::from(vec![Value::from(0.5); 12])),
        ])
    };
    Value::Map(vec![
        (s("op"), s("state")),
        (
            s("owner"),
            s(["pilot", "skill", "goto", "fc"][rng.between(0, 3) as usize]),
        ),
        (
            s("owner_label"),
            Value::from("Flying · ".repeat(rng.between(1, 6) as usize)),
        ),
        (s("control_mode"), s("ados")),
        (s("armed"), Value::from(true)),
        (s("landed"), Value::from(false)),
        (s("fc_mode"), s("GUIDED")),
        (s("input_age_ms"), Value::from(rng.between(0, 400))),
        (s("hold_reason"), Value::from(hold)),
        (s("input_session"), Value::from(n)),
        (s("plan_ned"), Value::Array(plan)),
        (s("readiness"), Value::Array(readiness)),
        (
            s("sensors"),
            Value::from("y".repeat(rng.between(0, 700) as usize)),
        ),
        (s("command"), diag(n)),
        (s("shadow"), diag(n)),
        (s("stick"), diag(n)),
    ])
}

fn field<'a>(m: &'a [(Value, Value)], k: &str) -> Option<&'a Value> {
    m.iter()
        .find(|(key, _)| key.as_str() == Some(k))
        .map(|(_, v)| v)
}

/// The `input_session` of a state.
pub fn session(v: &Value) -> Option<u64> {
    field(v.as_map()?, "input_session")?.as_u64()
}

/// Check one downlinked payload against the state it carries: one aux
/// frame, the FC's system id first, every ground field kept (the plan and
/// readiness only ever shortened), no diagnostics.
pub fn check_payload(orig: &Value, payload: &[u8], sysid: u8) -> Result<(), String> {
    if payload.len() > DOWNLINK_MAX_BODY + 1 {
        return Err(format!(
            "payload of {} bytes exceeds one aux frame",
            payload.len()
        ));
    }
    if payload[0] != sysid {
        return Err(format!(
            "payload names system {} for FC {sysid}",
            payload[0]
        ));
    }
    let body = rmpv::decode::read_value(&mut &payload[1..])
        .map_err(|e| format!("body does not decode: {e}"))?;
    let (Some(got), Some(want)) = (body.as_map(), orig.as_map()) else {
        return Err("body is not a map".into());
    };
    for k in GROUND {
        let (Some(g), Some(w)) = (field(got, k), field(want, k)) else {
            return Err(format!("ground field `{k}` missing"));
        };
        let ok = match k {
            "plan_ned" => g == w || g.as_array().is_some_and(Vec::is_empty),
            "readiness" => g
                .as_array()
                .zip(w.as_array())
                .is_some_and(|(g, w)| g.iter().all(|i| w.contains(i))),
            _ => g == w,
        };
        if !ok {
            return Err(format!("ground field `{k}` altered"));
        }
    }
    match DIAG.iter().find(|k| field(got, k).is_some()) {
        Some(k) => Err(format!("diagnostic `{k}` downlinked")),
        None => Ok(()),
    }
}
