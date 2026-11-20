//! The pilot side of a run: the router's real [`PilotLink`] fed the pilot's
//! `state` every 100 ms, its downlink leaving on a simulated aux radio that
//! refuses while the radio pair is down, and a ground station receiving it
//! over a lossy air link.
//!
//! Invariants, per state and per tick: every state is either downlinked
//! (sent or a counted send failure), reduced to fit, counted as an oversize
//! drop, or superseded within one downlink period of an accounted one; a
//! downlinked payload is one aux frame that keeps every ground field; and
//! the ground never goes a downlink period plus the air delay without a
//! state unless a counted drop, an air loss or the pilot's own silence
//! explains it.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};

use ados_mavlink_router::connection::FcConnection;
use ados_mavlink_router::pilot_link::{downlink_body, Downlink, PilotLink, DOWNLINK_PERIOD_MS};
use ados_protocol::pilot::ControlMode;
use rmpv::Value;

use crate::chan::{Faults, Link};
use crate::pilot::{check_payload, session, state, Radio, SimEgress};
use crate::sim::{World, STEP};

/// The pilot publishes its state this often.
pub const STATE_MS: u64 = 100;

pub struct PilotSide {
    link: PilotLink,
    _rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    pub radio: Arc<StdMutex<Radio>>,
    pub air: Link,
    n: u64,
    /// Recent states by `input_session`, with the FC id the router knew
    /// when each was offered, for the ground's oracle.
    recent: VecDeque<(u64, Value, u8)>,
    /// The pilot publishes nothing until then (a restart).
    pub silent_until: u64,
    /// When a state was last downlinked or counted as dropped; the link's
    /// start before the first (its period counts from there).
    accounted: u64,
    /// When the ground last received a state.
    ground_at: u64,
    /// When something last explained a gap on the ground.
    explained: u64,
    pub due: u64,
    pub delivered: u64,
}

impl PilotSide {
    pub fn new(fc: Arc<FcConnection>, air: Faults, now: u64) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let link = PilotLink::new(ControlMode::Ados, tx);
        let radio = Arc::new(StdMutex::new(Radio::default()));
        link.set_downlink(Downlink {
            egress: Arc::new(SimEgress(radio.clone())),
            fc_sysid: Box::new(move || fc.learned_fc_identity().map(|(s, _)| s)),
        });
        Self {
            link,
            _rx: rx,
            radio,
            air: Link::new(air),
            n: 0,
            recent: VecDeque::new(),
            silent_until: 0,
            accounted: now,
            ground_at: now,
            explained: now,
            due: 0,
            delivered: 0,
        }
    }

    /// `(sent, send_failed, oversize_dropped)`, the router status counters.
    pub fn counters(&self) -> (u64, u64, u64) {
        let s = &self.link.downlink_stats;
        (
            s.sent.load(Ordering::Relaxed),
            s.send_failed.load(Ordering::Relaxed),
            s.oversize_dropped.load(Ordering::Relaxed),
        )
    }
}

pub async fn step(w: &mut World) {
    let Some(mut p) = w.pilot.take() else {
        return;
    };
    let now = w.now;
    if now.is_multiple_of(STATE_MS) && now >= p.silent_until {
        offer(w, &mut p).await;
    }
    let out = std::mem::take(&mut p.radio.lock().unwrap().out);
    for payload in out {
        if !p.air.send(now, payload, &mut w.rng) {
            p.explained = now;
        }
    }
    for payload in p.air.recv(now) {
        p.delivered += 1;
        p.ground_at = now;
        let body = rmpv::decode::read_value(&mut &payload[1..]).ok();
        let n = body.as_ref().and_then(session);
        let Some((_, orig, sysid)) = p.recent.iter().find(|(k, _, _)| Some(*k) == n) else {
            w.fail(format_args!(
                "the ground received a state the pilot did not send"
            ));
        };
        if let Err(e) = check_payload(orig, &payload, *sysid) {
            w.fail(format_args!("downlinked state {n:?}: {e}"));
        }
    }
    let bound = DOWNLINK_PERIOD_MS + STATE_MS + p.air.max_delay() + 2 * STEP;
    let quiet = now - p.ground_at;
    let silent = now < p.silent_until || now - p.silent_until <= bound;
    if quiet > bound && now - p.explained > bound && !silent {
        w.fail(format_args!(
            "the ground has had no pilot state for {quiet} ms and nothing counted a drop"
        ));
    }
    w.pilot = Some(p);
}

/// One state from the pilot through the router's link.
async fn offer(w: &mut World, p: &mut PilotSide) {
    let now = w.now;
    p.n += 1;
    let v = state(&mut w.rng, p.n);
    let sysid = w.router.fc.learned_fc_identity().map_or(0, |(s, _)| s);
    p.recent.push_back((p.n, v.clone(), sysid));
    if p.recent.len() > 64 {
        p.recent.pop_front();
    }
    let (sent, failed, oversize) = p.counters();
    match p.link.on_pilot_state(&v) {
        Some(send) => {
            p.due += 1;
            send.send(&p.link.downlink_stats).await;
            let (s2, f2, o2) = p.counters();
            if s2 + f2 != sent + failed + 1 || o2 != oversize {
                w.fail(format_args!(
                    "a downlinked state was neither sent nor counted as failed"
                ));
            }
            if f2 > failed {
                p.explained = now;
            }
            p.accounted = now;
        }
        None => {
            let (s2, f2, o2) = p.counters();
            if (s2, f2) != (sent, failed) {
                w.fail(format_args!(
                    "a state not downlinked moved the send counters"
                ));
            }
            if o2 == oversize + 1 {
                p.due += 1;
                if downlink_body(&v).is_ok() {
                    w.fail(format_args!(
                        "state {} fits one aux frame reduced, yet was dropped",
                        p.n
                    ));
                }
                p.accounted = now;
                p.explained = now;
                w.note(format_args!("pilot state {} oversize dropped", p.n));
            } else if now - p.accounted >= DOWNLINK_PERIOD_MS {
                w.fail(format_args!(
                    "state {} silently skipped: nothing downlinked or counted for {} ms",
                    p.n,
                    now - p.accounted
                ));
            }
        }
    }
}
