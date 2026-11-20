//! One run: operators queue commands for the active window, the node lives
//! through its kills and the backend through its outages; then a fault-free
//! drain lets every row settle before the end checks.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tokio::time::sleep_until;

use crate::check;
use crate::node;
use crate::world::{Enqueue, Faults, Shared, Trace, World};

/// Fault-free time after the active window: past the longest delivery window
/// of a row queued in it (the queue's 10 min default), a lease (60 s), the
/// longest handler (90 s) and a poll, so every row is acked or failed by the
/// queue before the end checks.
pub const DRAIN_MS: u64 = 14 * 60 * 1000;

/// What the operators do.
pub struct Plan {
    pub enqueues: Vec<Enqueue>,
    /// Commands are queued, and faults land, inside this window.
    pub active_ms: u64,
}

/// What a run left behind.
pub struct Run {
    pub trace: Trace,
    pub violations: Vec<String>,
    pub sim_s: f64,
    pub commands: usize,
    /// Commands whose local effect happened.
    pub ran: usize,
    pub kills: u32,
    /// Rows the node acked `failed` without running them (refused, expired,
    /// interrupted).
    pub refused: usize,
    /// Acks the backend answered 404 (their row was gone).
    pub not_found: u32,
}

/// Run `plan` under `faults` on `seed`.
pub fn simulate(seed: u64, faults: Faults, plan: Plan) -> Run {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a paused runtime");
    let dir = tempfile::tempdir().expect("a record dir");
    let record = dir.path().join("cloud-executed-commands.json");
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let w: Shared = Rc::new(RefCell::new(World::new(seed, faults)));
        let end_ms = plan.active_ms + DRAIN_MS;
        let operators = tokio::task::spawn_local(operate(w.clone(), plan.enqueues));
        let monitor = tokio::task::spawn_local(check::monitor(w.clone()));
        node::life(w.clone(), record, end_ms).await;
        operators.abort();
        monitor.abort();
        check::finish(&w);
        let mut g = w.borrow_mut();
        let sim_s = g.now_ms() as f64 / 1000.0;
        let ran = g.effects.len();
        let refused = g
            .verdicts
            .iter()
            .filter(|(id, v)| v.status == "failed" && !g.started.contains(*id))
            .count();
        let commands = g.queue.rows.len() + g.queue.pruned.len();
        let (kills, not_found) = (g.kills, g.not_found);
        g.log(format!(
            "end: {commands} commands, {ran} ran, {refused} refused unrun, {kills} kills"
        ));
        Run {
            trace: std::mem::take(&mut g.trace),
            violations: std::mem::take(&mut g.violations),
            sim_s,
            commands,
            ran,
            kills,
            refused,
            not_found,
        }
    })
}

/// Queue each command at its time.
async fn operate(w: Shared, mut enqueues: Vec<Enqueue>) {
    enqueues.sort_by_key(|e| e.at_ms);
    let t0 = w.borrow().t0;
    for e in enqueues {
        sleep_until(t0 + Duration::from_millis(e.at_ms)).await;
        let mut g = w.borrow_mut();
        let now = g.server_ms();
        let id = g.queue.enqueue(now, e.command, e.args, e.ttl_ms);
        let ttl = e.ttl_ms.map_or("-".to_string(), |t| t.to_string());
        g.log(format!("operator: queue {id} {} ttl {ttl}", e.command));
    }
}
