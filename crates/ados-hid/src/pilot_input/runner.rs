//! The native ground-station controller loop: 50 Hz polls of the primary pad,
//! stick frames and safety actions to the pilot relay, lightbar and rumble from
//! the drone's pilot state, and the runtime sidecar.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use super::bindings::{ControlsDoc, ELRS_EMERGENCY_ACTION};
use super::feedback::{lightbar_rgb, transition_alert, PilotView, RumbleQueue};
use super::gestures::{Context, Gestures};
use super::linux::OpenPad;
use super::pad::{pilot_axes, PadMap};
use super::spec::ds4;
use super::status::{wall_ms, ControlMode, GsInputState, ModeTracker};
use super::wire;

/// What the relay link shares with the pad loop.
#[derive(Default)]
struct Shared {
    view: PilotView,
    modes: ModeTracker,
}

/// Inputs to the loop.
pub struct RunnerConfig {
    /// The pilot relay socket.
    pub pilot_sock: PathBuf,
    /// The runtime sidecar path.
    pub state_path: PathBuf,
    /// The live binding document.
    pub bindings: watch::Receiver<ControlsDoc>,
    /// The primary gamepad id (`usb:<vid>:<pid>:eventN`), `None` when unset.
    pub primary: watch::Receiver<Option<String>>,
}

fn node_for(id: &str) -> PathBuf {
    let base = id.rsplit(':').next().unwrap_or(id);
    PathBuf::from("/dev/input").join(base)
}

/// The relay link: connect, subscribe, fold `state` messages into the shared
/// view, and write whatever the pad loop queues. Reconnects every second.
async fn relay_link(
    sock: PathBuf,
    shared: Arc<Mutex<Shared>>,
    epoch: Instant,
    mut rx: mpsc::Receiver<Value>,
) {
    loop {
        let stream = match UnixStream::connect(&sock).await {
            Ok(s) => s,
            Err(_) => {
                // Drain while down: a stick frame is worthless once late.
                let deadline = tokio::time::sleep(Duration::from_secs(1));
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        _ = &mut deadline => break,
                        m = rx.recv() => if m.is_none() { return },
                    }
                }
                continue;
            }
        };
        let (mut r, mut w) = stream.into_split();
        if wire::write_msg(&mut w, &wire::subscribe()).await.is_err() {
            continue;
        }
        tracing::info!("pilot relay connected");
        shared.lock().unwrap().view.relay_up = true;
        // Frame reads are not cancel-safe, so they run in a task of their own
        // and never race the writes below.
        let reader_shared = shared.clone();
        let mut reader = tokio::spawn(async move {
            loop {
                match wire::read_msg(&mut r).await {
                    Ok(Some(m)) if m["op"] == "state" => {
                        let now = epoch.elapsed().as_millis() as u64;
                        let mut s = reader_shared.lock().unwrap();
                        if let Some(mode) = m["control_mode"].as_str().and_then(ControlMode::parse)
                        {
                            s.modes.report(mode);
                        }
                        s.view.state = Some((m, now));
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => return,
                    Err(e) => {
                        tracing::warn!(error = %e, "pilot relay read failed");
                        return;
                    }
                }
            }
        });
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => if let Err(e) = wire::write_msg(&mut w, &m).await {
                        tracing::warn!(error = %e, "pilot relay write failed");
                        break;
                    },
                    None => {
                        reader.abort();
                        return;
                    }
                },
                _ = &mut reader => break,
            }
        }
        reader.abort();
        tracing::warn!("pilot relay disconnected");
        shared.lock().unwrap().view.relay_up = false;
    }
}

/// Run the loop until the task is dropped.
pub async fn run(mut cfg: RunnerConfig) {
    let epoch = Instant::now();
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (tx, rx) = mpsc::channel::<Value>(64);
    tokio::spawn(relay_link(
        cfg.pilot_sock.clone(),
        shared.clone(),
        epoch,
        rx,
    ));

    let spec = ds4();
    let mut doc = cfg.bindings.borrow().clone();
    let mut gestures = Gestures::new(&doc.native_bindings());
    let mut rumble = RumbleQueue::default();
    let mut pad: Option<(OpenPad, PadMap)> = None;
    let mut session: u32 = 0;
    let mut seq: u32 = 0;
    let mut streaming = false;
    let mut prev_bar = "";
    let mut prev_state: Option<Value> = None;
    let mut last_sidecar: Option<(u64, GsInputState)> = None;

    let period = Duration::from_millis(1000 / u64::from(spec.stream.rate_hz.max(1)));
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let now = epoch.elapsed().as_millis() as u64;

        if cfg.bindings.has_changed().unwrap_or(false) {
            doc = cfg.bindings.borrow_and_update().clone();
            gestures = Gestures::new(&doc.native_bindings());
        }
        let want = cfg.primary.borrow_and_update().clone();
        if pad.as_ref().map(|(p, _)| Some(&p.id)) != Some(want.as_ref()) {
            pad = None;
            if let Some(id) = want.as_deref() {
                match OpenPad::open(id, &node_for(id)) {
                    Ok(p) => {
                        tracing::info!(id, driver = p.driver.as_str(), "native pad open");
                        let map = PadMap::for_driver(p.driver);
                        pad = Some((p, map));
                        session = wire::new_session();
                        seq = 0;
                        gestures.reset();
                    }
                    Err(e) => tracing::debug!(id, error = %e, "native pad open failed"),
                }
            }
        }

        // Poll; a failed poll is a lost pad.
        let state = match &pad {
            Some((p, map)) => match p.poll() {
                Ok(raw) => Some(map.read(&raw, spec.stream.trigger_pressed_threshold)),
                Err(e) => {
                    tracing::warn!(error = %e, "native pad lost");
                    None
                }
            },
            None => None,
        };
        if state.is_none() {
            pad = None;
        }

        let (view, (mode, mode_src)) = {
            let s = shared.lock().unwrap();
            (s.view.clone(), s.modes.effective())
        };
        let live = view.live(now).cloned();

        if let Some(st) = &state {
            let ctx = Context {
                rth_active: view.rth_active(now),
                // Unknown counts as airborne: Triangle-hold lands.
                landed: view.known_landed(now),
                vtol: live.as_ref().is_some_and(|s| s["vehicle_class"] == "vtol"),
            };
            let out = gestures.tick(st, &ctx, now);
            for a in &out.actions {
                tracing::info!(action = %a, "native controller action");
                if a == ELRS_EMERGENCY_ACTION {
                    shared.lock().unwrap().modes.emergency();
                }
                let _ = tx.try_send(wire::action(a));
            }
            for c in out.cues {
                rumble.cue(c, now);
            }
        }
        // The emergency switch may have just changed the mode.
        let (mode, mode_src) = if state.is_some() {
            shared.lock().unwrap().modes.effective()
        } else {
            (mode, mode_src)
        };

        let should_stream = state.is_some() && view.relay_up && mode == ControlMode::Ados;
        if should_stream {
            let st = state.as_ref().unwrap();
            let sm = spec
                .stick_modes
                .get(&doc.stick_mode)
                .unwrap_or(&spec.stick_modes[&spec.default_mode]);
            let axes = pilot_axes(st, sm, doc.deadzone);
            seq = seq.wrapping_add(1);
            let _ = tx.try_send(wire::input(session, seq, now, axes, st.buttons));
        } else if streaming {
            let _ = tx.try_send(wire::release(session));
        }
        streaming = should_stream;

        let bar = view.lightbar_state(now);
        if let Some(a) = transition_alert(prev_state.as_ref(), live.as_ref(), prev_bar, bar) {
            rumble.alert(a, now);
        }
        prev_bar = bar;
        prev_state = live;
        if let Some((p, _)) = pad.as_mut() {
            p.set_lightbar(lightbar_rgb(bar, now));
            for pulse in rumble.due(now) {
                p.rumble(pulse);
            }
        } else {
            rumble.due(now);
        }

        // The sidecar: on change, else once a second.
        let side = GsInputState {
            v: 1,
            ts_ms: 0,
            native_sticks: view.relay_up,
            pilot_relay: view.relay_up,
            streaming,
            control_mode: mode,
            control_mode_source: mode_src,
            // Filled only when written: the battery read is a sysfs read.
            controller: None,
        };
        let changed = last_sidecar.as_ref().is_none_or(|(at, prev)| {
            now.saturating_sub(*at) >= 1000
                || prev.native_sticks != side.native_sticks
                || prev.streaming != side.streaming
                || prev.control_mode != side.control_mode
                || prev.controller.is_some() != pad.is_some()
        });
        if changed {
            let mut body = side;
            body.controller = pad.as_ref().map(|(p, _)| p.controller());
            body.ts_ms = wall_ms();
            if let Err(e) = body.save(&cfg.state_path) {
                tracing::debug!(error = %e, "gs input state write failed");
            }
            last_sidecar = Some((now, body));
        }
    }
}
