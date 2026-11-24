//! Wiring: the MAVLink socket, the core task, publishing and persistence.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::pilot;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::config::{self, Settings};
use crate::core::{Out, Pilot, Reply};
use crate::ipc::{CoreReq, SetupHandler, Shared};

/// The pilot's MAVLink identity: the companion's system id (the FC's
/// `MAV_GCS_SYSID` = 1) and its own component, apart from the router's 191.
/// Its death is caught by the router (`pilot_link` death failsafe), not by
/// the FC's GCS failsafe, which any other sysid-1 heartbeat satisfies.
pub const SYSID: u8 = 1;
pub const COMPID: u8 = 196;
/// State publish period (10 Hz).
const STATE_PERIOD_MS: u64 = 100;

mod mav_link;
mod setup_view;
use mav_link::{mavlink_link, Lockstep};
pub use setup_view::SetupView;

/// FC traffic for the setup engine: a copy of every FC frame, and a way to send.
#[derive(Clone)]
pub struct FcLink {
    pub tx: mpsc::Sender<MavMessage>,
    pub frames: broadcast::Sender<(u8, u8, MavMessage)>,
    /// Raw AUTOPILOT_VERSION frames, for the fields the decoded message lacks:
    /// the workspace builds the dialect without MAVLink 2 message extensions,
    /// so `uid2` is dropped in decoding. Sent before the decoded message.
    pub version_frames: broadcast::Sender<Vec<u8>>,
}

/// Commands from the setup engine to the core.
pub enum CoreCmd {
    EnterTuning(oneshot::Sender<bool>),
    ExitTuning,
    SetControlMode(ados_protocol::pilot::ControlMode),
    SetRcAdosPosition(u8),
    /// Declare `pilot.rc_receiver` through the pilot's `rc_receiver` action.
    SetRcReceiver(ados_protocol::pilot::RcReceiver),
    /// `Some(need)`: setup incomplete (blocks arming); `None`: complete.
    SetupNeeded(Option<crate::setup::SetupNeed>),
    /// The setup engine's active step id, `None` without a run.
    SetupStep(Option<&'static str>),
    /// The setup-strings key of the air wizard's last non-operator abort
    /// while it is shown, `None` otherwise.
    SetupAbort(Option<String>),
    /// The setup engine is about to ask the FC for this custom mode.
    SetupMode(u32),
    /// What a setup take-off's supervised lift-off found.
    LiftoffOutcome(crate::airframe::LiftoffOutcome),
    /// The FC identity is known: its stored airframe record and the
    /// wizard's propeller size.
    AirframeLoad {
        uid: String,
        record: Option<crate::airframe::Record>,
        prop_in: Option<f32>,
    },
}

/// Handle the setup engine uses to take and give back the `tuning` owner.
#[derive(Clone)]
pub struct PilotHandle {
    pub cmd: mpsc::Sender<CoreCmd>,
    /// Operator sticks while the owner is `tuning`.
    pub tuning_axes: watch::Receiver<[f32; 4]>,
    /// The current owner (an operator action ending `tuning` shows here).
    pub owner: watch::Receiver<ados_protocol::pilot::Owner>,
    /// Readiness and link state, refreshed at the 10 Hz state publish.
    pub status: watch::Receiver<SetupView>,
}

impl PilotHandle {
    pub async fn enter_tuning(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(CoreCmd::EnterTuning(tx)).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub async fn exit_tuning(&self) {
        let _ = self.cmd.send(CoreCmd::ExitTuning).await;
    }

    pub async fn set_control_mode(&self, mode: ados_protocol::pilot::ControlMode) {
        let _ = self.cmd.send(CoreCmd::SetControlMode(mode)).await;
    }

    pub async fn set_rc_ados_position(&self, slot: u8) {
        let _ = self.cmd.send(CoreCmd::SetRcAdosPosition(slot)).await;
    }

    pub async fn set_rc_receiver(&self, rc: ados_protocol::pilot::RcReceiver) {
        let _ = self.cmd.send(CoreCmd::SetRcReceiver(rc)).await;
    }

    pub async fn setup_needed(&self, msg: Option<crate::setup::SetupNeed>) {
        let _ = self.cmd.send(CoreCmd::SetupNeeded(msg)).await;
    }

    pub async fn setup_step(&self, step: Option<&'static str>) {
        let _ = self.cmd.send(CoreCmd::SetupStep(step)).await;
    }

    pub async fn setup_abort(&self, key: Option<String>) {
        let _ = self.cmd.send(CoreCmd::SetupAbort(key)).await;
    }

    /// Note a mode the setup engine commands, before its DO_SET_MODE goes
    /// out, so the FC's change to it reads as the operator's, not a failsafe.
    pub async fn setup_mode(&self, mode: u32) {
        let _ = self.cmd.send(CoreCmd::SetupMode(mode)).await;
    }

    /// Hand a setup lift-off's outcome to airframe health.
    pub async fn liftoff_outcome(&self, o: crate::airframe::LiftoffOutcome) {
        let _ = self.cmd.send(CoreCmd::LiftoffOutcome(o)).await;
    }

    /// Hand the FC identity's stored airframe record to airframe health.
    pub async fn airframe_load(
        &self,
        uid: String,
        record: Option<crate::airframe::Record>,
        prop_in: Option<f32>,
    ) {
        let _ = self
            .cmd
            .send(CoreCmd::AirframeLoad {
                uid,
                record,
                prop_in,
            })
            .await;
    }
}

/// Everything `main` needs to start the daemon.
pub struct Daemon {
    pub fc_link: FcLink,
    pub handle: PilotHandle,
    fc_out_rx: mpsc::Receiver<MavMessage>,
    cmd_rx: mpsc::Receiver<CoreCmd>,
    axes_tx: watch::Sender<[f32; 4]>,
    owner_tx: watch::Sender<ados_protocol::pilot::Owner>,
    status_tx: watch::Sender<SetupView>,
    settings: Settings,
    config_path: PathBuf,
    /// ADOS time (wall, or the simulator's feed): the core's tick and stamps.
    pub clock: ados_time::Clock,
    /// The lockstep barrier's participants socket, under
    /// `simulation.clock_mode: lockstep` (`ados_time::lockstep_socket`).
    pub lockstep: Option<PathBuf>,
}

impl Daemon {
    pub fn new(settings: Settings, config_path: PathBuf) -> Self {
        let (fc_tx, fc_out_rx) = mpsc::channel(512);
        let (frames, _) = broadcast::channel(1024);
        let (version_frames, _) = broadcast::channel(8);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (axes_tx, axes_rx) = watch::channel([0.0; 4]);
        let (owner_tx, owner_rx) = watch::channel(ados_protocol::pilot::Owner::Idle);
        let (status_tx, status_rx) = watch::channel(SetupView::default());
        Self {
            fc_link: FcLink {
                tx: fc_tx,
                frames,
                version_frames,
            },
            handle: PilotHandle {
                cmd: cmd_tx,
                tuning_axes: axes_rx,
                owner: owner_rx,
                status: status_rx,
            },
            fc_out_rx,
            cmd_rx,
            axes_tx,
            owner_tx,
            status_tx,
            settings,
            config_path,
            clock: ados_time::Clock::wall(),
            lockstep: None,
        }
    }

    /// Run until a fatal error. `setup` serves the `setup.*` ops.
    pub async fn run(self, setup: Option<Arc<dyn SetupHandler>>) -> std::io::Result<()> {
        let run_dir = ados_protocol::paths::run_dir();
        let (req_tx, req_rx) = mpsc::channel::<CoreReq>(256);
        let (publish, _) = broadcast::channel::<Arc<Vec<u8>>>(256);
        let (feed_tx, feed_rx) = mpsc::channel(256);
        let shared = Shared {
            core: req_tx,
            publish: publish.clone(),
            setup,
        };
        let sock = run_dir.join(ados_protocol::paths::PILOT_SOCK);
        let server = tokio::spawn(async move { crate::ipc::serve(&sock, shared).await });
        tokio::spawn(crate::events::run(
            crate::events::socket_path(),
            feed_tx.clone(),
        ));
        tokio::spawn(crate::router_state::run(
            crate::router_state::socket_path(),
            feed_tx,
        ));
        let mav_path = run_dir.join(ados_protocol::paths::MAVLINK_SOCK);
        let (sync_tx, sync_rx) = mpsc::unbounded_channel();
        let lockstep = self.lockstep.map(|socket| Lockstep::new(socket, sync_tx));
        tokio::spawn(mavlink_link(
            mav_path,
            self.fc_out_rx,
            lockstep.is_some().then_some(sync_rx),
            self.fc_link.frames.clone(),
            self.fc_link.version_frames.clone(),
        ));
        let mut pilot = Pilot::new(self.settings);
        pilot.epoch_offset_ms = epoch_ms();
        // Scans are stamped on the host's CLOCK_MONOTONIC: FC attitudes are
        // logged on it to level each scan at its capture.
        pilot.mono_ns = ados_protocol::framebus::mono_ns;
        let vox = crate::world::VoxFeed::spawn(crate::world::VoxSource::default())?;
        let core = Core {
            clock: self.clock,
            lockstep,
            pilot,
            fc_tx: self.fc_link.tx.clone(),
            frames: self.fc_link.frames.subscribe(),
            publish,
            config_path: self.config_path,
            axes_tx: self.axes_tx,
            owner_tx: self.owner_tx,
            status_tx: self.status_tx,
            deferred: HashMap::new(),
            vox,
        };
        tokio::select! {
            r = server => r.unwrap_or_else(|e| Err(std::io::Error::other(e))),
            () = core.run(req_rx, feed_rx, self.cmd_rx) => Ok(()),
        }
    }
}

struct Core {
    pilot: Pilot,
    fc_tx: mpsc::Sender<MavMessage>,
    frames: broadcast::Receiver<(u8, u8, MavMessage)>,
    publish: broadcast::Sender<Arc<Vec<u8>>>,
    config_path: PathBuf,
    axes_tx: watch::Sender<[f32; 4]>,
    owner_tx: watch::Sender<ados_protocol::pilot::Owner>,
    status_tx: watch::Sender<SetupView>,
    /// Requests whose reply waits on the FC, by their [`Reply::Deferred`] token.
    deferred: HashMap<u64, oneshot::Sender<Reply>>,
    /// ADOS time, stamped on every state frame.
    clock: ados_time::Clock,
    /// The core's part in the simulator's lockstep barrier.
    lockstep: Option<Lockstep>,
    /// Navigation's voxel map (seam M), read off the loop on its own thread
    /// and swapped in before every tick.
    vox: crate::world::VoxFeed,
}

impl Core {
    async fn run(
        mut self,
        mut req_rx: mpsc::Receiver<CoreReq>,
        mut feed_rx: mpsc::Receiver<(String, serde_json::Value)>,
        mut cmd_rx: mpsc::Receiver<CoreCmd>,
    ) {
        let clock = self.clock.clone();
        let now = || clock.now_ms();
        let mut tick = clock.interval(Duration::from_millis(crate::core::TICK_MS));
        let mut state_ms = 0u64;
        loop {
            let mut ticked = false;
            tokio::select! {
                _ = tick.tick() => {
                    ticked = true;
                    let t = now();
                    self.pilot.sim_speed = clock.speed();
                    if let Some(mut snap) = self.vox.take() {
                        self.pilot.on_voxmap(&mut snap, t);
                        self.vox.recycle(snap);
                    }
                    self.pilot.tick(t);
                    let _ = self.axes_tx.send(self.pilot.tuning_axes);
                    if t.saturating_sub(state_ms) >= STATE_PERIOD_MS {
                        state_ms = t;
                        let mut s = self.pilot.state(t);
                        s.clock = Some(clock_stamp(&self.clock));
                        let view = SetupView {
                            readiness: s.readiness.clone(),
                            link_lost: self.pilot.link_lost(t),
                            place_class: s.place_class,
                            rc_receiver: self.pilot.rc_receiver(),
                            obstacle_margin_m: self.pilot.settings.pilot.effective_margin_m(),
                            supervised_liftoff: self.pilot.settings.pilot.supervised_liftoff,
                            liftoff_trim: self.pilot.airframe.trim(),
                            liftoff_hover: self.pilot.liftoff_hover(),
                            airframe: self.pilot.airframe.save(),
                        };
                        self.status_tx.send_if_modified(|v| {
                            let changed = *v != view;
                            *v = view;
                            changed
                        });
                        self.publish("state", &s);
                        self.pilot.state_published();
                    }
                    if self.pilot.obstacles_due(t) {
                        if let Some(o) = self.pilot.obstacles_frame(t) {
                            self.publish("obstacles", &o);
                        }
                    }
                }
                f = self.frames.recv() => match f {
                    Ok((sys, comp, msg)) => self.pilot.on_fc(sys, comp, &msg, now()),
                    Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!(skipped = n, "pilot_fc_frames_lagged"),
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                r = req_rx.recv() => match r {
                    Some((req, sid, reply)) => match self.pilot.on_request_from(req, sid.as_deref(), now()) {
                        Reply::Deferred(token) => { self.deferred.insert(token, reply); }
                        r => { let _ = reply.send(r); }
                    },
                    None => return,
                },
                f = feed_rx.recv() => if let Some((topic, payload)) = f {
                    self.pilot.on_feed(&topic, &payload, now());
                },
                c = cmd_rx.recv() => match c {
                    Some(CoreCmd::EnterTuning(tx)) => {
                        let ok = match self.pilot.auth.enter_tuning() {
                            Ok(revoked) => {
                                if let Some(r) = revoked { self.publish("lease.revoked", &r); }
                                true
                            }
                            Err(_) => false,
                        };
                        let _ = tx.send(ok);
                    }
                    Some(CoreCmd::ExitTuning) => self.pilot.exit_tuning(now()),
                    Some(CoreCmd::SetControlMode(m)) => {
                        self.setup_action(format!("control_mode:{}", m.as_str()), now());
                    }
                    Some(CoreCmd::SetRcReceiver(rc)) => {
                        self.setup_action(format!("rc_receiver:{}", rc.as_str()), now());
                    }
                    Some(CoreCmd::SetRcAdosPosition(n)) => self.pilot.set_rc_ados_position(n),
                    Some(CoreCmd::SetupNeeded(msg)) => self.pilot.setup_needed = Some(msg),
                    Some(CoreCmd::SetupStep(step)) => self.pilot.setup_step = step,
                    Some(CoreCmd::SetupAbort(key)) => self.pilot.setup_abort = key,
                    Some(CoreCmd::SetupMode(mode)) => self.pilot.note_setup_mode(mode, now()),
                    Some(CoreCmd::LiftoffOutcome(o)) => self.pilot.airframe.note_liftoff(&o, now()),
                    Some(CoreCmd::AirframeLoad { uid, record, prop_in }) => {
                        self.pilot.load_airframe(&uid, record, prop_in)
                    }
                    None => {}
                },
            }
            self.owner_tx.send_if_modified(|o| {
                let cur = self.pilot.owner();
                let changed = *o != cur;
                *o = cur;
                changed
            });
            self.flush().await;
            // Lockstep: done until the next tick once this tick's frames are
            // written (commands to the FC go out before the simulator steps).
            if let (true, Some(ls), Some(next)) = (ticked, &self.lockstep, tick.next_due_ns()) {
                ls.after_flush(next);
            }
        }
    }

    /// Apply a setup engine choice as the pilot action `name` (src `setup`);
    /// a refusal is logged by the action itself.
    fn setup_action(&mut self, name: String, now_ms: u64) {
        let req = ados_protocol::pilot::Request::Action(ados_protocol::pilot::ActionReq {
            name,
            src: "setup".into(),
            args: None,
            id: None,
        });
        let _ = self.pilot.on_request(req, now_ms);
    }

    fn publish<T: serde::Serialize>(&self, op: &str, body: &T) {
        match pilot::encode_op(op, body) {
            Ok(b) => {
                let _ = self.publish.send(Arc::new(b));
            }
            Err(e) => tracing::warn!(error = %e, op, "pilot_publish_encode_failed"),
        }
    }

    async fn flush(&mut self) {
        for o in self.pilot.drain() {
            match o {
                Out::Mav(m) => {
                    if self.fc_tx.try_send(*m).is_err() {
                        tracing::warn!("pilot_fc_queue_full");
                    }
                }
                Out::Revoked(r) => self.publish("lease.revoked", &r),
                Out::Answer(token, a) => {
                    if let Some(reply) = self.deferred.remove(&token) {
                        let _ = reply.send(Reply::Action(a));
                    }
                }
                Out::Sticks(s) => self.publish("sticks", &s),
                Out::GoalStatus(s) => self.publish("goal.status", &s),
                Out::Persist {
                    section,
                    key,
                    value,
                } => {
                    let path = self.config_path.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = config::persist(&path, section, key, &value) {
                            tracing::warn!(error = %e, section, key, "pilot_config_persist_failed");
                        }
                    });
                }
            }
        }
    }
}

/// The state frame's clock stamp: `wall`, or the sim time, lag and speed.
fn clock_stamp(c: &ados_time::Clock) -> ados_protocol::pilot::ClockStamp {
    ados_protocol::pilot::ClockStamp::new(c.source(), c.sim_ns(), c.lag_ms(), c.speed())
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
