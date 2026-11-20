//! The invariants. Some are checked where they can break (a handler's effect,
//! a handler's start, a claim and an ack reaching the backend: see `node`),
//! the rest every 100 ms of simulated time ([`monitor`]) and once on the
//! drained end state ([`finish`]).
//!
//! - A command runs at most once per id, across restarts and redeliveries.
//! - No command starts once its delivery window has closed.
//! - Every ack carries `completed` or `failed` and a reason, and the node never
//!   changes its verdict on a command.
//! - Acks survive an outage and a restart and go out once the backend is back:
//!   no claim reaches the backend while the node holds an ack that never got
//!   there (that claim would fail the row as never acknowledged).
//! - The heartbeat keeps its cadence whatever the command pass is doing, and
//!   the cloud link never reads healthy through an outage.
//! - At the end every row is terminal with a reason, and every row the node
//!   acked holds the node's verdict.

use std::time::Duration;

use ados_cloud::loops::heartbeat::HEARTBEAT_INTERVAL;

use crate::queue::Status;
use crate::world::{Shared, REQUEST_TIMEOUT};

/// The longest gap between two heartbeats: a heartbeat that times out holds
/// the next tick back until it does.
const HEARTBEAT_GAP: Duration = Duration::from_millis(10_500);
const _: () = assert!(HEARTBEAT_INTERVAL.as_millis() < HEARTBEAT_GAP.as_millis());

/// How long a row queued while the node and the backend are both up may wait
/// for a claim: a few polls behind a slow batch, never an ack's whole hold.
pub const CLAIM_WITHIN_MS: u64 = 90_000;

/// The per-tick checks.
pub async fn monitor(w: Shared) {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        let mut g = w.borrow_mut();
        let now = g.now_ms();
        if g.faults.live_claims {
            claims_live(&mut g, now);
        }
        if !g.alive {
            continue;
        }
        let up_for = now - g.up_ms;
        let gap = HEARTBEAT_GAP.as_millis() as u64;
        match g.hb_sent_ms {
            Some(t) if now - t > gap => {
                g.violate(format!(
                    "no heartbeat for {} ms while the node is up",
                    now - t
                ));
            }
            None if up_for > 1_000 => {
                g.violate("no heartbeat in the first second of a process".to_string());
            }
            _ => {}
        }
        if let Some(o) = g.outage_at(now) {
            let away_for = now - o.start_ms.max(g.up_ms);
            let settle = gap + REQUEST_TIMEOUT.as_millis() as u64 + 1_000;
            if away_for > settle {
                let server = g.server_ms();
                let link = g.link.link(true, true, None, server as i64);
                let newest_ok = (server - (now - o.start_ms) + g.faults.latency_ms.1) as i64;
                if link.last_heartbeat_error.is_none()
                    || link.last_heartbeat_ok_ms.is_some_and(|ok| ok > newest_ok)
                {
                    g.violate(format!(
                        "the cloud link reads healthy {away_for} ms into an outage: {link:?}"
                    ));
                }
            }
        }
    }
}

/// While the node and the backend are both up, no row queued since waits more
/// than [`CLAIM_WITHIN_MS`] for a claim.
fn claims_live(g: &mut crate::world::World, now: u64) {
    if !g.alive || g.outage_at(now).is_some() {
        g.both_up_since = None;
        return;
    }
    let server = g.server_ms();
    let since = *g.both_up_since.get_or_insert(server);
    let late: Vec<String> = g
        .queue
        .rows
        .iter()
        .filter(|r| r.status == Status::Pending && r.created_at >= since)
        .filter(|r| server - r.created_at > CLAIM_WITHIN_MS)
        .map(|r| {
            format!(
                "{} ({}) waited over {CLAIM_WITHIN_MS} ms for a claim",
                r.id, r.command
            )
        })
        .collect();
    for v in late {
        g.violate(v);
    }
}

/// The checks on the drained end state.
pub fn finish(w: &Shared) {
    let mut g = w.borrow_mut();
    let mut breaks = Vec::new();
    for row in g.queue.rows.iter().chain(&g.queue.pruned) {
        let id = &row.id;
        let message = row.message.as_deref().unwrap_or("");
        if !row.status.terminal() || message.is_empty() {
            breaks.push(format!(
                "{id} ({}) ended {:?} with reason {message:?}",
                row.command, row.status
            ));
            continue;
        }
        let status = if row.status == Status::Completed {
            "completed"
        } else {
            "failed"
        };
        // A row the backend closed itself (swept while the node was away, or
        // one whose ack mutation always fails) cannot hold the node's verdict.
        if row.swept || row.poisoned {
            continue;
        }
        if let Some(v) = g.verdicts.get(id) {
            if v.status != status || v.message != message {
                breaks.push(format!(
                    "{id} ({}): the node acked {} \"{}\", the queue holds {status} \"{message}\"",
                    row.command, v.status, v.message
                ));
            }
        } else if row.status == Status::Completed {
            breaks.push(format!("{id}: completed with no ack from the node"));
        }
    }
    for b in breaks {
        g.violate(b);
    }
}
