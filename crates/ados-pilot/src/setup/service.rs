//! Wires the setup [`Engine`] into the pilot daemon: FC frames from the
//! pilot's MAVLink link, a 50 Hz tick (the AUTOTUNE RC proxy streams
//! MANUAL_CONTROL at 50 Hz), the `setup.*` ops on `pilot.sock`, the tuning
//! owner and the `setup_needed` readiness item.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ados_protocol::pilot::{self, ControlMode, Owner};
use tokio::sync::{broadcast, mpsc};

use super::out::Effect;
use super::step::PilotInputs;
use super::store::DEFAULT_DIR;
use super::Engine;
use crate::ipc::SetupHandler;
use crate::runtime::{Daemon, PilotHandle};

const TICK_MS: u64 = 20;
/// Overrides the state directory (tests, benches).
pub const DIR_ENV: &str = "ADOS_FC_SETUP_DIR";

struct Inner {
    engine: Mutex<Engine>,
    subscribers: Mutex<Vec<mpsc::Sender<Vec<u8>>>>,
    start: Instant,
}

impl Inner {
    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

pub struct SetupService {
    inner: Arc<Inner>,
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Start the engine task on the daemon's FC link and return the `setup.*`
/// op handler for [`Daemon::run`].
pub fn start(daemon: &Daemon) -> Arc<dyn SetupHandler> {
    let dir = std::env::var(DIR_ENV).unwrap_or_else(|_| DEFAULT_DIR.to_string());
    let inner = Arc::new(Inner {
        engine: Mutex::new(Engine::new(dir, epoch_ms())),
        subscribers: Mutex::new(Vec::new()),
        start: Instant::now(),
    });
    let frames = daemon.fc_link.frames.subscribe();
    let tx = daemon.fc_link.tx.clone();
    let handle = daemon.handle.clone();
    tokio::spawn(run(inner.clone(), frames, tx, handle));
    Arc::new(SetupService { inner })
}

async fn run(
    inner: Arc<Inner>,
    mut frames: broadcast::Receiver<(u8, u8, ados_protocol::mavlink::ardupilotmega::MavMessage)>,
    tx: mpsc::Sender<ados_protocol::mavlink::ardupilotmega::MavMessage>,
    handle: PilotHandle,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut needed: Option<Option<String>> = None;
    loop {
        tokio::select! {
            f = frames.recv() => match f {
                Ok((sys, comp, msg)) => {
                    let now = inner.now();
                    inner.engine.lock().expect("engine lock").on_fc(sys, comp, &msg, now);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!(skipped = n, "fc_setup_frames_lagged"),
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                let now = inner.now();
                let status = handle.status.borrow().clone();
                let inputs = PilotInputs {
                    axes: *handle.tuning_axes.borrow(),
                    axes_ms: Some(now),
                    link_lost: status.link_lost,
                    readiness_blockers: status
                        .readiness
                        .iter()
                        .filter(|r| !r.ok && r.severity == pilot::Severity::Block && r.id != "setup_needed")
                        .map(|r| r.msg.clone())
                        .collect(),
                };
                let owner_tuning = *handle.owner.borrow() == Owner::Tuning;
                let (msgs, effects, out_frames, need) = {
                    let mut e = inner.engine.lock().expect("engine lock");
                    e.set_inputs(inputs);
                    e.set_pilot_tuning(owner_tuning);
                    e.tick(now);
                    let ob = e.outbox();
                    (ob.take_msgs(), ob.take_effects(), e.frames(now), e.setup_needed(now))
                };
                for m in msgs {
                    if tx.try_send(m).is_err() {
                        tracing::warn!("fc_setup_fc_queue_full");
                    }
                }
                for eff in effects {
                    apply(&inner, &handle, eff).await;
                }
                if !out_frames.is_empty() {
                    let encoded: Vec<Vec<u8>> =
                        out_frames.iter().filter_map(|f| pilot::encode_op("setup.stream", f).ok()).collect();
                    let mut subs = inner.subscribers.lock().expect("subscribers lock");
                    subs.retain(|s| !s.is_closed());
                    for s in subs.iter() {
                        for b in &encoded {
                            let _ = s.try_send(b.clone());
                        }
                    }
                }
                if needed.as_ref() != Some(&need) {
                    handle.setup_needed(need.clone()).await;
                    needed = Some(need);
                }
            }
        }
    }
}

async fn apply(inner: &Arc<Inner>, handle: &PilotHandle, eff: Effect) {
    match eff {
        Effect::EnterTuning => {
            let ok = handle.enter_tuning().await;
            let now = inner.now();
            inner.engine.lock().expect("engine lock").tuning_result(ok, now);
            if !ok {
                tracing::warn!("fc_setup_tuning_refused");
            }
        }
        Effect::ExitTuning { .. } => handle.exit_tuning().await,
        Effect::SetControlMode(m) => {
            let mode = match m {
                "fc_manual" => ControlMode::FcManual,
                "elrs" => ControlMode::Elrs,
                _ => ControlMode::Ados,
            };
            handle.set_control_mode(mode).await;
        }
        Effect::SetRcAdosPosition(n) => handle.set_rc_ados_position(n).await,
    }
}

impl SetupHandler for SetupService {
    fn handle(
        &self,
        op: String,
        body: rmpv::Value,
        sink: mpsc::Sender<Vec<u8>>,
    ) -> Pin<Box<dyn Future<Output = rmpv::Value> + Send>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            if op == "setup.subscribe" {
                inner.subscribers.lock().expect("subscribers lock").push(sink);
                return rmpv::Value::Map(vec![(rmpv::Value::from("ok"), rmpv::Value::from(true))]);
            }
            let json: serde_json::Value = rmpv::ext::from_value(body).unwrap_or(serde_json::Value::Null);
            let now = inner.now();
            let reply = inner.engine.lock().expect("engine lock").request(&op, &json, now);
            rmpv::ext::to_value(&reply).unwrap_or(rmpv::Value::Nil)
        })
    }
}
