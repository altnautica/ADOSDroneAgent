//! The plugin host's link to the ADOS pilot (`pilot.v1` on `pilot.sock`).
//!
//! Skills command motion through the pilot's single motion lease. The host
//! stamps the plugin id on every lease, forwards goals only from the holder,
//! delivers the pilot's `sticks`, `goal.status` and `lease.revoked` messages
//! as the bus topics [`STICKS_TOPIC`], [`GOAL_STATUS_TOPIC`] and
//! [`REVOKED_TOPIC`] to the holder alone, and releases a plugin's lease when
//! the plugin stops.
//!
//! Older plugins that still send `flight.guided_setpoint.send` get an implicit
//! lease on their first setpoint; each setpoint becomes a `position` goal, and
//! the lease is released after [`LEGACY_IDLE`] without one.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ados_protocol::pilot;
use rmpv::Value;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

use crate::handlers::EventBus;

/// Bus topic carrying the operator's shaped sticks to the lease holder.
pub const STICKS_TOPIC: &str = "flight.sticks";
/// Bus topic telling the holder its lease ended.
pub const REVOKED_TOPIC: &str = "flight.lease.revoked";
/// Bus topic carrying the pilot's progress toward a vantage goal to the
/// holder.
pub const GOAL_STATUS_TOPIC: &str = "flight.goal.status";
/// Skill id of the implicit lease a legacy setpoint stream takes.
pub const LEGACY_SKILL: &str = "legacy-setpoint";
/// A legacy implicit lease is released after this long without a setpoint.
pub const LEGACY_IDLE: Duration = Duration::from_secs(1);
/// The pilot counts as reachable while its state arrived within this window.
const ALIVE: Duration = Duration::from_millis(1500);
/// How long a `lease.acquire` waits for the pilot's reply.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Leases {
    /// lease id → plugin id.
    holder: HashMap<u64, String>,
    /// Plugins that asked for `flight.sticks`.
    sticks: HashSet<String>,
    /// plugin id → (implicit lease id, last setpoint).
    legacy: HashMap<String, (u64, Instant)>,
}

#[derive(Default)]
struct Link {
    connected: bool,
    last_state: Option<Instant>,
    pending_acquire: Option<oneshot::Sender<Value>>,
}

pub struct PilotClient {
    tx: mpsc::Sender<Vec<u8>>,
    leases: Mutex<Leases>,
    link: Mutex<Link>,
    /// Serialises `lease.acquire` round trips (the pilot answers in order).
    acquire: tokio::sync::Mutex<()>,
    bus: Arc<EventBus>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    )
}

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// A lease id given as an integer or a decimal string.
pub fn lease_id_of(args: &Value) -> Option<u64> {
    match get(args, "lease_id")? {
        Value::String(s) => s.as_str()?.trim().parse().ok(),
        v => v.as_u64(),
    }
}

impl PilotClient {
    /// Connect (and keep reconnecting) to `path`; events go out on `bus`.
    pub fn spawn(path: PathBuf, bus: Arc<EventBus>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(256);
        let me = Arc::new(Self {
            tx,
            leases: Mutex::new(Leases::default()),
            link: Mutex::new(Link::default()),
            acquire: tokio::sync::Mutex::new(()),
            bus,
        });
        tokio::spawn(Arc::clone(&me).run(path, rx));
        tokio::spawn(Arc::clone(&me).legacy_reaper());
        me
    }

    /// True when the pilot is up and publishing state.
    pub fn reachable(&self) -> bool {
        let l = lock(&self.link);
        l.connected && l.last_state.is_some_and(|t| t.elapsed() <= ALIVE)
    }

    fn send(&self, op: &str, body: &Value) -> bool {
        match pilot::encode_op(op, body) {
            Ok(b) => self.tx.try_send(b).is_ok(),
            Err(e) => {
                tracing::warn!(error = %e, op, "pilot_client_encode_failed");
                false
            }
        }
    }

    /// `flight.lease.acquire`: ask the pilot for the motion lease.
    pub async fn acquire(&self, plugin_id: &str, skill_id: &str, kind: &str) -> Value {
        if !self.reachable() {
            return map(vec![
                ("ok", Value::from(false)),
                ("err", Value::from("pilot_unavailable")),
            ]);
        }
        let _one = self.acquire.lock().await;
        let (tx, rx) = oneshot::channel();
        lock(&self.link).pending_acquire = Some(tx);
        let holder = map(vec![
            ("plugin_id", Value::from(plugin_id)),
            ("skill_id", Value::from(skill_id)),
        ]);
        if !self.send(
            "lease.acquire",
            &map(vec![("holder", holder), ("kind", Value::from(kind))]),
        ) {
            return map(vec![
                ("ok", Value::from(false)),
                ("err", Value::from("pilot_unavailable")),
            ]);
        }
        let reply = match tokio::time::timeout(ACQUIRE_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            _ => {
                return map(vec![
                    ("ok", Value::from(false)),
                    ("err", Value::from("pilot_timeout")),
                ])
            }
        };
        if let Some(id) = get(&reply, "lease_id").and_then(Value::as_u64) {
            lock(&self.leases).holder.insert(id, plugin_id.to_owned());
        }
        // The op key is the wire envelope, not part of the reply.
        match reply {
            Value::Map(m) => Value::Map(
                m.into_iter()
                    .filter(|(k, _)| k.as_str() != Some("op"))
                    .collect(),
            ),
            other => other,
        }
    }

    fn holds(&self, plugin_id: &str, lease_id: u64) -> bool {
        lock(&self.leases)
            .holder
            .get(&lease_id)
            .is_some_and(|p| p == plugin_id)
    }

    /// `flight.lease.release`.
    pub fn release(&self, plugin_id: &str, lease_id: u64) -> Value {
        if !self.holds(plugin_id, lease_id) {
            return map(vec![
                ("ok", Value::from(false)),
                ("err", Value::from("not_holder")),
            ]);
        }
        lock(&self.leases).holder.remove(&lease_id);
        let ok = self.send(
            "lease.release",
            &map(vec![("lease_id", Value::from(lease_id))]),
        );
        map(vec![("ok", Value::from(ok))])
    }

    /// `flight.goal.send`.
    pub fn goal(&self, plugin_id: &str, lease_id: u64, goal: Value) -> Value {
        if !self.holds(plugin_id, lease_id) {
            return map(vec![
                ("ok", Value::from(false)),
                ("err", Value::from("not_holder")),
            ]);
        }
        let ok = self.send(
            "goal",
            &map(vec![("lease_id", Value::from(lease_id)), ("goal", goal)]),
        );
        map(vec![("ok", Value::from(ok))])
    }

    /// `flight.sticks.subscribe`.
    pub fn subscribe_sticks(&self, plugin_id: &str) -> Value {
        lock(&self.leases).sticks.insert(plugin_id.to_owned());
        map(vec![("ok", Value::from(true))])
    }

    /// A plugin stopped: give back every lease it holds.
    pub fn release_plugin(&self, plugin_id: &str) {
        let ids: Vec<u64> = {
            let mut l = lock(&self.leases);
            l.sticks.remove(plugin_id);
            l.legacy.remove(plugin_id);
            let ids = l
                .holder
                .iter()
                .filter(|(_, p)| *p == plugin_id)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            for id in &ids {
                l.holder.remove(id);
            }
            ids
        };
        for id in ids {
            self.send("lease.release", &map(vec![("lease_id", Value::from(id))]));
        }
    }

    /// A legacy setpoint as a `position` goal under an implicit lease.
    /// `false` when no lease could be had (the caller keeps its old path).
    pub async fn legacy_goal(&self, plugin_id: &str, goal: Value) -> bool {
        let existing = lock(&self.leases).legacy.get(plugin_id).map(|(id, _)| *id);
        let id = match existing {
            Some(id) if self.holds(plugin_id, id) => id,
            _ => {
                let reply = self.acquire(plugin_id, LEGACY_SKILL, "motion").await;
                let Some(id) = get(&reply, "lease_id").and_then(Value::as_u64) else {
                    return false;
                };
                id
            }
        };
        lock(&self.leases)
            .legacy
            .insert(plugin_id.to_owned(), (id, Instant::now()));
        self.send(
            "goal",
            &map(vec![("lease_id", Value::from(id)), ("goal", goal)]),
        )
    }

    async fn legacy_reaper(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            let stale: Vec<(String, u64)> = {
                let mut l = lock(&self.leases);
                let stale: Vec<_> = l
                    .legacy
                    .iter()
                    .filter(|(_, (_, t))| t.elapsed() >= LEGACY_IDLE)
                    .map(|(p, (id, _))| (p.clone(), *id))
                    .collect();
                for (p, id) in &stale {
                    l.legacy.remove(p);
                    l.holder.remove(id);
                }
                stale
            };
            for (_, id) in stale {
                self.send("lease.release", &map(vec![("lease_id", Value::from(id))]));
            }
        }
    }

    /// Handle one message from the pilot.
    fn on_message(&self, op: &str, body: Value) {
        match op {
            "state" => lock(&self.link).last_state = Some(Instant::now()),
            "lease.acquire" => {
                if let Some(tx) = lock(&self.link).pending_acquire.take() {
                    let _ = tx.send(body);
                }
            }
            "sticks" => {
                let Some(id) = get(&body, "lease_id").and_then(Value::as_u64) else {
                    return;
                };
                let target = {
                    let l = lock(&self.leases);
                    l.holder.get(&id).filter(|p| l.sticks.contains(*p)).cloned()
                };
                if let Some(plugin) = target {
                    let axes = get(&body, "axes").cloned().unwrap_or(Value::Nil);
                    self.publish(STICKS_TOPIC, &plugin, id, vec![("axes", axes)]);
                }
            }
            "goal.status" => {
                let Some(id) = get(&body, "lease_id").and_then(Value::as_u64) else {
                    return;
                };
                let target = lock(&self.leases).holder.get(&id).cloned();
                if let Some(plugin) = target {
                    let fields = body
                        .as_map()
                        .into_iter()
                        .flatten()
                        .filter_map(|(k, v)| Some((k.as_str()?, v.clone())))
                        .filter(|(k, _)| !matches!(*k, "op" | "lease_id" | "plugin_id"))
                        .collect();
                    self.publish(GOAL_STATUS_TOPIC, &plugin, id, fields);
                }
            }
            "lease.revoked" => {
                let Some(id) = get(&body, "lease_id").and_then(Value::as_u64) else {
                    return;
                };
                let plugin = {
                    let mut l = lock(&self.leases);
                    let p = l.holder.remove(&id);
                    if let Some(p) = &p {
                        if l.legacy.get(p).is_some_and(|(lid, _)| *lid == id) {
                            l.legacy.remove(p);
                        }
                    }
                    p
                };
                if let Some(plugin) = plugin {
                    let reason = get(&body, "reason").cloned().unwrap_or(Value::from(""));
                    self.publish(REVOKED_TOPIC, &plugin, id, vec![("reason", reason)]);
                }
            }
            _ => {}
        }
    }

    fn publish(&self, topic: &str, plugin_id: &str, lease_id: u64, extra: Vec<(&str, Value)>) {
        let mut entries = vec![
            ("lease_id", Value::from(lease_id)),
            ("plugin_id", Value::from(plugin_id)),
        ];
        entries.extend(extra);
        crate::vehicle_events::publish_host_event(&self.bus, topic, map(entries));
    }

    async fn run(self: Arc<Self>, path: PathBuf, mut rx: mpsc::Receiver<Vec<u8>>) {
        loop {
            if let Ok(stream) = UnixStream::connect(&path).await {
                let (mut rd, mut wr) = stream.into_split();
                let sub = pilot::encode_op("subscribe", &()).unwrap_or_default();
                if pilot::write_msg(&mut wr, &sub).await.is_ok() {
                    lock(&self.link).connected = true;
                    tracing::info!("plugin_host_pilot_connected");
                    let me = Arc::clone(&self);
                    let reader = tokio::spawn(async move {
                        while let Ok(Some(body)) = pilot::read_msg(&mut rd).await {
                            if let Ok((op, v)) = pilot::peek_op(&body) {
                                me.on_message(&op, v);
                            }
                        }
                    });
                    loop {
                        tokio::select! {
                            m = rx.recv() => {
                                let Some(m) = m else { reader.abort(); return };
                                if pilot::write_msg(&mut wr, &m).await.is_err() { break; }
                            }
                            () = tokio::time::sleep(Duration::from_millis(500)) => {
                                if reader.is_finished() { break; }
                            }
                        }
                    }
                    reader.abort();
                }
            }
            {
                let mut l = lock(&self.link);
                l.connected = false;
                l.pending_acquire = None;
            }
            // The pilot restarted or is down: every lease it held is gone.
            let held: Vec<(u64, String)> = lock(&self.leases).holder.drain().collect();
            lock(&self.leases).legacy.clear();
            for (id, plugin) in held {
                self.publish(
                    REVOKED_TOPIC,
                    &plugin,
                    id,
                    vec![("reason", Value::from("pilot-restart"))],
                );
            }
            while rx.try_recv().is_ok() {}
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Whether `event` (a `flight.*` host event) may reach `subscriber_id`: only
/// the plugin the host stamped on it.
pub fn flight_event_for(subscriber_id: &str, payload: &Value) -> bool {
    get(payload, "plugin_id").and_then(Value::as_str) == Some(subscriber_id)
}

#[cfg(test)]
mod tests;
