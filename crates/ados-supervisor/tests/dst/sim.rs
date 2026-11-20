//! One run: a seeded world, both rigs' supervisor processes running the real
//! [`BindOrchestrator`] on a paused clock in one thread, the operator's bind
//! calls, and the fault plan's kills, cancels and second binds. Returns the
//! trace, the outcome and every invariant break.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::logd::emitter::EventEmitter;
use ados_supervisor::bind::keys::{apply_pair_fields, FleetIdentity};
use ados_supervisor::bind::orchestrator::{BindOrchestrator, BindStartError};
use ados_supervisor::process_manager::ProcessManager;
use tokio::task::JoinHandle;

use crate::check;
use crate::host::boot_handles;
use crate::world::{key_blob, lock, Faults, File, Shared, Trace, World, DRONE, GS};

/// How long a killed supervisor stays down (`RestartSec=3`).
const RESTART_SEC: Duration = Duration::from_secs(3);
/// Quiet time after both operators are done, for anything still in flight.
const SETTLE: Duration = Duration::from_secs(120);

/// What happens in one run besides the faults on the air and the units.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// The ground station takes part (false: the drone binds alone).
    pub gs_present: bool,
    /// When each rig's operator first starts a bind, seconds.
    pub first_bind_s: [(f32, f32); 2],
    /// Bind calls each operator makes before giving up.
    pub attempts: [u32; 2],
    /// Kill this rig's supervisor once, in this window (seconds from start).
    pub kill: Option<(usize, (f32, f32))>,
    /// Cancel this rig's bind once, in this window.
    pub cancel: Option<(usize, (f32, f32))>,
    /// Start a second bind on this rig once, in this window.
    pub second_bind: Option<(usize, (f32, f32))>,
}

/// A finished run.
pub struct Run {
    pub trace: Trace,
    pub violations: Vec<String>,
    pub outcome: check::Outcome,
    pub sim_s: f64,
}

/// An event emitter whose shipper never runs: events go nowhere, no socket is
/// opened.
fn quiet_emitter() -> EventEmitter {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    rt.block_on(async { EventEmitter::with_socket("dst", "/nonexistent/dst.sock") })
}

/// Run `plan` under `faults` on `seed`.
pub fn simulate(seed: u64, faults: Faults, plan: Plan) -> Run {
    let events = quiet_emitter();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a paused runtime");
    rt.block_on(async move {
        let w: Shared = Arc::new(Mutex::new(World::new(seed, faults)));
        let fleet = setup(&w);
        let before = check::Snapshot::take(&w);
        let monitor = tokio::spawn(check::monitor(w.clone()));
        let mut lives: Vec<JoinHandle<()>> = Vec::new();
        for rig in [DRONE, GS] {
            if rig == GS && !plan.gs_present {
                continue;
            }
            let f = fleet[rig];
            lives.push(tokio::spawn(life(
                w.clone(),
                rig,
                f,
                plan.clone(),
                events.clone(),
            )));
        }
        for l in lives {
            l.await.expect("a rig's life ends");
        }
        lock(&w).log("operators done");
        tokio::time::sleep(SETTLE).await;
        monitor.abort();
        let outcome = check::finish(&w, &before, fleet[DRONE], plan.gs_present);
        let mut g = lock(&w);
        let sim_s = g.now_ms() as f64 / 1000.0;
        g.log(format!("outcome {outcome:?}"));
        Run {
            trace: std::mem::take(&mut g.trace),
            violations: std::mem::take(&mut g.violations),
            outcome,
            sim_s,
        }
    })
}

/// A freshly flashed drone (no key, auto-pair armed) and a ground station
/// still holding the key of an earlier pairing. Returns the fleet addressing
/// each rig's operator passes: the ground station's registry issues the drone
/// a slot, the ground station itself takes slot 0.
fn setup(w: &Shared) -> [FleetIdentity; 2] {
    let mut g = lock(w);
    let fleet_id = 1 + (g.rng.next_u64() % 200) as u16;
    let slot = 1 + (g.rng.next_u64() % 7) as u8;
    let mtime = g.wall_now();
    for r in &mut g.rigs {
        apply_pair_fields(&mut r.config, r.role, None, None, Some(true), None);
    }
    let gs = &mut g.rigs[GS];
    gs.files.insert(
        gs.role.key_path().to_string(),
        File {
            data: key_blob(0, 1),
            mtime,
        },
    );
    g.log(format!("fleet {fleet_id}, drone slot {slot}"));
    [
        FleetIdentity {
            fleet_id,
            fleet_slot: slot,
        },
        FleetIdentity {
            fleet_id,
            fleet_slot: 0,
        },
    ]
}

/// Start a supervisor process on `rig` as the daemon does: its orchestrator
/// over this generation's host, the recovery of a bind the previous process
/// was killed in, then the boot pass that starts the node's radio unit
/// (`Supervisor::start`).
async fn boot(w: &Shared, rig: usize, events: &EventEmitter) -> Arc<BindOrchestrator> {
    let (host, pm) = boot_handles(w, rig);
    let orch = Arc::new(BindOrchestrator::with_host(
        host,
        pm.clone(),
        events.clone(),
    ));
    orch.recover_interrupted().await;
    let unit = lock(w).rigs[rig].role.normal_unit();
    pm.start(unit).await;
    let mut g = lock(w);
    g.rigs[rig].settled = true;
    let name = g.rigs[rig].name;
    g.log(format!("{name} supervisor up"));
    orch
}

/// Kill `rig`'s supervisor: every call its process makes from now on blocks
/// forever, and its socat children die with its control group.
fn kill(w: &Shared, rig: usize) {
    let mut g = lock(w);
    let r = &mut g.rigs[rig];
    r.epoch += 1;
    r.busy = false;
    r.settled = false;
    // The killed attempt never reports; a rig judged after a kill reads its
    // pair state from its config, as the pairing surfaces do.
    r.last_state = None;
    let name = r.name;
    g.log(format!("{name} supervisor killed"));
}

fn at(w: &Shared, window: (f32, f32)) -> tokio::time::Instant {
    let mut g = lock(w);
    g.t0 + g.draw(window)
}

/// One rig over the run: its supervisor, its operator, and whatever the plan
/// does to it.
async fn life(w: Shared, rig: usize, fleet: FleetIdentity, plan: Plan, events: EventEmitter) {
    let mut orch = boot(&w, rig, &events).await;
    let first = at(&w, plan.first_bind_s[rig]);
    let attempts = plan.attempts[rig];
    if let Some((r, window)) = plan.cancel.filter(|(r, _)| *r == rig) {
        let (o, when) = (orch.clone(), at(&w, window));
        let w = w.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(when).await;
            let name = lock(&w).rigs[r].name;
            lock(&w).log(format!("{name} operator cancels"));
            o.cancel_current();
        });
    }
    if let Some((_, window)) = plan.second_bind.filter(|(r, _)| *r == rig) {
        let when = at(&w, window);
        tokio::spawn(second_bind(w.clone(), rig, orch.clone(), when));
    }
    let mut driver = tokio::spawn(operator(
        w.clone(),
        rig,
        orch.clone(),
        fleet,
        first,
        attempts,
    ));
    let Some((_, window)) = plan.kill.filter(|(r, _)| *r == rig) else {
        driver.await.expect("the operator ends");
        return;
    };
    let when = at(&w, window);
    tokio::select! {
        r = &mut driver => r.expect("the operator ends"),
        _ = tokio::time::sleep_until(when) => {
            kill(&w, rig);
            driver.abort();
            let _ = driver.await;
            tokio::time::sleep(RESTART_SEC).await;
            orch = boot(&w, rig, &events).await;
            // Auto-pair's settle delay before the first attempt after a boot.
            let now = tokio::time::Instant::now() + ados_supervisor::auto_pair::START_DELAY;
            operator(w.clone(), rig, orch, fleet, now, attempts).await;
        }
    }
}

/// The operator (or auto-pair) on one rig: bind until paired, with the
/// auto-pair backoff between attempts, while the rig is still armed.
async fn operator(
    w: Shared,
    rig: usize,
    orch: Arc<BindOrchestrator>,
    fleet: FleetIdentity,
    first: tokio::time::Instant,
    attempts: u32,
) {
    tokio::time::sleep_until(first).await;
    for attempt in 1..=attempts {
        let role = {
            let mut g = lock(&w);
            if !check::armed(&g.rigs[rig]) {
                return;
            }
            g.rigs[rig].busy = true;
            let name = g.rigs[rig].name;
            g.log(format!("{name} bind attempt {attempt}"));
            g.rigs[rig].role
        };
        let res = orch
            .start_local_bind(role, None, "operator", Some(fleet), std::future::pending())
            .await;
        let state = {
            let mut g = lock(&w);
            let r = &mut g.rigs[rig];
            r.busy = false;
            let name = r.name;
            let line = match &res {
                Ok(v) => {
                    let state = v["state"].as_str().unwrap_or("").to_string();
                    r.last_state = Some(state.clone());
                    format!(
                        "{name} bind ends {state}: {}",
                        v["error"].as_str().unwrap_or("")
                    )
                }
                Err(BindStartError::Busy) => format!("{name} bind refused busy"),
            };
            g.log(line);
            g.rigs[rig].last_state.clone()
        };
        if state.as_deref() == Some("paired") {
            return;
        }
        let backoff = Duration::from_secs(3) + lock(&w).draw((0.0, 4.0));
        tokio::time::sleep(backoff).await;
    }
}

/// A second bind on a rig whose first is in flight (a REST retry racing
/// auto-pair): it must be refused while the first is in progress, and must
/// not end the first. With no bind in flight there is nothing to race.
async fn second_bind(
    w: Shared,
    rig: usize,
    orch: Arc<BindOrchestrator>,
    when: tokio::time::Instant,
) {
    tokio::time::sleep_until(when).await;
    let state = orch
        .status()
        .await
        .and_then(|v| v["state"].as_str().map(str::to_string));
    let in_progress = orch.session_active();
    let role = {
        let mut g = lock(&w);
        let name = g.rigs[rig].name;
        g.log(format!(
            "{name} second bind during {state:?} (in progress: {in_progress})"
        ));
        g.rigs[rig].role
    };
    if !in_progress {
        return;
    }
    let res = orch
        .start_local_bind(role, None, "rest", None, std::future::pending())
        .await;
    let mut g = lock(&w);
    let name = g.rigs[rig].name;
    let finished = matches!(state.as_deref(), Some("paired" | "failed" | "aborted"));
    match res {
        Err(BindStartError::Busy) => g.log(format!("{name} second bind refused busy")),
        Ok(v) => {
            let s = v["state"].as_str().unwrap_or("").to_string();
            g.log(format!("{name} second bind ran: {s}"));
            if in_progress && !finished {
                let at_ms = g.now_ms();
                g.violations.push(format!(
                    "{name}: a second bind during {state:?} was not refused (it ran to {s}) at {at_ms} ms"
                ));
            }
        }
    }
}
