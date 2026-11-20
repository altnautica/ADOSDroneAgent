//! A phone ground, modelled on the Android app's handover
//! (`radio/rx/.../uplink/UplinkHandover.kt`, `UplinkMac.kt`) and the spec
//! (`06-uplink-handover.md`): take over the LAN and start only on the box's
//! standby; give back by stopping first and telling the box after; answer a
//! take-back call once quiet; yield at once on another ground's beacon (the
//! tie rule). Its MAC and its take-back guard are written here from the
//! phone's wire definition, independently of the box's code, so a box that
//! signs under the wrong context or body fails at the phone as it would in
//! the field.
//!
//! One move follows the spec where the app differs: a take-back call that
//! arrives while the transmitters open for a take the box just confirmed
//! closes them before the phone answers. Answering at once and opening
//! anyway lets the box transmit, confirmed, on top of the phone.

use std::collections::BTreeMap;
use std::net::IpAddr;

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::oneshot;

use crate::boxside::BOX_ID;
use crate::net::{Kind, Signed};
use crate::rng::Rng;

/// The phone's request context (`UplinkMac.Domain.Request`).
const REQUEST_CONTEXT: &[u8] = b"ados/gs-uplink/v1\n";
/// The take-back call's context (`UplinkMac.Domain.Yield`).
const YIELD_CONTEXT: &[u8] = b"ados/gs-uplink-yield/v1\n";
/// `UplinkMac.CLOCK_WINDOW_MS`.
const CLOCK_WINDOW_MS: i64 = 120_000;
/// `UplinkHandover.PEER_STALE_MS`: how long another ground's beacon blocks a take.
const PEER_STALE_MS: u64 = 25_000;
/// `UplinkHandover.TAKE_GRACE_MS`: the box's last beacon before it stopped.
const TAKE_GRACE_MS: u64 = 2_000;
/// `BoxUplinkApi`: connect plus read timeout.
const POST_TIMEOUT_MS: u64 = 24_000;
/// The phone's take-back listener port.
pub const YIELD_PORT: u16 = 47_811;

fn mac(context: &[u8], key: &[u8; 32], ts: i64, body: &[u8]) -> String {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("any key length");
    m.update(context);
    m.update(ts.to_string().as_bytes());
    m.update(b"\n");
    m.update(body);
    base64::engine::general_purpose::STANDARD.encode(m.finalize().into_bytes())
}

/// The phone's verdict on a take-back call (`MacVerdict`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accepted,
    NoKey,
    Mismatch,
    Clock,
    NotAYield,
    Replay,
}

/// What the phone does after its transmitters read quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Then {
    GiveBack,
    Quiet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    ReceiveOnly,
    /// A take in flight; `hold_until` once the box's standby arrived and the
    /// phone's transmitters are opening.
    Taking {
        msg: u64,
        deadline: u64,
        hold_until: Option<u64>,
    },
    Transmitting,
    /// Transmitters closing until they read quiet.
    Stopping {
        until: u64,
        then: Then,
    },
    GivingBack {
        msg: u64,
        deadline: u64,
    },
    Yielded,
    /// Crashed: no radio, no LAN.
    Dead,
}

/// What the phone sends.
#[derive(Debug)]
pub enum Act {
    /// A signed request to the box.
    Request(Signed),
    /// The answer to a take-back call.
    Answer {
        ok: bool,
        answer: Option<oneshot::Sender<bool>>,
    },
}

pub struct Phone {
    pub i: usize,
    pub device_id: String,
    pub key: [u8; 32],
    pub pub_b64: String,
    pub ip: IpAddr,
    /// The phone's wall clock minus the box's.
    pub skew_ms: i64,
    pub phase: Phase,
    /// Set by a take attempt: the box this phone took from (`held`).
    held: bool,
    /// The last take-back timestamp accepted (`YieldGuard`).
    guard_last: Option<i64>,
    /// Other grounds heard, by beacon id: when.
    other: BTreeMap<String, u64>,
    taken_at: u64,
    yield_during_take: bool,
    /// Take-back calls answered once the transmitters read quiet.
    waiting: Vec<Option<oneshot::Sender<bool>>>,
    /// The next presence beacon while transmitting.
    pub next_beacon: u64,
}

impl Phone {
    pub fn new(i: usize, device_id: String, rng: &mut Rng) -> Self {
        let pub_b64 = base64::engine::general_purpose::STANDARD.encode(rng.key());
        Self {
            i,
            device_id,
            key: rng.key(),
            pub_b64,
            ip: IpAddr::from([10, 0, 0, 10 + i as u8]),
            skew_ms: rng.ms(0, 10_000) as i64 - 5_000,
            phase: Phase::ReceiveOnly,
            held: false,
            guard_last: None,
            other: BTreeMap::new(),
            taken_at: 0,
            yield_during_take: false,
            waiting: Vec::new(),
            next_beacon: rng.ms(0, 10_000),
        }
    }

    /// Both transmitters radiate: opening, open, or closing.
    pub fn tx_up(&self) -> bool {
        matches!(
            self.phase,
            Phase::Taking {
                hold_until: Some(_),
                ..
            } | Phase::Transmitting
                | Phase::Stopping { .. }
        )
    }

    fn signed(&self, msg: u64, wall: i64, body: Value) -> Signed {
        let body = body.to_string().into_bytes();
        let ts = wall + self.skew_ms;
        Signed {
            msg,
            kind: Kind::Request,
            ts,
            mac: mac(REQUEST_CONTEXT, &self.key, ts, &body),
            phone_pub: Some(self.pub_b64.clone()),
            body,
        }
    }

    /// "Take uplink": refused at once while taking or holding, or while
    /// another ground than the box was heard in the stale window.
    pub fn take(&mut self, msg: u64, now: u64, wall: i64) -> Option<Signed> {
        if !matches!(self.phase, Phase::ReceiveOnly | Phase::Yielded) {
            return None;
        }
        let box_beacon = ados_groundlink::uplink::beacon_id(BOX_ID);
        if self
            .other
            .iter()
            .any(|(id, &t)| id != box_beacon && now.saturating_sub(t) < PEER_STALE_MS)
        {
            return None;
        }
        self.held = true;
        self.yield_during_take = false;
        self.phase = Phase::Taking {
            msg,
            deadline: now + POST_TIMEOUT_MS,
            hold_until: None,
        };
        let body = json!({
            "op": "take",
            "holder_device_id": self.device_id,
            "holder_name": format!("Phone {}", self.i),
            "yield_port": YIELD_PORT,
        });
        Some(self.signed(msg, wall, body))
    }

    /// "Give back": stop, read quiet, then tell the box.
    pub fn give_back(&mut self, now: u64, rng: &mut Rng) {
        if self.phase == Phase::Transmitting {
            self.stop(now, Then::GiveBack, rng);
        }
    }

    fn stop(&mut self, now: u64, then: Then, rng: &mut Rng) {
        // `RELEASE_QUIET_MS` of flat counters after the close, polled every
        // 200 ms, given up at 5 s.
        self.phase = Phase::Stopping {
            until: now + rng.ms(1_200, 4_600),
            then,
        };
    }

    pub fn crash(&mut self) {
        self.phase = Phase::Dead;
        self.waiting.clear();
    }

    /// The route's reply to this phone's request.
    pub fn on_reply(&mut self, msg: u64, reply: &Value, now: u64, rng: &mut Rng) {
        match self.phase {
            Phase::Taking {
                msg: m,
                hold_until: None,
                ..
            } if m == msg => {
                let standby = reply["ok"] == json!(true) && reply["state"] == json!("standby");
                self.phase = match (standby, self.yield_during_take) {
                    (true, true) => Phase::Yielded,
                    (true, false) => Phase::Taking {
                        msg,
                        deadline: u64::MAX,
                        hold_until: Some(now + rng.ms(150, 1_200)),
                    },
                    (false, _) => Phase::ReceiveOnly,
                };
            }
            Phase::GivingBack { msg: m, .. } if m == msg => self.phase = Phase::Yielded,
            _ => {}
        }
    }

    /// Timers: a take or give-back that got no reply, transmitters opened or
    /// read quiet.
    pub fn step(&mut self, now: u64, wall: i64, next_msg: &mut u64) -> Vec<Act> {
        let mut acts = Vec::new();
        match self.phase.clone() {
            Phase::Taking {
                deadline,
                hold_until: None,
                ..
            } if now >= deadline => self.phase = Phase::ReceiveOnly,
            Phase::Taking {
                hold_until: Some(t),
                ..
            } if now >= t => {
                self.phase = Phase::Transmitting;
                self.taken_at = now;
            }
            Phase::Stopping { until, then } if now >= until => {
                for answer in self.waiting.drain(..) {
                    acts.push(Act::Answer { ok: true, answer });
                }
                self.phase = Phase::Yielded;
                if then == Then::GiveBack {
                    *next_msg += 1;
                    let body = json!({"op": "give_back", "holder_device_id": self.device_id});
                    acts.push(Act::Request(self.signed(*next_msg, wall, body)));
                    self.phase = Phase::GivingBack {
                        msg: *next_msg,
                        deadline: now + POST_TIMEOUT_MS,
                    };
                }
            }
            Phase::GivingBack { deadline, .. } if now >= deadline => self.phase = Phase::Yielded,
            _ => {}
        }
        acts
    }

    /// Another ground's beacon decoded on the fleet's ground link: the tie rule.
    pub fn on_beacon(&mut self, id: &str, now: u64, rng: &mut Rng) {
        let box_beacon = ados_groundlink::uplink::beacon_id(BOX_ID);
        if id != box_beacon {
            self.other.insert(id.to_string(), now);
        }
        if self.phase != Phase::Transmitting {
            return;
        }
        if id == box_beacon && now.saturating_sub(self.taken_at) < TAKE_GRACE_MS {
            return;
        }
        self.stop(now, Then::Quiet, rng);
    }

    /// `YieldGuard.check`.
    fn check(&mut self, req: &Signed, wall: i64) -> Verdict {
        if !self.held {
            return Verdict::NoKey;
        }
        if req.mac != mac(YIELD_CONTEXT, &self.key, req.ts, &req.body) {
            return Verdict::Mismatch;
        }
        if (req.ts - (wall + self.skew_ms)).abs() > CLOCK_WINDOW_MS {
            return Verdict::Clock;
        }
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        if body["op"] != json!("yield") || body["box_device_id"] != json!(BOX_ID) {
            return Verdict::NotAYield;
        }
        if self.guard_last.is_some_and(|last| req.ts <= last) {
            return Verdict::Replay;
        }
        self.guard_last = Some(req.ts);
        Verdict::Accepted
    }

    /// A take-back call reaching the yield listener (`onYield`).
    pub fn on_yield(
        &mut self,
        req: &Signed,
        answer: Option<oneshot::Sender<bool>>,
        now: u64,
        wall: i64,
        rng: &mut Rng,
    ) -> (Verdict, Vec<Act>) {
        if self.phase == Phase::Dead {
            return (Verdict::NoKey, Vec::new());
        }
        let verdict = self.check(req, wall);
        if verdict != Verdict::Accepted {
            return (verdict, vec![Act::Answer { ok: false, answer }]);
        }
        match self.phase {
            // The transmitters open for a take the box confirmed: they close
            // before the answer, as a holder's do.
            Phase::Taking {
                hold_until: Some(_),
                ..
            }
            | Phase::Transmitting => {
                self.stop(now, Then::Quiet, rng);
                self.waiting.push(answer);
                return (verdict, Vec::new());
            }
            Phase::Taking { .. } => {
                self.yield_during_take = true;
            }
            Phase::Stopping { .. } => {
                self.waiting.push(answer);
                return (verdict, Vec::new());
            }
            _ => {}
        }
        (verdict, vec![Act::Answer { ok: true, answer }])
    }
}
