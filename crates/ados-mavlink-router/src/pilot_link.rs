//! The router's side of the command authority (`pilot.v1`).
//!
//! While the control mode is `ados` and the pilot daemon is alive, the pilot
//! is the only producer of motion setpoints:
//!
//! * client `MANUAL_CONTROL` (69) and `RC_CHANNELS_OVERRIDE` (70) are dropped;
//! * client gotos (`DO_REPOSITION` as COMMAND_LONG/INT, and position
//!   `SET_POSITION_TARGET_LOCAL_NED` / `_GLOBAL_INT`) are redirected to the
//!   pilot as `goto`, so they fly through its planner and limits;
//! * the router's own 1 Hz heartbeat stops, because the pilot sends the
//!   heartbeat the FC's GCS failsafe watches: if the pilot dies, that heartbeat
//!   stops and the FC fails safe;
//! * the swarm loop submits its velocities as `swarm.goal`.
//!
//! The pilot's own MAVLink connection (injector `pilot`) is exempt. With the
//! pilot down, or in `fc_manual` / `elrs`, everything passes as before.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage};
use ados_protocol::pilot::{self, ControlMode};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// The pilot counts as alive while its state arrived within this window.
pub const ALIVE_MS: u64 = 1500;
/// The injector client id the pilot daemon declares on `mavlink.sock`.
pub const PILOT_INJECTOR: &str = "pilot";

const MSG_MANUAL_CONTROL: u32 = 69;
const MSG_RC_CHANNELS_OVERRIDE: u32 = 70;

/// The pilot state goes down the aux link this often.
pub const DOWNLINK_PERIOD_MS: u64 = 500;

/// Who the downlinked state is from, and the lane it leaves on.
pub struct Downlink {
    pub egress: std::sync::Arc<ados_protocol::aux_egress::AuxEgress>,
    /// The flight controller's system id, once learned.
    pub fc_sysid: Box<dyn Fn() -> Option<u8> + Send + Sync>,
}

pub struct PilotLink {
    started: Instant,
    /// Milliseconds since `started` of the last pilot `state` (0 = never).
    alive_ms: AtomicU64,
    cfg_mode: AtomicU8,
    live_mode: AtomicU8,
    tx: mpsc::Sender<Vec<u8>>,
    pub dropped: AtomicU64,
    pub redirected: AtomicU64,
    /// Ground-station frames forwarded to the pilot.
    pub relayed: AtomicU64,
    downlink: OnceLock<Downlink>,
    downlink_ms: AtomicU64,
}

fn mode_code(m: ControlMode) -> u8 {
    match m {
        ControlMode::Ados => 1,
        ControlMode::FcManual => 2,
        ControlMode::Elrs => 3,
    }
}

fn mode_of(code: u8) -> Option<ControlMode> {
    match code {
        1 => Some(ControlMode::Ados),
        2 => Some(ControlMode::FcManual),
        3 => Some(ControlMode::Elrs),
        _ => None,
    }
}

static LINK: OnceLock<PilotLink> = OnceLock::new();

/// The process-wide link, once [`start`] ran.
pub fn global() -> Option<&'static PilotLink> {
    LINK.get()
}

/// Start the link (drone profile). `cfg_mode` is `control.mode` from config.
pub fn start(cfg_mode: ControlMode, path: PathBuf) {
    let (tx, rx) = mpsc::channel(256);
    let link = PilotLink::new(cfg_mode, tx);
    if LINK.set(link).is_ok() {
        tokio::spawn(run(path, rx));
    }
}

impl PilotLink {
    pub fn new(cfg_mode: ControlMode, tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            started: Instant::now(),
            alive_ms: AtomicU64::new(0),
            cfg_mode: AtomicU8::new(mode_code(cfg_mode)),
            live_mode: AtomicU8::new(0),
            tx,
            dropped: AtomicU64::new(0),
            redirected: AtomicU64::new(0),
            relayed: AtomicU64::new(0),
            downlink: OnceLock::new(),
            downlink_ms: AtomicU64::new(0),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64 + 1
    }

    pub fn alive(&self) -> bool {
        let t = self.alive_ms.load(Ordering::Relaxed);
        t != 0 && self.now_ms().saturating_sub(t) <= ALIVE_MS
    }

    /// The live mode from the pilot's state, else the configured one.
    pub fn mode(&self) -> ControlMode {
        mode_of(self.live_mode.load(Ordering::Relaxed))
            .or_else(|| mode_of(self.cfg_mode.load(Ordering::Relaxed)))
            .unwrap_or_default()
    }

    /// True when the pilot owns motion: `ados` mode and the pilot alive.
    pub fn gating(&self) -> bool {
        self.mode() == ControlMode::Ados && self.alive()
    }

    /// Record a `state` body from the pilot.
    pub fn on_state(&self, body: &rmpv::Value) {
        self.alive_ms.store(self.now_ms(), Ordering::Relaxed);
        let mode = match body {
            rmpv::Value::Map(m) => m
                .iter()
                .find(|(k, _)| k.as_str() == Some("control_mode"))
                .and_then(|(_, v)| v.as_str())
                .and_then(ControlMode::parse),
            _ => None,
        };
        if let Some(m) = mode {
            self.live_mode.store(mode_code(m), Ordering::Relaxed);
        }
    }

    fn send(&self, op: &str, body: serde_json::Value) {
        match pilot::encode_op(op, &body) {
            Ok(b) => {
                if self.tx.try_send(b).is_err() {
                    tracing::debug!(op, "pilot_link_queue_full");
                }
            }
            Err(e) => tracing::warn!(error = %e, op, "pilot_link_encode_failed"),
        }
    }

    /// Forward one `pilot.v1` request body from a ground station.
    pub fn forward(&self, body: Vec<u8>) {
        self.relayed.fetch_add(1, Ordering::Relaxed);
        if self.tx.try_send(body).is_err() {
            tracing::debug!("pilot_link_queue_full");
        }
    }

    /// Send the pilot state down the aux link (drone profile).
    pub fn set_downlink(&self, d: Downlink) {
        let _ = self.downlink.set(d);
    }

    /// The state body to downlink, when one is due.
    fn downlink_due(
        &self,
        body: &rmpv::Value,
    ) -> Option<(
        std::sync::Arc<ados_protocol::aux_egress::AuxEgress>,
        Vec<u8>,
    )> {
        let d = self.downlink.get()?;
        let now = self.now_ms();
        if now.saturating_sub(self.downlink_ms.load(Ordering::Relaxed)) < DOWNLINK_PERIOD_MS {
            return None;
        }
        self.downlink_ms.store(now, Ordering::Relaxed);
        let mut payload = vec![(d.fc_sysid)().unwrap_or(0)];
        payload.extend(downlink_body(body)?);
        Some((std::sync::Arc::clone(&d.egress), payload))
    }

    pub fn send_goto(&self, ned: [f32; 3], yaw: Option<f32>) {
        self.redirected.fetch_add(1, Ordering::Relaxed);
        self.send(
            "goto",
            serde_json::json!({"n": ned[0], "e": ned[1], "d": ned[2], "yaw": yaw}),
        );
    }

    pub fn send_swarm(&self, vel: [f32; 3], yaw: Option<f32>) {
        self.send(
            "swarm.goal",
            serde_json::json!({"vn": vel[0], "ve": vel[1], "vd": vel[2], "yaw": yaw}),
        );
    }
}

/// True when the router should skip its own heartbeat.
pub fn suppress_router_heartbeat() -> bool {
    global().is_some_and(PilotLink::gating)
}

async fn run(path: PathBuf, mut rx: mpsc::Receiver<Vec<u8>>) {
    loop {
        if let Ok(stream) = UnixStream::connect(&path).await {
            tracing::info!("pilot_link_connected");
            let (mut rd, mut wr) = stream.into_split();
            let sub = pilot::encode_op("subscribe", &()).unwrap_or_default();
            if pilot::write_msg(&mut wr, &sub).await.is_ok() {
                let reader = tokio::spawn(async move {
                    while let Ok(Some(body)) = pilot::read_msg(&mut rd).await {
                        if let (Ok((op, v)), Some(link)) = (pilot::peek_op(&body), global()) {
                            if op == "state" {
                                link.on_state(&v);
                                if let Some((egress, payload)) = link.downlink_due(&v) {
                                    tokio::spawn(async move {
                                        let ch = ados_protocol::aux_mux::AuxChannel::Pilot;
                                        if let Err(e) = egress.send(ch, &payload).await {
                                            tracing::debug!(error = %e, "pilot_state_downlink_failed");
                                        }
                                    });
                                }
                            }
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
        // Drain stale requests while the pilot is away: a goto must not fly
        // seconds later.
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The `state` body trimmed to fit one aux frame: the plan polyline goes
/// first, then the readiness items that pass, then every readiness item.
pub fn downlink_body(state: &rmpv::Value) -> Option<Vec<u8>> {
    let max = ados_protocol::aux_mux::AUX_MAX_PAYLOAD - 1;
    let rmpv::Value::Map(map) = state else {
        return None;
    };
    let encode = |m: &Vec<(rmpv::Value, rmpv::Value)>| {
        let mut out = Vec::new();
        rmpv::encode::write_value(&mut out, &rmpv::Value::Map(m.clone()))
            .ok()
            .map(|_| out)
    };
    let mut m = map.clone();
    let mut out = encode(&m)?;
    let set = |m: &mut Vec<(rmpv::Value, rmpv::Value)>,
               key: &str,
               f: &dyn Fn(&rmpv::Value) -> rmpv::Value| {
        for (k, v) in m.iter_mut() {
            if k.as_str() == Some(key) {
                *v = f(v);
            }
        }
    };
    if out.len() > max {
        set(&mut m, "plan_ned", &|_| rmpv::Value::Array(Vec::new()));
        out = encode(&m)?;
    }
    if out.len() > max {
        set(&mut m, "readiness", &|v| {
            let items = v.as_array().cloned().unwrap_or_default();
            rmpv::Value::Array(
                items
                    .into_iter()
                    .filter(|i| {
                        i.as_map()
                            .and_then(|e| e.iter().find(|(k, _)| k.as_str() == Some("ok")))
                            .and_then(|(_, v)| v.as_bool())
                            == Some(false)
                    })
                    .collect(),
            )
        });
        out = encode(&m)?;
    }
    if out.len() > max {
        set(&mut m, "readiness", &|_| rmpv::Value::Array(Vec::new()));
        out = encode(&m)?;
    }
    (out.len() <= max).then_some(out)
}

/// The vehicle's current global and local position, to turn a global target
/// into the FC's local frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoRef {
    pub lat: f64,
    pub lon: f64,
    /// Altitude above home, metres.
    pub alt_rel: f64,
    /// Local NED position at the same moment.
    pub local: [f64; 3],
}

impl GeoRef {
    /// Local NED of a global point (`alt_rel` above home).
    pub fn to_local(&self, lat: f64, lon: f64, alt_rel: f64) -> [f32; 3] {
        const R: f64 = 6_378_137.0;
        let dn = (lat - self.lat).to_radians() * R;
        let de = (lon - self.lon).to_radians() * R * self.lat.to_radians().cos();
        let dd = -(alt_rel - self.alt_rel);
        [
            (self.local[0] + dn) as f32,
            (self.local[1] + de) as f32,
            (self.local[2] + dd) as f32,
        ]
    }
}

/// What happens to one client frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Pass,
    Drop,
    Goto([f32; 3], Option<f32>),
}

const POS_IGNORE: u16 = 0b111;
const YAW_IGNORE: u16 = 1 << 10;

/// Classify one decoded client frame while the pilot owns motion.
pub fn classify(msg_id: u32, msg: Option<&MavMessage>, geo: Option<GeoRef>) -> Verdict {
    if msg_id == MSG_MANUAL_CONTROL || msg_id == MSG_RC_CHANNELS_OVERRIDE {
        return Verdict::Drop;
    }
    let Some(msg) = msg else { return Verdict::Pass };
    let global = |lat: f64, lon: f64, alt: f64| geo.map(|g| g.to_local(lat, lon, alt));
    let target = match msg {
        MavMessage::COMMAND_LONG(c) if c.command == MavCmd::MAV_CMD_DO_REPOSITION => Some(
            global(
                f64::from(c.param5),
                f64::from(c.param6),
                f64::from(c.param7),
            )
            .map(|p| (p, yaw_param(c.param4))),
        ),
        MavMessage::COMMAND_INT(c) if c.command == MavCmd::MAV_CMD_DO_REPOSITION => Some(
            global(f64::from(c.x) / 1e7, f64::from(c.y) / 1e7, f64::from(c.z))
                .map(|p| (p, yaw_param(c.param4))),
        ),
        MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => {
            let mask = s.type_mask.bits();
            if mask & POS_IGNORE != 0 {
                return Verdict::Drop;
            }
            Some(Some((
                [s.x, s.y, s.z],
                (mask & YAW_IGNORE == 0).then_some(s.yaw),
            )))
        }
        MavMessage::SET_POSITION_TARGET_GLOBAL_INT(s) => {
            let mask = s.type_mask.bits();
            if mask & POS_IGNORE != 0 {
                return Verdict::Drop;
            }
            let p = global(
                f64::from(s.lat_int) / 1e7,
                f64::from(s.lon_int) / 1e7,
                f64::from(s.alt),
            );
            Some(p.map(|p| (p, (mask & YAW_IGNORE == 0).then_some(s.yaw))))
        }
        _ => None,
    };
    match target {
        None => Verdict::Pass,
        Some(Some((p, yaw))) if p.iter().all(|v| v.is_finite()) => Verdict::Goto(p, yaw),
        Some(_) => Verdict::Drop,
    }
}

fn yaw_param(p4: f32) -> Option<f32> {
    p4.is_finite().then(|| p4.to_radians())
}

/// Total length of the frame at `buf[0]`, if its header is complete.
fn frame_len(buf: &[u8]) -> Option<usize> {
    match buf.first()? {
        0xFD if buf.len() >= 3 => Some(12 + buf[1] as usize + if buf[2] & 1 != 0 { 13 } else { 0 }),
        0xFE if buf.len() >= 2 => Some(8 + buf[1] as usize),
        _ => None,
    }
}

fn frame_id(f: &[u8]) -> u32 {
    if f[0] == 0xFD {
        u32::from_le_bytes([f[7], f[8], f[9], 0])
    } else {
        u32::from(f[5])
    }
}

/// Filter a client write: the bytes to forward, and the gotos to redirect.
/// Bytes that are not whole frames pass through untouched.
/// A redirected goto: local NED and optional yaw (radians).
pub type Redirect = ([f32; 3], Option<f32>);

pub fn filter(data: &[u8], geo: Option<GeoRef>) -> (Vec<u8>, Vec<Redirect>, u64) {
    let mut kept = Vec::with_capacity(data.len());
    let mut gotos = Vec::new();
    let mut dropped = 0;
    let mut i = 0;
    while i < data.len() {
        let rest = &data[i..];
        let Some(n) = frame_len(rest).filter(|&n| n <= rest.len() && n >= 8) else {
            kept.extend_from_slice(rest);
            break;
        };
        let frame = &rest[..n];
        let decoded = ados_protocol::mavlink::parse_any(frame)
            .ok()
            .map(|(_, m)| m);
        match classify(frame_id(frame), decoded.as_ref(), geo) {
            Verdict::Pass => kept.extend_from_slice(frame),
            Verdict::Drop => dropped += 1,
            Verdict::Goto(p, yaw) => gotos.push((p, yaw)),
        }
        i += n;
    }
    (kept, gotos, dropped)
}

#[cfg(test)]
mod tests;
