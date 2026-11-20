//! The node: the real command-poll pass ([`command_poll::poll_once`]) and the
//! real executed-command record on a temp dir, over a simulated network to the
//! simulated queue, with a simulated local API behind the runner seam. The
//! heartbeat loop beside it posts on the same network and folds each outcome
//! into the real [`CloudLinkTracker`]. A kill aborts the process's tasks at
//! whatever await they are parked on, like SIGKILL; the restart loads the
//! record from disk again.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use ados_cloud::dispatch::executed_commands::ExecutedCommands;
use ados_cloud::dispatch::CommandResult;
use ados_cloud::loops::command_poll::{self, AckAttempt, CommandLink, CommandRunner};
use ados_cloud::loops::heartbeat::{CloudLinkTracker, HeartbeatOutcome, HEARTBEAT_INTERVAL};
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, Instant};

use crate::queue::DEVICE;
use crate::world::{Shared, Verdict, REQUEST_TIMEOUT};

enum Req {
    Claim { held: Vec<String> },
    Ack(Value),
    Status,
}

enum Answer {
    Claim(Value),
    Ack(u16),
    Status,
}

/// One request over the simulated network: up-link latency, then the backend
/// (away, losing the request, or processing it), then the answer's
/// down-link latency, or a client timeout.
async fn exchange(w: &Shared, req: Req) -> Option<Answer> {
    let (up, down, req_lost, resp_lost) = {
        let mut g = w.borrow_mut();
        let lat = g.faults.latency_ms;
        let (p_req, p_resp) = (g.faults.p_req_lost, g.faults.p_resp_lost);
        (
            g.draw_ms(lat),
            g.draw_ms(lat),
            g.chance(p_req),
            g.chance(p_resp),
        )
    };
    let timeout_rest = REQUEST_TIMEOUT - Duration::from_millis(up);
    sleep(Duration::from_millis(up)).await;
    let outcome = {
        let mut g = w.borrow_mut();
        let now = g.now_ms();
        if let Some(o) = g.outage_at(now) {
            g.log(format!("net: {} at the backend: away", req.name()));
            if o.blackhole {
                Fate::Timeout
            } else {
                Fate::Refused
            }
        } else if req_lost {
            g.log(format!("net: {} lost on the way up", req.name()));
            Fate::Timeout
        } else {
            let a = process(&mut g, req);
            if resp_lost {
                g.log("net: answer lost on the way down");
                Fate::Timeout
            } else {
                Fate::Answer(a)
            }
        }
    };
    match outcome {
        Fate::Answer(a) => {
            sleep(Duration::from_millis(down)).await;
            Some(a)
        }
        Fate::Refused => {
            sleep(Duration::from_millis(down)).await;
            None
        }
        Fate::Timeout => {
            sleep(timeout_rest).await;
            None
        }
    }
}

enum Fate {
    Answer(Answer),
    Refused,
    Timeout,
}

impl Req {
    fn name(&self) -> &'static str {
        match self {
            Req::Claim { .. } => "claim",
            Req::Ack(_) => "ack",
            Req::Status => "status",
        }
    }
}

/// The backend handles one request.
fn process(g: &mut crate::world::World, req: Req) -> Answer {
    let now = g.server_ms();
    match req {
        Req::Claim { held } => {
            // Acks survive an outage and go out once the backend is back: the
            // node holds no undelivered ack when its next claim lands, or the
            // claim would fail that ack's row as never acknowledged.
            for id in held {
                if !g.landed.contains(&id) {
                    g.violate(format!(
                        "a claim landed while the node still held the undelivered ack of {id}"
                    ));
                }
            }
            let (mut rows, failed) = g.queue.claim(now);
            for (id, reason) in failed {
                g.log(format!("queue: {id} failed: {reason}"));
            }
            let (p_dup, p_reorder) = (g.faults.p_dup_row, g.faults.p_reorder);
            let mut i = 0;
            while i < rows.len() {
                if g.chance(p_dup) {
                    let row = rows[i].clone();
                    rows.insert(i + 1, row);
                    i += 1;
                }
                i += 1;
            }
            if rows.len() > 1 && g.chance(p_reorder) {
                for k in (1..rows.len()).rev() {
                    let j = (g.rng.next_u64() % (k as u64 + 1)) as usize;
                    rows.swap(k, j);
                }
            }
            let ids: Vec<String> = rows
                .iter()
                .map(|r| {
                    format!(
                        "{}#{}",
                        r["_id"].as_str().unwrap_or(""),
                        r["attempts"].as_u64().unwrap_or(0)
                    )
                })
                .collect();
            for r in &rows {
                let id = r["_id"].as_str().unwrap_or("").to_string();
                *g.copies.entry(id).or_insert(0) += 1;
            }
            g.log(format!("queue: claim -> [{}]", ids.join(" ")));
            Answer::Claim(json!({ "commands": rows }))
        }
        Req::Ack(body) => {
            let id = body["commandId"].as_str().unwrap_or("").to_string();
            let copies = if g.chance(g.faults.p_dup_ack) { 2 } else { 1 };
            let mut status = 0;
            for _ in 0..copies {
                let now = g.server_ms();
                let (s, set) = g.queue.ack(now, &body);
                status = s;
                g.log(format!(
                    "queue: ack {id} {} -> {s}{}",
                    body["status"].as_str().unwrap_or("?"),
                    if set { "" } else { " (ignored)" }
                ));
                if s == 200 || s == 404 {
                    g.landed.insert(id.clone());
                }
                if s == 404 {
                    g.not_found += 1;
                }
            }
            Answer::Ack(status)
        }
        Req::Status => Answer::Status,
    }
}

/// The queue as the poll pass sees it.
pub struct SimLink {
    pub w: Shared,
}

impl CommandLink for SimLink {
    async fn claim(&self) -> Option<Value> {
        let held = self.w.borrow().held();
        match exchange(&self.w, Req::Claim { held }).await {
            Some(Answer::Claim(body)) => Some(body),
            _ => {
                self.w.borrow_mut().log("node: claim failed");
                None
            }
        }
    }

    async fn ack(&self, ack: &Value) -> AckAttempt {
        {
            let mut g = self.w.borrow_mut();
            let id = ack["commandId"].as_str().unwrap_or("").to_string();
            let verdict = Verdict {
                status: ack["status"].as_str().unwrap_or("").to_string(),
                message: ack["result"]["message"].as_str().unwrap_or("").to_string(),
            };
            if verdict.message.is_empty() || !["completed", "failed"].contains(&&*verdict.status) {
                g.violate(format!("{id}: ack without a status and reason: {ack}"));
            }
            match g.verdicts.get(&id) {
                Some(v) if *v != verdict => {
                    let first = v.clone();
                    g.violate(format!(
                        "{id}: the node changed its verdict from {first:?} to {verdict:?}"
                    ));
                }
                Some(_) => {}
                None => {
                    g.verdicts.insert(id.clone(), verdict.clone());
                }
            }
            // Once the node has heard the queue settle an ack (2xx, or 404
            // for a gone row), it sends it again only to answer another copy
            // of the row (a duplicate, or a redelivery) it has not heard
            // settled yet.
            if g.answered.contains(&id) && g.copies.get(&id).copied().unwrap_or(0) == 0 {
                g.violate(format!(
                    "{id}: the node re-sent an ack the queue had answered, with no new delivery"
                ));
            }
            g.log(format!(
                "node: ack {id} {} \"{}\"",
                verdict.status, verdict.message
            ));
        }
        let attempt = match exchange(&self.w, Req::Ack(ack.clone())).await {
            Some(Answer::Ack(s)) if (200..300).contains(&s) => AckAttempt::Delivered,
            Some(Answer::Ack(s)) => AckAttempt::Refused(s),
            _ => AckAttempt::Unanswered,
        };
        if matches!(attempt, AckAttempt::Delivered | AckAttempt::Refused(404)) {
            let mut g = self.w.borrow_mut();
            let id = ack["commandId"].as_str().unwrap_or("").to_string();
            g.answered.insert(id.clone());
            // The copy this ack answered is spent.
            if let Some(n) = g.copies.get_mut(&id) {
                *n = n.saturating_sub(1);
            }
        }
        attempt
    }
}

/// The local API behind the runner seam: each handler acts (its effect) after
/// a while, then answers; a restart of the cloud relay's own unit kills the
/// node before the answer comes back.
pub struct SimRunner {
    pub w: Shared,
    pub kill: Rc<Notify>,
}

impl CommandRunner for SimRunner {
    async fn run(&self, name: &str, cmd: &Value) -> CommandResult {
        let id = cmd["_id"].as_str().unwrap_or("").to_string();
        let self_restart = name == "restart_service" && cmd["args"]["name"] == "ados-cloud";
        let (effect_ms, reply_ms, fails, gen) = {
            let mut g = self.w.borrow_mut();
            let now = g.server_ms();
            match g.queue.row(&id).map(|r| r.deadline()) {
                Some(deadline) if now >= deadline => {
                    let late = now - deadline;
                    g.violate(format!(
                        "{id} ({name}) started {late} ms after its delivery window closed"
                    ));
                }
                Some(_) => {}
                None => g.violate(format!("{id}: ran a command the queue never held")),
            }
            g.started.insert(id.clone());
            let (effect, reply) = match name {
                "send_command" => ((50, 300), (20, 200)),
                "restart_service" if self_restart => ((100, 500), (2_000, 4_000)),
                "restart_service" => ((100, 500), (200, 1_500)),
                "scan_peripherals" => ((500, 3_000), (10, 100)),
                "wfb_pair_init_remote" | "wfb_pair_apply_remote" | "wfb_pair_unpair" => {
                    ((500, 5_000), (50, 300))
                }
                "plugin.install" => (g.faults.install_ms, (10, 100)),
                n if n.starts_with("plugin.") => ((200, 2_000), (10, 100)),
                _ => ((20, 200), (10, 100)),
            };
            let p_fail = g.faults.p_handler_fails;
            let gen = g.gen;
            let out = (g.draw_ms(effect), g.draw_ms(reply), g.chance(p_fail), gen);
            g.log(format!("node: run {id} {name}"));
            out
        };
        sleep(Duration::from_millis(effect_ms)).await;
        {
            let mut g = self.w.borrow_mut();
            let n = {
                let e = g.effects.entry(id.clone()).or_insert(0);
                *e += 1;
                *e
            };
            g.log(format!("local: {id} {name} acted"));
            if n > 1 {
                g.violate(format!("{id} ({name}) ran {n} times"));
            }
            if self_restart {
                let delay = g.draw_ms((50, 300));
                let (w, kill) = (self.w.clone(), self.kill.clone());
                tokio::task::spawn_local(async move {
                    sleep(Duration::from_millis(delay)).await;
                    let mut g = w.borrow_mut();
                    if g.alive && g.gen == gen {
                        g.kill_gen = Some(gen);
                        drop(g);
                        kill.notify_one();
                    }
                });
            }
        }
        sleep(Duration::from_millis(reply_ms)).await;
        if fails {
            CommandResult::failed("local api error: HTTP 500")
        } else {
            CommandResult::completed(format!("{name} ok"))
        }
    }
}

/// One node process: the command-poll loop as the daemon runs it.
async fn poll_loop(w: Shared, kill: Rc<Notify>, record: PathBuf) {
    let mut executed = ExecutedCommands::load(&record);
    let link = SimLink { w: w.clone() };
    let runner = SimRunner { w, kill };
    let mut tick = tokio::time::interval(command_poll::POLL_INTERVAL);
    loop {
        tick.tick().await;
        command_poll::poll_once(&link, &runner, DEVICE, &mut executed).await;
    }
}

/// The heartbeat loop beside it, on the same network and backend.
async fn heartbeat_loop(w: Shared) {
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tick.tick().await;
        {
            let mut g = w.borrow_mut();
            let now = g.now_ms();
            g.hb_sent_ms = Some(now);
        }
        let outcome = match exchange(&w, Req::Status).await {
            Some(_) => HeartbeatOutcome::Answered(200),
            None => HeartbeatOutcome::Unanswered("timed out".to_string()),
        };
        let mut g = w.borrow_mut();
        let now = g.server_ms();
        g.link.record(&outcome, now as i64);
    }
}

/// Boot one process generation.
fn boot(w: &Shared, kill: &Rc<Notify>, record: &Path) -> [JoinHandle<()>; 2] {
    {
        let mut g = w.borrow_mut();
        g.gen += 1;
        g.alive = true;
        g.up_ms = g.now_ms();
        g.hb_sent_ms = None;
        g.link = CloudLinkTracker::default();
        let gen = g.gen;
        g.log(format!("node: process {gen} up"));
    }
    [
        tokio::task::spawn_local(poll_loop(w.clone(), kill.clone(), record.to_path_buf())),
        tokio::task::spawn_local(heartbeat_loop(w.clone())),
    ]
}

/// Run the node until `end_ms`: kill it at each planned kill and whenever its
/// own command restarts it, and bring it back after the restart delay.
pub async fn life(w: Shared, record: PathBuf, end_ms: u64) {
    let kill = Rc::new(Notify::new());
    let t0 = w.borrow().t0;
    let end = t0 + Duration::from_millis(end_ms);
    let away = w.borrow().faults.away;
    let mut planned: Vec<u64> = w.borrow().faults.kills.clone();
    planned.extend(away.map(|a| a.at_ms));
    planned.sort_unstable();
    let mut planned = planned.into_iter().peekable();
    let mut tasks = boot(&w, &kill, &record);
    loop {
        let next = planned
            .peek()
            .map(|&ms| t0 + Duration::from_millis(ms))
            .unwrap_or(end);
        tokio::select! {
            _ = sleep_until(next.min(end)) => {
                if Instant::now() >= end {
                    break;
                }
                let at = planned.next();
                let mut g = w.borrow_mut();
                g.log("node: killed (planned)");
                if let Some(a) = away.filter(|a| Some(a.at_ms) == at) {
                    go_away(&mut g, a);
                }
            }
            _ = kill.notified() => {
                let mut g = w.borrow_mut();
                if g.kill_gen != Some(g.gen) {
                    continue;
                }
                g.log("node: killed by its own restart");
            }
        }
        for t in &tasks {
            t.abort();
        }
        let delay = {
            let mut g = w.borrow_mut();
            g.alive = false;
            g.kills += 1;
            let r = g.faults.restart_ms;
            g.draw_ms(r)
        };
        sleep(Duration::from_millis(delay)).await;
        tasks = boot(&w, &kill, &record);
    }
    for t in &tasks {
        t.abort();
    }
    w.borrow_mut().alive = false;
}

/// The node stays away while the backend's clock runs `a.server_ms` on: the
/// stuck sweep runs an hour or two in, the retention sweep at the end.
fn go_away(g: &mut crate::world::World, a: crate::world::Away) {
    let before = g.server_ms();
    let swept = g.queue.sweep_stuck(before + 2 * 60 * 60 * 1000);
    g.server_offset_ms += a.server_ms;
    let now = g.server_ms();
    let pruned = g.queue.prune(now);
    g.log(format!(
        "node: away {} h; queue swept [{}], pruned {} rows",
        a.server_ms / 3_600_000,
        swept.join(" "),
        pruned.len()
    ));
}
