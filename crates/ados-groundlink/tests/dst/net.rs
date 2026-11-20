//! The two channels between the grounds, as one ordered event queue: the LAN
//! (a phone's signed take and give-back to the box's route, the route's reply,
//! the box's signed take-back call to the phone and its answer) and the
//! fleet's ground radio link (presence beacons). Each event is due at a
//! virtual millisecond; ties keep their send order, so a seed names the
//! delivery order exactly.

use std::collections::BTreeMap;

use serde_json::Value;
use tokio::sync::oneshot;

/// A ground of the fleet: the box, or phone `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gid {
    Box,
    Phone(usize),
}

impl std::fmt::Display for Gid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Gid::Box => write!(f, "box"),
            Gid::Phone(i) => write!(f, "phone{i}"),
        }
    }
}

/// Which copy of a signed message this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The one the sender sent.
    Original,
    /// A copy the network delivered a second time.
    Duplicate,
    /// A copy an eavesdropper on the LAN captured and sent again later.
    Replay,
}

/// What a signed message was when its sender signed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A phone's take or give-back to the box.
    Request,
    /// The box's take-back call to a phone.
    Yield,
}

/// One signed LAN message, as captured off the wire.
#[derive(Debug, Clone)]
pub struct Signed {
    /// Names the message across its copies.
    pub msg: u64,
    pub kind: Kind,
    /// The sender's timestamp header.
    pub ts: i64,
    /// The base64 MAC header.
    pub mac: String,
    /// The phone-key header (a phone's requests only).
    pub phone_pub: Option<String>,
    pub body: Vec<u8>,
}

/// One delivery.
#[derive(Debug)]
pub enum Event {
    /// A copy of a signed message reaching the box's LAN route.
    AtBox {
        from: Option<usize>,
        via: Via,
        req: Signed,
    },
    /// The route's reply reaching the phone that asked.
    Reply {
        phone: usize,
        msg: u64,
        reply: Value,
    },
    /// A copy of a signed message reaching phone `phone`'s yield listener;
    /// `answer` is the box's waiting call, on the genuine copy only, from the
    /// box's boot `boot`.
    AtPhone {
        phone: usize,
        via: Via,
        req: Signed,
        answer: Option<oneshot::Sender<bool>>,
        boot: u32,
    },
    /// The phone's answer to a take-back call reaching the box.
    YieldAnswer {
        ok: bool,
        answer: oneshot::Sender<bool>,
        boot: u32,
    },
    /// A presence beacon reaching a ground's receiver.
    Beacon { from: Gid, to: Gid, frame: [u8; 68] },
}

/// The events in flight, by due time then send order.
#[derive(Debug, Default)]
pub struct Queue {
    seq: u64,
    events: BTreeMap<(u64, u64), Event>,
}

impl Queue {
    pub fn push(&mut self, at_ms: u64, ev: Event) {
        self.seq += 1;
        self.events.insert((at_ms, self.seq), ev);
    }

    /// The next event due by `now_ms`.
    pub fn pop_due(&mut self, now_ms: u64) -> Option<Event> {
        let (&key, _) = self.events.first_key_value()?;
        if key.0 > now_ms {
            return None;
        }
        self.events.remove(&key)
    }
}
