//! The run's trace and its invariants. Every check returns the violation as
//! one sentence; the world stops the run on the first with its seed and the
//! trace's tail.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::net::{Kind, Signed, Via};
use crate::phone::Verdict;

/// The holder timer's window (`uplink::HOLDER_STALE`).
pub const HOLDER_STALE_MS: u64 = 25_000;
/// The longest two grounds may radiate at once: only after the box took the
/// uplink back without the holder's confirmation, until the holder decodes
/// another ground's beacon (every 10 s, some lost) and reads quiet.
pub const OVERLAP_MAX_MS: u64 = 60_000;
/// The longest no ground may transmit: a holder gone silent right after its
/// last beacon, the stale window, the watcher's 1 s tick, the start.
pub const GAP_MAX_MS: u64 = HOLDER_STALE_MS + 5_000;
/// The take-back call's clock window (`sign::CLOCK_WINDOW_MS`).
const CLOCK_WINDOW_MS: i64 = 120_000;

/// What the drone saw and the grounds did, line by line.
#[derive(Debug, Default)]
pub struct Trace {
    pub bytes: Vec<u8>,
}

impl Trace {
    pub fn line(&mut self, now: u64, text: &str) {
        self.bytes
            .extend_from_slice(format!("{:>7}.{:03} {text}\n", now / 1000, now % 1000).as_bytes());
    }

    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(&self.bytes).into()
    }

    /// The first line where two traces differ: its number and both lines.
    pub fn first_difference(&self, other: &Trace) -> Option<(usize, String, String)> {
        let a = String::from_utf8_lossy(&self.bytes);
        let b = String::from_utf8_lossy(&other.bytes);
        let (mut la, mut lb) = (a.lines(), b.lines());
        for n in 1.. {
            match (la.next(), lb.next()) {
                (None, None) => return None,
                (x, y) if x == y => continue,
                (x, y) => {
                    let s = |l: Option<&str>| l.unwrap_or("<end>").to_string();
                    return Some((n, s(x), s(y)));
                }
            }
        }
        None
    }

    /// The last `n` lines.
    pub fn tail(&self, n: usize) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }
}

/// What a run did, for the scenarios' outcome checks.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub takes: u32,
    pub give_backs: u32,
    pub take_backs_confirmed: u32,
    pub take_backs_unconfirmed: u32,
    /// Take-backs refused because a phone took the uplink meanwhile.
    pub take_backs_changed: u32,
    pub fallbacks: u32,
    pub refused: u32,
    pub replays_refused: u32,
    pub yields_refused: u32,
    pub overlap_ms: u64,
    pub max_overlap_ms: u64,
    pub max_gap_ms: u64,
}

/// One tick's view of every ground.
pub struct View<'a> {
    pub now: u64,
    pub box_air: bool,
    pub wants_transmit: bool,
    /// The box's holder (device id, phone key) while it stands by.
    pub standby: Option<(String, String)>,
    pub phone_tx: &'a [bool],
}

/// The invariants' memory.
#[derive(Debug, Default)]
pub struct Ledger {
    /// Phones the box stopped counting on without their confirmation: since.
    revoked: Vec<Option<u64>>,
    /// Signed requests the route admitted, by message.
    admitted: BTreeMap<u64, Via>,
    /// (device id, phone key) of every take the route admitted.
    takes: BTreeSet<(String, String)>,
    /// (phone, message) of every take-back call a phone accepted.
    accepted: BTreeSet<(usize, u64)>,
    overlap_since: Option<u64>,
    gap_since: Option<u64>,
    /// The standby's holder and the later of the standby's start and the
    /// holder's last beacon the box decoded.
    floor: Option<(String, u64)>,
    pub stats: Stats,
}

impl Ledger {
    pub fn new(phones: usize) -> Self {
        Self {
            revoked: vec![None; phones],
            ..Self::default()
        }
    }

    /// The box will transmit without `phone`'s confirmation.
    pub fn revoke(&mut self, phone: usize, now: u64) {
        self.revoked[phone].get_or_insert(now);
    }

    /// `phone` asked for the uplink again: whatever it holds next is new.
    pub fn take_sent(&mut self, phone: usize) {
        self.revoked[phone] = None;
    }

    /// The route admitted a copy of a signed request.
    pub fn admitted(
        &mut self,
        msg: u64,
        via: Via,
        ts: i64,
        take: Option<(String, String)>,
        wall: i64,
    ) -> Result<(), String> {
        if let Some(first) = self.admitted.get(&msg) {
            return Err(format!(
                "the route admitted a {via:?} copy of request {msg} after admitting its {first:?} copy: a replayed request was accepted"
            ));
        }
        if (wall - ts).abs() > CLOCK_WINDOW_MS {
            return Err(format!(
                "the route admitted request {msg} signed {} s from its clock",
                (wall - ts) / 1000
            ));
        }
        self.admitted.insert(msg, via);
        if let Some(t) = take {
            self.takes.insert(t);
        }
        Ok(())
    }

    pub fn refused(&mut self, msg: u64, via: Via) {
        self.stats.refused += 1;
        if via != Via::Original && self.admitted.contains_key(&msg) {
            self.stats.replays_refused += 1;
        }
    }

    /// A phone's verdict on a copy of a signed message at its yield listener.
    pub fn yield_verdict(
        &mut self,
        phone: usize,
        via: Via,
        req: &Signed,
        verdict: Verdict,
    ) -> Result<(), String> {
        match verdict {
            Verdict::Accepted if req.kind == Kind::Request => Err(format!(
                "phone{phone} accepted its own signed request {} as a take-back call",
                req.msg
            )),
            Verdict::Accepted if !self.accepted.insert((phone, req.msg)) => Err(format!(
                "phone{phone} accepted a {via:?} copy of take-back call {} it had accepted before: a stale yield passed",
                req.msg
            )),
            Verdict::Accepted | Verdict::Replay => Ok(()),
            v if via == Via::Original && req.kind == Kind::Yield => Err(format!(
                "phone{phone} refused the box's genuine take-back call {} as {v:?}",
                req.msg
            )),
            _ => {
                self.stats.yields_refused += 1;
                Ok(())
            }
        }
    }

    /// The box decoded its holder's beacon.
    pub fn holder_beacon(&mut self, now: u64) {
        if let Some((_, t)) = &mut self.floor {
            *t = now;
        }
    }

    /// The holder timer took the uplink back from `holder`.
    pub fn fell_back(&mut self, holder: &str, now: u64, tick: u64) -> Result<(), String> {
        self.stats.fallbacks += 1;
        match &self.floor {
            Some((h, floor)) if h == holder && now + tick < floor + HOLDER_STALE_MS => {
                Err(format!(
                    "the box fell back from {holder} {} ms after its last beacon or the standby, before the {HOLDER_STALE_MS} ms window",
                    now - floor
                ))
            }
            _ => Ok(()),
        }
    }

    /// The box restarted: whatever it stood by for is gone.
    pub fn box_restarted(&mut self) {
        self.floor = None;
    }

    /// The invariants every tick.
    pub fn tick(&mut self, v: &View<'_>, tick: u64) -> Result<(), String> {
        let now = v.now;
        if v.standby.is_some() && !v.wants_transmit && v.box_air {
            return Err(
                "the box reads standby with its transmitters asked down, yet they radiate".into(),
            );
        }
        if let Some((id, key)) = &v.standby {
            if !self.takes.contains(&(id.clone(), key.clone())) {
                return Err(format!(
                    "the box stands by for {id}, which no admitted take named: it yielded to a phone it did not hand the uplink to"
                ));
            }
            match &self.floor {
                Some((h, _)) if h == id => {}
                _ => self.floor = Some((id.clone(), now)),
            }
        } else {
            self.floor = None;
        }
        if let (Some((h, floor)), false) = (&self.floor, v.wants_transmit) {
            if now > floor + HOLDER_STALE_MS + 1_000 + 2 * tick {
                return Err(format!(
                    "{h} has been silent for {} ms and the box still stands by",
                    now - floor
                ));
            }
        }
        let on_air: Vec<String> = std::iter::once(("box".to_string(), v.box_air))
            .chain(
                v.phone_tx
                    .iter()
                    .enumerate()
                    .map(|(i, &up)| (format!("phone{i}"), up)),
            )
            .filter(|(_, up)| *up)
            .map(|(n, _)| n)
            .collect();
        let licensed = usize::from(v.box_air)
            + v.phone_tx
                .iter()
                .enumerate()
                .filter(|(i, &up)| up && self.revoked[*i].is_none())
                .count();
        if licensed > 1 {
            return Err(format!(
                "two grounds transmit for one fleet ({}) and the box handed the uplink over with confirmation",
                on_air.join(" + ")
            ));
        }
        if on_air.len() > 1 {
            let since = *self.overlap_since.get_or_insert(now);
            self.stats.overlap_ms += tick;
            self.stats.max_overlap_ms = self.stats.max_overlap_ms.max(now - since);
            if now - since > OVERLAP_MAX_MS {
                return Err(format!(
                    "{} have transmitted together for {} ms",
                    on_air.join(" + "),
                    now - since
                ));
            }
        } else {
            self.overlap_since = None;
        }
        if on_air.is_empty() {
            let since = *self.gap_since.get_or_insert(now);
            self.stats.max_gap_ms = self.stats.max_gap_ms.max(now - since);
            if now - since > GAP_MAX_MS {
                return Err(format!("no ground has transmitted for {} ms", now - since));
            }
        } else {
            self.gap_since = None;
        }
        Ok(())
    }

    /// The end of a calm tail: exactly one ground holds the fleet's uplink,
    /// and the box agrees which.
    pub fn end(&self, v: &View<'_>, phone_ids: &[String]) -> Result<(), String> {
        let up: Vec<usize> = (0..v.phone_tx.len()).filter(|&i| v.phone_tx[i]).collect();
        match (v.box_air, up.as_slice()) {
            (true, []) if v.standby.is_none() && v.wants_transmit => Ok(()),
            (false, [i]) if v.standby.as_ref().map(|s| &s.0) == Some(&phone_ids[*i]) => Ok(()),
            _ => Err(format!(
                "the run ended with box on air {}, phones on air {up:?}, box standby {:?}",
                v.box_air, v.standby
            )),
        }
    }
}
