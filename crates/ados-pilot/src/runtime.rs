//! Wiring: the MAVLink socket, the core task, publishing and persistence.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::mavlink::{self, ardupilotmega::MavMessage, MavHeader};
use ados_protocol::mavlink_socket;
use ados_protocol::pilot;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
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

/// What the setup engine watches: readiness and the surface link.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SetupView {
    pub readiness: Vec<ados_protocol::pilot::Readiness>,
    /// No operator surface heard from for `pilot.link_loss_s`.
    pub link_lost: bool,
}

/// Commands from the setup engine to the core.
pub enum CoreCmd {
    EnterTuning(oneshot::Sender<bool>),
    ExitTuning,
    SetControlMode(ados_protocol::pilot::ControlMode),
    SetRcAdosPosition(u8),
    /// `Some(msg)`: setup incomplete (blocks arming); `None`: complete.
    SetupNeeded(Option<String>),
    /// The setup engine's active step id, `None` without a run.
    SetupStep(Option<&'static str>),
    /// The setup engine is about to ask the FC for this custom mode.
    SetupMode(u32),
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

    pub async fn setup_needed(&self, msg: Option<String>) {
        let _ = self.cmd.send(CoreCmd::SetupNeeded(msg)).await;
    }

    pub async fn setup_step(&self, step: Option<&'static str>) {
        let _ = self.cmd.send(CoreCmd::SetupStep(step)).await;
    }

    /// Note a mode the setup engine commands, before its DO_SET_MODE goes
    /// out, so the FC's change to it reads as the operator's, not a failsafe.
    pub async fn setup_mode(&self, mode: u32) {
        let _ = self.cmd.send(CoreCmd::SetupMode(mode)).await;
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
        tokio::spawn(mavlink_link(
            mav_path,
            self.fc_out_rx,
            self.fc_link.frames.clone(),
            self.fc_link.version_frames.clone(),
        ));
        let mut pilot = Pilot::new(self.settings);
        pilot.epoch_offset_ms = epoch_ms();
        let core = Core {
            pilot,
            fc_tx: self.fc_link.tx.clone(),
            frames: self.fc_link.frames.subscribe(),
            publish,
            config_path: self.config_path,
            axes_tx: self.axes_tx,
            owner_tx: self.owner_tx,
            status_tx: self.status_tx,
            deferred: HashMap::new(),
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
}

impl Core {
    async fn run(
        mut self,
        mut req_rx: mpsc::Receiver<CoreReq>,
        mut feed_rx: mpsc::Receiver<(String, serde_json::Value)>,
        mut cmd_rx: mpsc::Receiver<CoreCmd>,
    ) {
        let start = Instant::now();
        let now = || start.elapsed().as_millis() as u64;
        let mut tick = tokio::time::interval(Duration::from_millis(crate::core::TICK_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut state_ms = 0u64;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let t = now();
                    self.pilot.tick(t);
                    let _ = self.axes_tx.send(self.pilot.tuning_axes);
                    if t.saturating_sub(state_ms) >= STATE_PERIOD_MS {
                        state_ms = t;
                        let s = self.pilot.state(t);
                        let view = SetupView { readiness: s.readiness.clone(), link_lost: self.pilot.link_lost(t) };
                        self.status_tx.send_if_modified(|v| {
                            let changed = *v != view;
                            *v = view;
                            changed
                        });
                        self.publish("state", &s);
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
                    Some((req, reply)) => match self.pilot.on_request(req, now()) {
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
                        let name = format!("control_mode:{}", m.as_str());
                        let _ = self.pilot.on_request(
                            ados_protocol::pilot::Request::Action(ados_protocol::pilot::ActionReq {
                                name, src: "setup".into(), args: None, id: None,
                            }),
                            now(),
                        );
                    }
                    Some(CoreCmd::SetRcAdosPosition(n)) => self.pilot.set_rc_ados_position(n),
                    Some(CoreCmd::SetupNeeded(msg)) => self.pilot.setup_needed = Some(msg),
                    Some(CoreCmd::SetupStep(step)) => self.pilot.setup_step = step,
                    Some(CoreCmd::SetupMode(mode)) => self.pilot.note_setup_mode(mode, now()),
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
        }
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

/// Keep a MAVLink socket session alive: read FC frames into `frames`, write
/// queued frames with the pilot's identity.
async fn mavlink_link(
    path: PathBuf,
    mut out_rx: mpsc::Receiver<MavMessage>,
    frames: broadcast::Sender<(u8, u8, MavMessage)>,
    version_frames: broadcast::Sender<Vec<u8>>,
) {
    let mut seq: u8 = 0;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                tracing::info!(path = %path.display(), "pilot_mavlink_connected");
                let (mut rd, mut wr) = stream.into_split();
                let decl = ados_protocol::ipc::encode_injector_declaration("pilot", None);
                let ok = match ados_protocol::frame::encode_frame(
                    &decl,
                    ados_protocol::frame::MAVLINK_MAX_FRAME,
                ) {
                    Ok(f) => wr.write_all(&f).await.is_ok(),
                    Err(_) => false,
                };
                if ok {
                    let frames = frames.clone();
                    let version_frames = version_frames.clone();
                    let reader = tokio::spawn(async move {
                        while let Ok(Some(rec)) = mavlink_socket::read_frame(&mut rd).await {
                            if let Ok((h, msg)) = mavlink::parse_any(&rec.frame) {
                                if h.system_id == SYSID && h.component_id == COMPID {
                                    continue;
                                }
                                if matches!(msg, MavMessage::AUTOPILOT_VERSION(_)) {
                                    let _ = version_frames.send(rec.frame.clone());
                                }
                                let _ = frames.send((h.system_id, h.component_id, msg));
                            }
                        }
                    });
                    while let Some(m) = out_rx.recv().await {
                        let header = MavHeader {
                            system_id: SYSID,
                            component_id: COMPID,
                            sequence: seq,
                        };
                        seq = seq.wrapping_add(1);
                        let Ok(bytes) = mavlink::serialize_v2(header, &m) else {
                            continue;
                        };
                        let Ok(f) = ados_protocol::frame::encode_frame(
                            &bytes,
                            ados_protocol::frame::MAVLINK_MAX_FRAME,
                        ) else {
                            continue;
                        };
                        if wr.write_all(&f).await.is_err() || reader.is_finished() {
                            break;
                        }
                    }
                    if out_rx.is_closed() {
                        reader.abort();
                        return;
                    }
                    reader.abort();
                }
            }
            Err(e) => tracing::debug!(error = %e, "pilot_mavlink_connect_failed"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
