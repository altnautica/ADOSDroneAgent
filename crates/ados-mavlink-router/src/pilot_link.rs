//! The router's side of the command authority (`pilot.v1`).
//!
//! While the control mode is `ados` and the pilot daemon is alive, the pilot
//! is the only producer of motion setpoints:
//!
//! * client `MANUAL_CONTROL` (69) and `RC_CHANNELS_OVERRIDE` (70) are dropped;
//! * client gotos (`DO_REPOSITION` as COMMAND_LONG/INT, and position
//!   `SET_POSITION_TARGET_LOCAL_NED` / `_GLOBAL_INT`) are redirected to the
//!   pilot as `goto`, so they fly through its planner and limits; the FC
//!   never sees a redirected `DO_REPOSITION`, so the router answers its
//!   sender with the `COMMAND_ACK` ([`PilotLink::reposition_ack`]);
//! * pilot death fails safe here: when the pilot's state stops for
//!   [`DEATH_MS`] while it was flying the vehicle, the router sets the FC
//!   mode for `pilot.link_loss_action` ([`PilotLink::death_mode`]). The
//!   FC's GCS failsafe cannot do it: any sysid-1 heartbeat counts for it (the
//!   router's own, a VIO component's), so the pilot's heartbeat stopping
//!   never trips it;
//! * the swarm loop submits its velocities as `swarm.goal`.
//!
//! The pilot's own MAVLink connection (injector `pilot`) is exempt. With the
//! pilot down, or in `fc_manual` / `elrs`, everything passes as before.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ados_protocol::pilot::{self, ControlMode};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

mod filter;
pub use filter::*;

/// The pilot counts as alive while its state arrived within this window.
pub const ALIVE_MS: u64 = 1500;
/// No pilot state for this long while it flew the vehicle: the router fails
/// safe on its behalf.
pub const DEATH_MS: u64 = 1000;
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
    /// What the last state said the pilot was flying: 0 nothing (not in
    /// `ados`, disarmed, landed, or the FC owns motion), else the vehicle
    /// class ([`CLASS_MULTIROTOR`], [`CLASS_FIXEDWING`], [`CLASS_VTOL`]).
    flying: AtomicU8,
    /// The death failsafe already fired for this silence.
    fired: AtomicBool,
    /// The last state lets the pilot start a goto: armed, in the air, not
    /// tuning (the conditions of its `goto` handler).
    goto_ok: AtomicBool,
}

const CLASS_MULTIROTOR: u8 = 1;
const CLASS_FIXEDWING: u8 = 2;
const CLASS_VTOL: u8 = 3;

/// `pilot.link_loss_action`: what the vehicle does when the pilot is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LossAction {
    #[default]
    Rth,
    Hold,
    Land,
}

impl LossAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "rth" => Some(Self::Rth),
            "hold" => Some(Self::Hold),
            "land" => Some(Self::Land),
            _ => None,
        }
    }
}

/// The ArduPilot custom mode for `action` on a vehicle of `class`: RTL; a
/// position hold (Copter LOITER, Plane LOITER, QuadPlane QLOITER); a landing
/// (Copter LAND, QuadPlane QLAND, and RTL for a plane, as the pilot's own
/// `land` does).
fn loss_mode(class: u8, action: LossAction) -> u32 {
    match (class, action) {
        (CLASS_FIXEDWING, LossAction::Rth | LossAction::Land) | (CLASS_VTOL, LossAction::Rth) => 11,
        (CLASS_FIXEDWING, LossAction::Hold) => 12,
        (CLASS_VTOL, LossAction::Hold) => 19,
        (CLASS_VTOL, LossAction::Land) => 20,
        (_, LossAction::Rth) => 6,
        (_, LossAction::Hold) => 5,
        (_, LossAction::Land) => 9,
    }
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
            flying: AtomicU8::new(0),
            fired: AtomicBool::new(false),
            goto_ok: AtomicBool::new(false),
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
        self.fired.store(false, Ordering::Relaxed);
        let field = |name: &str| match body {
            rmpv::Value::Map(m) => m
                .iter()
                .find(|(k, _)| k.as_str() == Some(name))
                .map(|(_, v)| v),
            _ => None,
        };
        let mode = field("control_mode")
            .and_then(rmpv::Value::as_str)
            .and_then(ControlMode::parse);
        if let Some(m) = mode {
            self.live_mode.store(mode_code(m), Ordering::Relaxed);
        }
        let flag = |name: &str| field(name).and_then(rmpv::Value::as_bool) == Some(true);
        let motion = matches!(
            field("owner").and_then(rmpv::Value::as_str),
            Some("pilot" | "skill" | "goto" | "swarm")
        );
        let class = match field("vehicle_class").and_then(rmpv::Value::as_str) {
            Some("fixedwing") => CLASS_FIXEDWING,
            Some("vtol") => CLASS_VTOL,
            _ => CLASS_MULTIROTOR,
        };
        let flying = mode == Some(ControlMode::Ados) && flag("armed") && !flag("landed") && motion;
        self.flying
            .store(if flying { class } else { 0 }, Ordering::Relaxed);
        let tuning = field("owner").and_then(rmpv::Value::as_str) == Some("tuning");
        self.goto_ok.store(
            flag("armed") && !flag("landed") && !tuning,
            Ordering::Relaxed,
        );
    }

    /// The FC mode to set because the pilot died while flying: its state
    /// stopped for [`DEATH_MS`] after one that had it flying the vehicle.
    /// Fires once per silence; `action` (read from config) only then.
    pub fn death_mode(&self, action: impl FnOnce() -> LossAction) -> Option<u32> {
        self.death_mode_at(self.now_ms(), action)
    }

    fn death_mode_at(&self, now_ms: u64, action: impl FnOnce() -> LossAction) -> Option<u32> {
        let t = self.alive_ms.load(Ordering::Relaxed);
        let class = self.flying.load(Ordering::Relaxed);
        if t == 0 || class == 0 || now_ms.saturating_sub(t) < DEATH_MS {
            return None;
        }
        if self.fired.swap(true, Ordering::Relaxed) {
            return None;
        }
        Some(loss_mode(class, action()))
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

    /// The `COMMAND_ACK` for a client `DO_REPOSITION` the filter took off the
    /// FC stream, as from the flight controller (`fc_sysid`, component 1), so
    /// the sender learns the outcome instead of retrying into a timeout:
    /// accepted when it went to a pilot that will fly it, temporarily
    /// rejected when the pilot cannot (disarmed, landed, tuning), failed when
    /// the target could not be placed.
    pub fn reposition_ack(&self, r: Reposition, fc_sysid: u8) -> Option<Vec<u8>> {
        use ados_protocol::mavlink::ardupilotmega::{
            MavCmd, MavMessage, MavResult, COMMAND_ACK_DATA,
        };
        let result = if !r.redirected {
            MavResult::MAV_RESULT_FAILED
        } else if self.goto_ok.load(Ordering::Relaxed) {
            MavResult::MAV_RESULT_ACCEPTED
        } else {
            MavResult::MAV_RESULT_TEMPORARILY_REJECTED
        };
        let header = ados_protocol::mavlink::MavHeader {
            system_id: fc_sysid,
            component_id: 1,
            sequence: 0,
        };
        let ack = MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
            command: MavCmd::MAV_CMD_DO_REPOSITION,
            result,
        });
        ados_protocol::mavlink::serialize_v2(header, &ack).ok()
    }

    pub fn send_swarm(&self, vel: [f32; 3], yaw: Option<f32>) {
        self.send(
            "swarm.goal",
            serde_json::json!({"vn": vel[0], "ve": vel[1], "vd": vel[2], "yaw": yaw}),
        );
    }
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

#[cfg(test)]
mod tests;
