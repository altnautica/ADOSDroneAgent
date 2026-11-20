//! The invariants: checked every 100 ms of simulated time while the rigs run,
//! and once more on the settled end state.
//!
//! Every tick, on a rig whose supervisor is up and has no bind call in flight:
//! the normal radio unit runs, the bind unit is stopped, the bind sentinel
//! reads inactive and no socat is left. At the end: the bind is either bound
//! on both rigs (the drone runs the key the ground station generated for it,
//! the ground station its half of the same keypair, the drone holds the slot it
//! was issued, auto-pair disarmed on both) or aborted on both (keys and pair
//! config exactly as before), and a rig's last bind reports `paired` exactly
//! when its key changed.

use std::time::Duration;

use ados_supervisor::bind::keys::FleetIdentity;
use serde_norway::{Mapping, Value};

use crate::world::{key_origin, lock, Rig, Shared, DRONE, GS};

/// How the run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Both rigs bound to one keypair.
    Bound,
    /// Neither rig changed.
    Aborted,
    /// Anything else (a violation).
    Split,
}

/// The pair state of both rigs before the run.
pub struct Snapshot {
    keys: [Option<Vec<u8>>; 2],
    config: [Mapping; 2],
}

impl Snapshot {
    pub fn take(w: &Shared) -> Self {
        let g = lock(w);
        Self {
            keys: [0, 1].map(|i| g.rigs[i].key().map(<[u8]>::to_vec)),
            config: [0, 1].map(|i| g.rigs[i].config.clone()),
        }
    }
}

/// `video.wfb.<key>` from a rig's config.
fn wfb<'a>(r: &'a Rig, key: &str) -> Option<&'a Value> {
    r.config
        .get("video")
        .and_then(|v| v.get("wfb"))
        .and_then(|v| v.get(key))
}

/// Whether auto-pair is still armed on the rig.
pub fn armed(r: &Rig) -> bool {
    wfb(r, "auto_pair_enabled").and_then(Value::as_bool) == Some(true)
}

/// The radio-at-rest invariant on one rig, as a list of breaks.
fn at_rest(r: &Rig) -> Vec<String> {
    let mut out = Vec::new();
    if !r.unit_running(r.role.normal_unit()) {
        out.push(format!("{}: normal radio unit not running", r.name));
    }
    if r.unit_running(r.role.bind_unit()) {
        out.push(format!("{}: bind unit still running", r.name));
    }
    if r.sentinel_active() {
        out.push(format!("{}: bind sentinel still reads active", r.name));
    }
    if r.socats > 0 || r.listening {
        out.push(format!("{}: a bind socat is still alive", r.name));
    }
    out
}

/// Record a break once per message.
fn record(w: &Shared, breaks: Vec<String>) {
    let mut g = lock(w);
    let ms = g.now_ms();
    for b in breaks {
        if !g.violations.iter().any(|v| v.starts_with(&b)) {
            g.log(format!("VIOLATION {b}"));
            g.violations.push(format!("{b} (first at {ms} ms)"));
        }
    }
}

/// The per-tick check.
pub async fn monitor(w: Shared) {
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let breaks: Vec<String> = {
            let g = lock(&w);
            g.rigs
                .iter()
                .filter(|r| r.settled && !r.busy)
                .flat_map(at_rest)
                .collect()
        };
        record(&w, breaks);
    }
}

/// The end-state check: classify the outcome and record every break.
pub fn finish(
    w: &Shared,
    before: &Snapshot,
    drone_fleet: FleetIdentity,
    gs_present: bool,
) -> Outcome {
    let mut breaks = Vec::new();
    let outcome = {
        let g = lock(w);
        let rigs = if gs_present {
            &g.rigs[..]
        } else {
            &g.rigs[..1]
        };
        for r in rigs {
            breaks.extend(at_rest(r));
        }
        let changed = [DRONE, GS].map(|i| g.rigs[i].key().map(<[u8]>::to_vec) != before.keys[i]);
        for i in [DRONE, GS] {
            let r = &g.rigs[i];
            // A rig whose last attempt never reported (its supervisor was
            // killed) reports its pair state from config.
            let paired = match r.last_state.as_deref() {
                Some(s) => s == "paired",
                None => !armed(r),
            };
            if paired != changed[i] {
                breaks.push(format!(
                    "{}: last bind reports {:?} but the key {}",
                    r.name,
                    r.last_state,
                    if changed[i] {
                        "changed"
                    } else {
                        "did not change"
                    }
                ));
            }
            if !changed[i] && r.config != before.config[i] {
                breaks.push(format!("{}: pair config changed without a new key", r.name));
            }
        }
        let origin = [DRONE, GS].map(|i| g.rigs[i].key().and_then(key_origin));
        let drone = &g.rigs[DRONE];
        let slot_ok = wfb(drone, "fleet_slot").and_then(Value::as_u64)
            == Some(u64::from(drone_fleet.fleet_slot))
            && wfb(drone, "fleet_id").and_then(Value::as_u64)
                == Some(u64::from(drone_fleet.fleet_id));
        // A rig that took a key holds a whole enrolment: its half of a
        // generated pair, auto-pair disarmed, and on the drone the issued slot.
        let whole = [
            matches!(origin[DRONE], Some((_, 0))) && slot_ok && !armed(drone),
            matches!(origin[GS], Some((_, 1))) && !armed(&g.rigs[GS]),
        ];
        for i in [DRONE, GS] {
            if changed[i] && !whole[i] {
                breaks.push(format!(
                    "{}: partial enrolment: key {:?}, slot ok {slot_ok}, armed {}",
                    g.rigs[i].name,
                    origin[i],
                    armed(&g.rigs[i])
                ));
            }
        }
        match changed {
            [false, false] => Outcome::Aborted,
            [true, true] => {
                if !matches!(origin, [Some((a, 0)), Some((b, 1))] if a == b) {
                    breaks.push(format!(
                        "the rigs took keys of different pairs: drone {:?}, gs {:?}",
                        origin[DRONE], origin[GS]
                    ));
                }
                Outcome::Bound
            }
            _ => Outcome::Split,
        }
    };
    record(w, breaks);
    outcome
}
