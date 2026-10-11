//! The pure half of route-aware forwarding: which endpoint owns which
//! `(sysid, compid)`, and which endpoints an FC frame is delivered to.
//!
//! No I/O and no clock of its own: every call takes `now`, so the TTL and the
//! reply-correlation window are tested without sleeping.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::mavlink::{self, Message};
use serde_json::{json, Map, Value};

/// How long a learned `(sysid, compid) -> endpoint` route stays valid without
/// a fresh frame from that source.
pub const ROUTE_TTL: Duration = Duration::from_secs(30);

/// How long a `PARAM_REQUEST_READ` waits for the FC's `PARAM_VALUE` before the
/// reply falls back to the normal (broadcast) rule.
pub const PARAM_REPLY_WINDOW: Duration = Duration::from_secs(3);

/// A routing endpoint: a proxy client, the IPC socket, the cloud relay, the
/// aux radio lane or the FC itself.
pub type EndpointId = u32;

/// Per-endpoint frame counters. `rx` counts frames received from the endpoint,
/// `tx` frames delivered to it.
#[derive(Debug, Default)]
pub struct EndpointCounters {
    pub rx: AtomicU64,
    pub tx: AtomicU64,
}

/// Which endpoints an FC frame goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    /// Every endpoint (broadcast, untargeted, or a target nobody owns).
    All,
    /// Only these endpoints.
    Only(Arc<[EndpointId]>),
}

impl Audience {
    /// Whether `ep` is in the audience.
    pub fn includes(&self, ep: EndpointId) -> bool {
        match self {
            Audience::All => true,
            Audience::Only(eps) => eps.contains(&ep),
        }
    }
}

/// What a `PARAM_REQUEST_READ` names, and so what its `PARAM_VALUE` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamKey {
    Index(i16),
    Id([u8; 16]),
}

#[derive(Debug)]
struct PendingParam {
    key: ParamKey,
    ep: EndpointId,
    at: Instant,
}

/// The source ids of one frame: `(sysid, compid)`.
pub fn frame_source(frame: &[u8]) -> Option<(u8, u8)> {
    match frame.first() {
        Some(&0xFD) if frame.len() >= 10 => Some((frame[5], frame[6])),
        Some(&0xFE) if frame.len() >= 6 => Some((frame[3], frame[4])),
        _ => None,
    }
}

/// Total on-wire length of the frame at the start of `buf`, signature
/// included, or `None` when `buf` does not start with a whole frame.
pub fn frame_len(buf: &[u8]) -> Option<usize> {
    let len = match buf.first() {
        Some(&0xFD) if buf.len() >= 3 => {
            let signed = buf[2] & 0x01 != 0;
            12 + buf[1] as usize + if signed { 13 } else { 0 }
        }
        Some(&0xFE) if buf.len() >= 2 => 8 + buf[1] as usize,
        _ => return None,
    };
    (buf.len() >= len).then_some(len)
}

/// The route table. One instance per router, behind the hub's mutex.
#[derive(Debug, Default)]
pub struct RouteTable {
    endpoints: HashMap<EndpointId, (String, Arc<EndpointCounters>)>,
    by_label: HashMap<String, EndpointId>,
    next_id: EndpointId,
    routes: HashMap<(u8, u8, EndpointId), Instant>,
    pending: Vec<PendingParam>,
}

impl RouteTable {
    /// The endpoint named `label`, registered on first use.
    pub fn endpoint(&mut self, label: &str) -> (EndpointId, Arc<EndpointCounters>) {
        if let Some(&id) = self.by_label.get(label) {
            if let Some((_, counters)) = self.endpoints.get(&id) {
                return (id, counters.clone());
            }
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let counters = Arc::new(EndpointCounters::default());
        self.endpoints
            .insert(id, (label.to_string(), counters.clone()));
        self.by_label.insert(label.to_string(), id);
        (id, counters)
    }

    /// Forget a gone endpoint: its label, counters, routes and pending reads.
    pub fn retire(&mut self, id: EndpointId) {
        if let Some((label, _)) = self.endpoints.remove(&id) {
            self.by_label.remove(&label);
        }
        self.routes.retain(|(_, _, ep), _| *ep != id);
        self.pending.retain(|p| p.ep != id);
    }

    /// Learn the source of every whole frame in `data` (one or more
    /// concatenated frames) as owned by `ep`, and note any parameter read it
    /// asks for so the reply can go back to it alone.
    pub fn learn(&mut self, ep: EndpointId, data: &[u8], now: Instant) {
        let mut rest = data;
        while let Some(len) = frame_len(rest) {
            let frame = &rest[..len];
            if let Some((sys, comp)) = frame_source(frame) {
                self.routes.insert((sys, comp, ep), now);
            }
            if let Ok((_, MavMessage::PARAM_REQUEST_READ(d))) = mavlink::parse_any(frame) {
                let key = if d.param_index >= 0 {
                    ParamKey::Index(d.param_index)
                } else {
                    ParamKey::Id(d.param_id.into())
                };
                self.pending.push(PendingParam { key, ep, at: now });
            }
            rest = &rest[len..];
        }
    }

    /// Learn the FC's own source id as owned by `fc_ep`, and pick the
    /// audience of the FC frame.
    ///
    /// A `PARAM_VALUE` answering a pending read goes to the endpoint that asked.
    /// A frame whose message targets a system goes to the endpoints that own
    /// it (the exact `(sysid, compid)` first, then any component of the
    /// system). Target `0`, a message with no target, and a target no endpoint
    /// owns are broadcast.
    pub fn classify(&mut self, fc_ep: EndpointId, frame: &[u8], now: Instant) -> Audience {
        self.prune(now);
        if let Some((sys, comp)) = frame_source(frame) {
            self.routes.insert((sys, comp, fc_ep), now);
        }
        let Ok((_, msg)) = mavlink::parse_any(frame) else {
            return Audience::All;
        };
        if let MavMessage::PARAM_VALUE(d) = &msg {
            let id: [u8; 16] = d.param_id.into();
            let hit = self.pending.iter().position(|p| match p.key {
                ParamKey::Index(i) => i == d.param_index as i16,
                ParamKey::Id(want) => want == id,
            });
            if let Some(at) = hit {
                let p = self.pending.remove(at);
                return Audience::Only(Arc::from([p.ep]));
            }
        }
        let Some(sys) = msg.target_system_id().filter(|s| *s != 0) else {
            return Audience::All;
        };
        let comp = msg.target_component_id().unwrap_or(0);
        let owners = |exact: bool| -> Vec<EndpointId> {
            let mut eps: Vec<EndpointId> = self
                .routes
                .keys()
                .filter(|(s, c, ep)| {
                    *s == sys && *ep != fc_ep && (!exact || comp == 0 || *c == comp)
                })
                .map(|(_, _, ep)| *ep)
                .collect();
            eps.sort_unstable();
            eps.dedup();
            eps
        };
        let mut eps = owners(true);
        if eps.is_empty() {
            eps = owners(false);
        }
        if eps.is_empty() {
            Audience::All
        } else {
            Audience::Only(Arc::from(eps))
        }
    }

    /// Drop routes past [`ROUTE_TTL`] and reads past [`PARAM_REPLY_WINDOW`].
    pub fn prune(&mut self, now: Instant) {
        self.routes
            .retain(|_, at| now.saturating_duration_since(*at) < ROUTE_TTL);
        self.pending
            .retain(|p| now.saturating_duration_since(p.at) < PARAM_REPLY_WINDOW);
    }

    /// The sidecar body: `{version, routes:[{sysid,compid,endpoint}],
    /// per_endpoint:{label:{rx,tx}}}`, sorted so an unchanged table serialises
    /// identically.
    pub fn snapshot(&mut self, now: Instant) -> Value {
        self.prune(now);
        let label = |ep: &EndpointId| self.endpoints.get(ep).map_or("", |(l, _)| l.as_str());
        let mut routes: Vec<(u8, u8, EndpointId)> = self.routes.keys().copied().collect();
        routes.sort_unstable();
        let routes: Vec<Value> = routes
            .iter()
            .map(|(s, c, ep)| json!({"sysid": s, "compid": c, "endpoint": label(ep)}))
            .collect();
        let mut eps: Vec<&(String, Arc<EndpointCounters>)> = self.endpoints.values().collect();
        eps.sort_by(|a, b| a.0.cmp(&b.0));
        let mut per = Map::new();
        for (name, c) in eps {
            per.insert(
                name.clone(),
                json!({"rx": c.rx.load(Ordering::Relaxed), "tx": c.tx.load(Ordering::Relaxed)}),
            );
        }
        json!({"version": 1, "routes": routes, "per_endpoint": per})
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ados_protocol::mavlink::ardupilotmega::{
        HEARTBEAT_DATA, MISSION_ACK_DATA, PARAM_REQUEST_READ_DATA, PARAM_VALUE_DATA,
    };
    use ados_protocol::mavlink::MavHeader;

    pub(crate) fn frame(sys: u8, comp: u8, msg: &MavMessage) -> Vec<u8> {
        mavlink::serialize_v2(
            MavHeader {
                system_id: sys,
                component_id: comp,
                sequence: 0,
            },
            msg,
        )
        .unwrap()
    }

    /// An FC frame targeted at `(sys, comp)`. MISSION_ACK carries its target
    /// in the base payload; the dialect build here has no COMMAND_ACK target
    /// extension fields, so a COMMAND_ACK classifies as untargeted.
    pub(crate) fn ack_to(sys: u8, comp: u8) -> Vec<u8> {
        let d = MISSION_ACK_DATA {
            target_system: sys,
            target_component: comp,
            ..Default::default()
        };
        frame(1, 1, &MavMessage::MISSION_ACK(d))
    }

    pub(crate) fn heartbeat(sys: u8, comp: u8) -> Vec<u8> {
        frame(sys, comp, &MavMessage::HEARTBEAT(HEARTBEAT_DATA::default()))
    }

    #[test]
    fn frame_len_covers_v1_v2_and_signature() {
        let hb = heartbeat(255, 190);
        assert_eq!(frame_len(&hb), Some(hb.len()));
        assert_eq!(frame_len(&hb[..hb.len() - 1]), None);
        let mut signed = hb.clone();
        signed[2] |= 1;
        signed.extend_from_slice(&[0u8; 13]);
        assert_eq!(frame_len(&signed), Some(hb.len() + 13));
    }

    #[test]
    fn targeted_frame_picks_owner_and_unknown_target_broadcasts() {
        let mut t = RouteTable::default();
        let now = Instant::now();
        let (fc, _) = t.endpoint("fc");
        let (a, _) = t.endpoint("tcp#1");
        let (b, _) = t.endpoint("tcp#2");
        t.learn(a, &heartbeat(255, 190), now);
        t.learn(b, &heartbeat(254, 190), now);
        assert_eq!(
            t.classify(fc, &ack_to(255, 190), now),
            Audience::Only(Arc::from([a]))
        );
        assert_eq!(t.classify(fc, &ack_to(0, 0), now), Audience::All);
        assert_eq!(t.classify(fc, &ack_to(77, 1), now), Audience::All);
        assert_eq!(t.classify(fc, &heartbeat(1, 1), now), Audience::All);
        // A route past its TTL no longer claims the target.
        assert_eq!(
            t.classify(fc, &ack_to(255, 190), now + ROUTE_TTL),
            Audience::All
        );
    }

    #[test]
    fn param_value_goes_back_to_the_reader_once() {
        let mut t = RouteTable::default();
        let now = Instant::now();
        let (fc, _) = t.endpoint("fc");
        let (a, _) = t.endpoint("tcp#1");
        let req = PARAM_REQUEST_READ_DATA {
            param_index: 7,
            target_system: 1,
            ..Default::default()
        };
        t.learn(
            a,
            &frame(255, 190, &MavMessage::PARAM_REQUEST_READ(req)),
            now,
        );
        let v = PARAM_VALUE_DATA {
            param_index: 7,
            ..Default::default()
        };
        let reply = frame(1, 1, &MavMessage::PARAM_VALUE(v));
        assert_eq!(t.classify(fc, &reply, now), Audience::Only(Arc::from([a])));
        assert_eq!(t.classify(fc, &reply, now), Audience::All);
    }

    #[test]
    fn snapshot_lists_routes_and_counters() {
        let mut t = RouteTable::default();
        let now = Instant::now();
        let (a, c) = t.endpoint("ipc");
        c.rx.fetch_add(2, Ordering::Relaxed);
        t.learn(a, &heartbeat(255, 190), now);
        let s = t.snapshot(now);
        assert_eq!(s["version"], 1);
        assert_eq!(s["routes"][0]["endpoint"], "ipc");
        assert_eq!(s["per_endpoint"]["ipc"]["rx"], 2);
    }
}
