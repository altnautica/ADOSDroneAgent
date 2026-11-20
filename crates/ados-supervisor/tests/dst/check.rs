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

use crate::world::{key_origin, lock, Rig, Shared, World, DRONE, GS};

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

/// How the pair stands against `before`: both rigs on new keys, neither, or one.
pub fn classify(g: &World, before: &Snapshot) -> Outcome {
    let changed = [DRONE, GS].map(|i| g.rigs[i].key().map(<[u8]>::to_vec) != before.keys[i]);
    match changed {
        [false, false] => Outcome::Aborted,
        [true, true] => Outcome::Bound,
        _ => Outcome::Split,
    }
}

/// The rig's fleet addressing as written, `None` when it carries none.
fn fleet_of(r: &Rig) -> Option<(u64, u64)> {
    Some((
        wfb(r, "fleet_id").and_then(Value::as_u64)?,
        wfb(r, "fleet_slot").and_then(Value::as_u64)?,
    ))
}

/// The config without its fleet addressing, which a keyless rig takes the
/// moment it is issued, bound or not.
fn pair_fields(config: &Mapping) -> Mapping {
    let mut c = config.clone();
    if let Some(Value::Mapping(wfb)) = c
        .get_mut("video")
        .and_then(|v| v.as_mapping_mut())
        .and_then(|v| v.get_mut("wfb"))
    {
        wfb.remove("fleet_id");
        wfb.remove("fleet_slot");
    }
    c
}

/// Whether the radio service would bring this rig's link up: its `video.wfb`
/// block read as `ados_radio`'s config, then the radio's own fleet-identity
/// rule. `Err` names the refusal.
fn radio_accepts(r: &Rig) -> Result<(), String> {
    let wfb = r
        .config
        .get("video")
        .and_then(|v| v.get("wfb"))
        .cloned()
        .unwrap_or(Value::Mapping(Mapping::new()));
    let cfg: ados_radio::config::WfbConfig =
        serde_norway::from_value(wfb).map_err(|e| format!("video.wfb does not parse: {e}"))?;
    let ground = r.role == ados_supervisor::bind::BindRole::Gs;
    match ados_radio::config::fleet_identity_error(cfg.fleet_id, cfg.fleet_slot, ground) {
        None => Ok(()),
        Some(e) => Err(e.to_string()),
    }
}

/// The end-state check: classify the outcome and record every break.
/// `issued` is the addressing the ground station's registry issued each rig.
pub fn finish(
    w: &Shared,
    before: &Snapshot,
    issued: [FleetIdentity; 2],
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
            // A rig's last reported bind says paired exactly when the rig's
            // key now differs from the key it held when that bind started (a
            // key landing after a failed report is the status lie). A rig
            // whose last attempt never reported (its supervisor was killed, or
            // it never bound) is judged by its enrolment below.
            if let Some(state) = r.last_state.as_deref() {
                let took = r.key() != r.last_bind_key.as_deref();
                if (state == "paired") != took {
                    breaks.push(format!(
                        "{}: last bind reports {state} but its key {}",
                        r.name,
                        if took { "changed" } else { "did not change" }
                    ));
                }
            }
            if !changed[i] && pair_fields(&r.config) != pair_fields(&before.config[i]) {
                breaks.push(format!("{}: pair config changed without a new key", r.name));
            }
            // Whatever addressing a rig holds is the one it was issued.
            let want = (
                u64::from(issued[i].fleet_id),
                u64::from(issued[i].fleet_slot),
            );
            if let Some(have) = fleet_of(r).filter(|have| *have != want) {
                breaks.push(format!(
                    "{}: holds fleet/slot {have:?}, was issued {want:?}",
                    r.name
                ));
            }
        }
        let origin = [DRONE, GS].map(|i| g.rigs[i].key().and_then(key_origin));
        // A rig that took a key holds a whole enrolment: its half of a
        // generated pair, auto-pair disarmed, and fleet addressing.
        let whole = [
            matches!(origin[DRONE], Some((_, 0))),
            matches!(origin[GS], Some((_, 1))),
        ];
        for i in [DRONE, GS] {
            let r = &g.rigs[i];
            if changed[i] && (!whole[i] || armed(r) || fleet_of(r).is_none()) {
                breaks.push(format!(
                    "{}: partial enrolment: key {:?}, fleet {:?}, armed {}",
                    r.name,
                    origin[i],
                    fleet_of(r),
                    armed(r)
                ));
            }
        }
        let outcome = classify(&g, before);
        if outcome == Outcome::Bound {
            if !matches!(origin, [Some((a, 0)), Some((b, 1))] if a == b) {
                breaks.push(format!(
                    "the rigs took keys of different pairs: drone {:?}, gs {:?}",
                    origin[DRONE], origin[GS]
                ));
            }
            // A bound pair flies: the radio accepts both configs, the drone's
            // slot is the one the ground station's registry holds, and the
            // ground hears it: on its own fleet, or (an auto-paired drone,
            // whose bind carries no fleet id) on the default fleet the ground
            // also listens on when it is on another.
            for r in &g.rigs {
                if let Err(e) = radio_accepts(r) {
                    breaks.push(format!(
                        "{}: the radio refuses the bound config: {e}",
                        r.name
                    ));
                }
            }
            let (drone, gs) = (fleet_of(&g.rigs[DRONE]), fleet_of(&g.rigs[GS]));
            let registry = u64::from(issued[DRONE].fleet_slot);
            let heard = match (drone, gs.and_then(|f| u16::try_from(f.0).ok())) {
                (Some((fleet, slot)), Some(ground)) => {
                    fleet == u64::from(ground)
                        || (ados_radio::config::provisional_fleet(ground).map(u64::from)
                            == Some(fleet)
                            && slot == u64::from(ados_radio::config::FIRST_DRONE_SLOT))
                }
                _ => false,
            };
            if drone.map(|f| f.1) != Some(registry) || !heard {
                breaks.push(format!(
                    "bound drone holds fleet/slot {drone:?}; the ground holds fleet {:?}, slot {registry}",
                    gs.map(|f| f.0)
                ));
            }
        }
        outcome
    };
    record(w, breaks);
    outcome
}
