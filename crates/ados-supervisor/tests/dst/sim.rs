//! One run: a seeded world, both rigs' supervisor processes running the real
//! [`BindOrchestrator`] on a paused clock in one thread, the operator's bind
//! calls, and the fault plan's kills, cancels and second binds. Returns the
//! trace, the outcome and every invariant break.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::logd::emitter::EventEmitter;
use ados_supervisor::bind::keys::{apply_pair_fields, FleetIdentity};
use ados_supervisor::bind::orchestrator::{BindOrchestrator, BindStartError};
use ados_supervisor::bind::{BindRole, UPSTREAM_DRONE_KEY, UPSTREAM_GS_KEY};
use ados_supervisor::process_manager::ProcessManager;
use tokio::task::JoinHandle;

use crate::check;
use crate::host::boot_handles;
use crate::reconcile;
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
    /// Kill the ground station's supervisor the moment it has sent the drone
    /// its key, before the drone's `OK` arrives: the drone commits, the ground
    /// station keeps the key it had.
    pub kill_ground_awaiting_ok: bool,
    /// After the operators, run both rigs' auto-pair loops until the pair
    /// converges, within [`reconcile::bound`].
    pub reconcile: bool,
    /// The drone's binds carry no fleet assignment, as an auto-pair bind's
    /// never does: it must come out holding the default fleet's provisional
    /// first slot, which is the slot a fresh ground's registry issues it,
    /// whatever fleet the ground is on (drawn per seed), and its radio must
    /// accept it.
    pub unissued_drone: bool,
}

/// A finished run.
pub struct Run {
    pub trace: Trace,
    pub violations: Vec<String>,
    /// The outcome when the operators were done.
    pub before_reconcile: check::Outcome,
    /// The final outcome.
    pub outcome: check::Outcome,
    pub sim_s: f64,
    /// Each supervisor kill: the rig, and whether it was past its gate.
    pub kills: Vec<(usize, bool)>,
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
        let fleet = setup(&w, plan.unissued_drone);
        let before = check::Snapshot::take(&w);
        let monitor = tokio::spawn(check::monitor(w.clone()));
        let mut lives: Vec<(usize, JoinHandle<Arc<BindOrchestrator>>)> = Vec::new();
        for rig in [DRONE, GS] {
            if rig == GS && !plan.gs_present {
                continue;
            }
            let f = fleet[rig];
            let life = life(w.clone(), rig, f, plan.clone(), events.clone());
            lives.push((rig, tokio::spawn(life)));
        }
        let mut orchs = Vec::new();
        for (rig, l) in lives {
            orchs.push((rig, l.await.expect("a rig's life ends")));
        }
        let before_reconcile = check::classify(&lock(&w), &before);
        lock(&w).log(format!("operators done: {before_reconcile:?}"));
        // A split that a kill after the killed rig's peer-evidence gate left
        // behind is one the restart was to finish, not a lost last exchange.
        if before_reconcile == check::Outcome::Split {
            let mut g = lock(&w);
            if g.kills.iter().any(|(_, after_check)| *after_check) {
                g.violations
                    .push("split after a kill past the peer-evidence gate".to_string());
            }
        }
        if plan.reconcile {
            reconcile_pair(&w, orchs, before_reconcile).await;
        }
        tokio::time::sleep(SETTLE).await;
        monitor.abort();
        let outcome = check::finish(&w, &before, fleet, plan.gs_present);
        let mut g = lock(&w);
        let sim_s = g.now_ms() as f64 / 1000.0;
        g.log(format!("outcome {outcome:?}"));
        Run {
            trace: std::mem::take(&mut g.trace),
            violations: std::mem::take(&mut g.violations),
            before_reconcile,
            outcome,
            sim_s,
            kills: std::mem::take(&mut g.kills),
        }
    })
}

/// Run both rigs' auto-pair loops for [`reconcile::bound`]. A pair the
/// operators left split must have converged by then; a pair they left bound
/// stays bound with no bind window opened on it (the loops check every tick).
async fn reconcile_pair(
    w: &Shared,
    orchs: Vec<(usize, Arc<BindOrchestrator>)>,
    before: check::Outcome,
) {
    let records = tempfile::tempdir().expect("a pair-proof record dir");
    let deadline = tokio::time::Instant::now() + reconcile::bound();
    let loops: Vec<_> = orchs
        .into_iter()
        .map(|(rig, orch)| {
            let record = records.path().join(format!("pair-proof-{rig}.json"));
            if rig == GS {
                // The station's original key carried its earlier pairing.
                let fp = reconcile::key_fingerprint(&key_blob(0, 1));
                reconcile::seed_proven(&record, BindRole::Gs, &fp, 1_600_000_000);
            }
            tokio::spawn(reconcile::auto_pair(w.clone(), rig, orch, record, deadline))
        })
        .collect();
    for l in loops {
        l.await.expect("an auto-pair loop ends");
    }
    let mut g = lock(w);
    let converged = reconcile::converged(&g);
    g.log(format!(
        "auto-pair horizon over, pair converged: {converged}"
    ));
    if matches!(before, check::Outcome::Split | check::Outcome::Bound) && !converged {
        let bound = reconcile::bound().as_secs();
        g.violations.push(format!(
            "a pair the operators left {before:?} did not converge within {bound} s"
        ));
    }
}

/// A freshly flashed drone (no key, auto-pair armed) and a ground station
/// holding the key of an earlier pairing that worked (disarmed, fleet slot
/// 0), with that pairing's `wfb_keygen` output still in `/etc`: its own
/// `gs.key` and the `drone.key` it handed that drone, the shared secret its
/// hop, presence and swarm keys derive from. Returns the fleet addressing each
/// rig's operator passes: the ground station's registry issues the drone a
/// slot, the ground station itself takes slot 0. An `unissued` drone is told
/// nothing, so it must end on the radio's default fleet and the registry's
/// first slot, whatever fleet the ground is on (drawn: the default fleet on
/// about half the seeds, another on the rest); a ground on another fleet hears
/// it there through its provisional link.
fn setup(w: &Shared, unissued: bool) -> [FleetIdentity; 2] {
    use ados_radio::config::{DEFAULT_FLEET_ID, FIRST_DRONE_SLOT};
    let mut g = lock(w);
    let (ground_fleet, drone) = if unissued {
        let ground_fleet = if g.rng.next_u64().is_multiple_of(2) {
            DEFAULT_FLEET_ID
        } else {
            2 + (g.rng.next_u64() % 199) as u16
        };
        (ground_fleet, (DEFAULT_FLEET_ID, FIRST_DRONE_SLOT))
    } else {
        let fleet_id = 1 + (g.rng.next_u64() % 200) as u16;
        (fleet_id, (fleet_id, 1 + (g.rng.next_u64() % 7) as u8))
    };
    let ground = FleetIdentity {
        fleet_id: ground_fleet,
        fleet_slot: 0,
    };
    let mtime = g.wall_now();
    let rig = &mut g.rigs[DRONE];
    apply_pair_fields(&mut rig.config, rig.role, None, None, Some(true), None);
    let gs = &mut g.rigs[GS];
    apply_pair_fields(
        &mut gs.config,
        gs.role,
        Some("earlier-drone-01"),
        Some("2026-01-01T00:00:00+00:00"),
        Some(false),
        Some(ground),
    );
    for (path, which) in [
        (gs.role.key_path(), 1),
        (UPSTREAM_GS_KEY, 1),
        (UPSTREAM_DRONE_KEY, 0),
    ] {
        gs.files.insert(
            path.to_string(),
            File {
                data: key_blob(0, which),
                mtime,
            },
        );
    }
    g.log(format!(
        "ground fleet {ground_fleet}, drone fleet {} slot {}",
        drone.0, drone.1
    ));
    [
        FleetIdentity {
            fleet_id: drone.0,
            fleet_slot: drone.1,
        },
        ground,
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
/// forever, and its socat children die with its control group. Records
/// whether its bind had passed the peer-evidence gate (the sentinel says so).
fn kill(w: &Shared, rig: usize) {
    let mut g = lock(w);
    let r = &mut g.rigs[rig];
    let after_check = r.sentinel_active()
        && r.sentinel
            .as_deref()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
            .and_then(|v| v["peer_verified"].as_bool())
            == Some(true);
    r.epoch += 1;
    r.busy = false;
    r.settled = false;
    // The killed attempt never reports; a rig judged after a kill reads its
    // pair state from its config, as the pairing surfaces do.
    r.last_state = None;
    let name = r.name;
    g.kills.push((rig, after_check));
    g.log(format!(
        "{name} supervisor killed (past the peer check: {after_check})"
    ));
}

fn at(w: &Shared, window: (f32, f32)) -> tokio::time::Instant {
    let mut g = lock(w);
    g.t0 + g.draw(window)
}

/// One rig over the run: its supervisor, its operator, and whatever the plan
/// does to it.
async fn life(
    w: Shared,
    rig: usize,
    fleet: FleetIdentity,
    plan: Plan,
    events: EventEmitter,
) -> Arc<BindOrchestrator> {
    let mut orch = boot(&w, rig, &events).await;
    let first = at(&w, plan.first_bind_s[rig]);
    let attempts = plan.attempts[rig];
    let start_key = lock(&w).rigs[rig].key().map(<[u8]>::to_vec);
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
    // An unissued drone's binds carry no assignment.
    let fleet = (!(plan.unissued_drone && rig == DRONE)).then_some(fleet);
    let mut driver = tokio::spawn(operator(
        w.clone(),
        rig,
        orch.clone(),
        fleet,
        first,
        attempts,
        start_key.clone(),
    ));
    let at_ok = plan.kill_ground_awaiting_ok && rig == GS;
    let window = plan.kill.filter(|(r, _)| *r == rig).map(|(_, w)| w);
    if window.is_none() && !at_ok {
        driver.await.expect("the operator ends");
        return orch;
    }
    let when = window.map(|win| at(&w, win));
    let trigger = {
        let w = w.clone();
        async move {
            match when {
                Some(when) => tokio::time::sleep_until(when).await,
                None => {
                    while !lock(&w).bind_sent {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            }
        }
    };
    tokio::select! {
        r = &mut driver => r.expect("the operator ends"),
        _ = trigger => {
            kill(&w, rig);
            driver.abort();
            let _ = driver.await;
            tokio::time::sleep(RESTART_SEC).await;
            orch = boot(&w, rig, &events).await;
            // Auto-pair's settle delay before the first attempt after a boot.
            let now = tokio::time::Instant::now() + ados_supervisor::auto_pair::START_DELAY;
            operator(w.clone(), rig, orch.clone(), fleet, now, attempts, start_key).await;
        }
    }
    orch
}

/// The operator on one rig, pairing it from the ground station's surface:
/// bind until the rig holds a new key, with the auto-pair backoff between
/// attempts. `start_key` is the key the rig held when the operator began.
async fn operator(
    w: Shared,
    rig: usize,
    orch: Arc<BindOrchestrator>,
    fleet: Option<FleetIdentity>,
    first: tokio::time::Instant,
    attempts: u32,
    start_key: Option<Vec<u8>>,
) {
    tokio::time::sleep_until(first).await;
    for attempt in 1..=attempts {
        let role = {
            let mut g = lock(&w);
            if g.rigs[rig].key() != start_key.as_deref() {
                return;
            }
            g.rigs[rig].busy = true;
            g.rigs[rig].last_bind_key = g.rigs[rig].key().map(<[u8]>::to_vec);
            let name = g.rigs[rig].name;
            g.log(format!("{name} bind attempt {attempt}"));
            g.rigs[rig].role
        };
        let res = orch
            .start_local_bind(role, None, "operator", fleet, std::future::pending())
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
