//! The world: the box, the phones, the drone's view of the air and an
//! eavesdropper on the LAN, stepped in 20 ms ticks of a paused tokio clock on
//! one thread. The box's tasks run between the ticks, when the clock moves;
//! everything random comes from the run's one generator.

mod links;

use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ados_control::routes::ground_station_uplink::phones::ApprovedPhone;
use ados_control::routes::ground_station_uplink::sign;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::boxside::{wall_ms, yield_signed, BoxSide, Out};
use crate::check::{Ledger, Trace, View};
use crate::net::{Event, Queue, Signed, Via};
use crate::phone::{Phase, Phone};
use crate::rng::Rng;

/// The world's step.
pub const TICK_MS: u64 = 20;
/// The calm tail after the script: no faults, no actions.
pub const TAIL_MS: u64 = 75_000;
/// Every ground's presence beacon period.
const BEACON_MS: u64 = 10_000;
/// Where the eavesdropper replays from.
const ADVERSARY_IP: [u8; 4] = [10, 0, 0, 99];

/// The faults a scenario injects until its calm tail.
#[derive(Debug, Clone, Copy, Default)]
pub struct Faults {
    pub lan_loss: f64,
    pub lan_dup: f64,
    pub radio_loss: f64,
    /// Chance per tick that the eavesdropper replays a captured message.
    pub replay: f64,
}

/// One scripted move.
#[derive(Debug, Clone, Copy)]
pub enum Action {
    Take(usize),
    GiveBack(usize),
    /// The box's operator takes the uplink back.
    TakeBack,
    Crash(usize),
    /// The box stops hearing phone `.0` for `.1` ms; the phone hears it.
    Fade(usize, u64),
    RestartBox,
    RestartControl,
}

/// A scenario's setup.
pub struct Plan {
    pub phones: Vec<String>,
    pub faults: Faults,
    pub script: Vec<(u64, Action)>,
}

/// A finished run.
pub struct Run {
    pub trace: Trace,
    pub ledger: Ledger,
    pub sim_ms: u64,
}

pub struct World {
    name: &'static str,
    seed: u64,
    rng: Rng,
    t0: Instant,
    _dir: tempfile::TempDir,
    bx: BoxSide,
    boot: u32,
    phones: Vec<Phone>,
    q: Queue,
    out: mpsc::UnboundedReceiver<Out>,
    pair_key: [u8; 32],
    faults: Faults,
    calm: bool,
    captured: Vec<(Option<usize>, Signed)>,
    /// Calls whose answer the network lost: the box waits them out.
    limbo: Vec<oneshot::Sender<bool>>,
    next_msg: u64,
    box_beacon: u64,
    fade_until: Vec<u64>,
    ledger: Ledger,
    trace: Trace,
}

/// Run scenario `name` on `seed` to the end of its calm tail; panics with a
/// reproducer on the first violation.
pub fn run(name: &'static str, seed: u64, plan: fn(&mut Rng) -> Plan) -> Run {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a current-thread runtime");
    rt.block_on(async move {
        let mut rng = Rng::new(seed);
        let plan = plan(&mut rng);
        let mut w = World::new(name, seed, rng, &plan);
        w.fly(plan.script).await;
        Run {
            sim_ms: w.now(),
            trace: w.trace,
            ledger: w.ledger,
        }
    })
}

impl World {
    fn new(name: &'static str, seed: u64, mut rng: Rng, plan: &Plan) -> Self {
        let dir = tempfile::tempdir().expect("a scratch dir");
        let t0 = Instant::now();
        let phones: Vec<Phone> = plan
            .phones
            .iter()
            .enumerate()
            .map(|(i, id)| Phone::new(i, id.clone(), &mut rng))
            .collect();
        let approved = Arc::new(
            phones
                .iter()
                .map(|p| ApprovedPhone {
                    phone_pub_b64: p.pub_b64.clone(),
                    fingerprint: format!("FP-{}", p.i),
                    label: format!("Phone {}", p.i),
                    uplink_key_b64: base64_key(&p.key),
                    approved_at_ms: 0,
                })
                .collect::<Vec<_>>(),
        );
        let (tx, out) = mpsc::unbounded_channel();
        let bx = BoxSide::boot(dir.path(), approved, tx, t0, &mut rng);
        let n = phones.len();
        Self {
            name,
            seed,
            pair_key: rng.key(),
            box_beacon: rng.ms(0, BEACON_MS),
            rng,
            t0,
            _dir: dir,
            bx,
            boot: 0,
            phones,
            q: Queue::default(),
            out,
            faults: plan.faults,
            calm: false,
            captured: Vec::new(),
            limbo: Vec::new(),
            next_msg: 0,
            fade_until: vec![0; n],
            ledger: Ledger::new(n),
            trace: Trace::default(),
        }
    }

    fn now(&self) -> u64 {
        (Instant::now() - self.t0).as_millis() as u64
    }

    fn wall(&self) -> i64 {
        wall_ms(self.t0)
    }

    fn note(&mut self, text: &str) {
        let now = self.now();
        self.trace.line(now, text);
    }

    fn fail(&self, why: &str) -> ! {
        panic!(
            "dst uplink {name} seed {seed} at {t} ms: {why}\nreproduce: cargo test -p ados-groundlink --test dst -- --exact dst_{name} (seed {seed})\ntrace tail:\n{tail}",
            name = self.name,
            seed = self.seed,
            t = self.now(),
            tail = self.trace.tail(40)
        )
    }

    fn ok(&self, r: Result<(), String>) {
        if let Err(why) = r {
            self.fail(&why);
        }
    }

    async fn fly(&mut self, script: Vec<(u64, Action)>) {
        let end = script.last().map_or(0, |(t, _)| *t) + 5_000;
        let mut script = script.into_iter().peekable();
        loop {
            let now = self.now();
            if now >= end && !self.calm {
                self.calm = true;
                self.note("calm: faults off");
            }
            while let Some((_, a)) = script.next_if(|(t, _)| *t <= now) {
                self.act(a).await;
            }
            if !self.calm && !self.captured.is_empty() && self.rng.chance(self.faults.replay) {
                self.replay();
            }
            self.drain_box();
            while let Some(ev) = self.q.pop_due(now) {
                self.deliver(ev);
            }
            self.step_phones();
            self.beacons();
            self.check();
            if now.is_multiple_of(1_000) {
                self.snapshot();
            }
            if now >= end + TAIL_MS {
                break;
            }
            tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
        }
        let ids: Vec<String> = self.phones.iter().map(|p| p.device_id.clone()).collect();
        let tx = self.phone_tx();
        let r = self.ledger.end(&self.view(&tx), &ids);
        self.ok(r);
        let s = format!("end {:?}", self.ledger.stats);
        self.note(&s);
    }

    fn phone_tx(&self) -> Vec<bool> {
        self.phones.iter().map(Phone::tx_up).collect()
    }

    fn view<'a>(&self, phone_tx: &'a [bool]) -> View<'a> {
        View {
            now: self.now(),
            box_air: self.bx.air.load(Ordering::SeqCst),
            wants_transmit: self.bx.u.wants_transmit(),
            standby: self.bx.standby_holder(),
            phone_tx,
        }
    }

    fn check(&mut self) {
        let tx = self.phone_tx();
        let v = self.view(&tx);
        let r = self.ledger.tick(&v, TICK_MS);
        self.ok(r);
    }

    fn snapshot(&mut self) {
        let phones: Vec<String> = self
            .phones
            .iter()
            .map(|p| format!("{:?}{}", p.phase, if p.tx_up() { "*" } else { "" }))
            .collect();
        let s = format!(
            "box {}{} watch={} | {}",
            self.bx.u.state().as_str(),
            if self.bx.air.load(Ordering::SeqCst) {
                "*"
            } else {
                ""
            },
            self.bx.listening.load(Ordering::SeqCst),
            phones.join(" | ")
        );
        self.note(&s);
    }

    async fn act(&mut self, a: Action) {
        self.note(&format!("act {a:?}"));
        let now = self.now();
        match a {
            Action::Take(i) => {
                self.next_msg += 1;
                let (msg, wall) = (self.next_msg, self.wall());
                match self.phones[i].take(msg, now, wall) {
                    Some(req) => {
                        self.ledger.take_sent(i);
                        self.lan_request(i, req);
                    }
                    None => self.note(&format!("phone{i} take refused on the phone")),
                }
            }
            Action::GiveBack(i) => self.phones[i].give_back(now, &mut self.rng),
            Action::TakeBack => self.bx.take_back(),
            Action::Crash(i) => self.phones[i].crash(),
            Action::Fade(i, ms) => self.fade_until[i] = now + ms,
            Action::RestartBox => {
                if let Some(i) = self
                    .bx
                    .standby_holder()
                    .and_then(|(id, _)| self.phone_of(&id))
                {
                    self.ledger.revoke(i, now);
                }
                self.bx.restart(&mut self.rng).await;
                self.boot += 1;
                self.ledger.box_restarted();
            }
            Action::RestartControl => self.bx.restart_control(),
        }
    }

    fn phone_of(&self, device_id: &str) -> Option<usize> {
        self.phones.iter().position(|p| p.device_id == device_id)
    }

    fn drain_box(&mut self) {
        while let Ok(o) = self.out.try_recv() {
            let now = self.now();
            match o {
                Out::Reply {
                    phone,
                    msg,
                    via,
                    reply,
                } => {
                    // The reply's `since_ms` is the service's own wall clock:
                    // left out, so a seed's trace is its own.
                    self.note(&format!(
                        "box reply {msg} {via:?}: ok={} state={} error={}",
                        reply["ok"], reply["state"], reply["error"]
                    ));
                    if let (Some(i), Via::Original, false) =
                        (phone, via, self.lost(self.faults.lan_loss))
                    {
                        let at = self.lan_delay();
                        self.q.push(
                            at,
                            Event::Reply {
                                phone: i,
                                msg,
                                reply,
                            },
                        );
                    }
                }
                Out::Admitted { msg, via, ts, take } => {
                    self.note(&format!("route admitted {msg} {via:?}"));
                    if take.is_some() {
                        self.ledger.stats.takes += 1;
                    } else {
                        self.ledger.stats.give_backs += 1;
                    }
                    let r = self.ledger.admitted(msg, via, ts, take, self.wall());
                    self.ok(r);
                }
                Out::Refused { msg, via, why } => {
                    self.note(&format!("route refused {msg} {via:?}: {why}"));
                    self.ledger.refused(msg, via);
                }
                Out::Yield {
                    url,
                    ts,
                    body,
                    mac,
                    answer,
                } => {
                    self.next_msg += 1;
                    let req = yield_signed(self.next_msg, ts, body, mac);
                    self.note(&format!("box take-back call {} to {url}", req.msg));
                    let ip = sign::parse_http_url(&url).map(|(a, _)| a.ip());
                    match self.phones.iter().position(|p| Some(p.ip) == ip) {
                        Some(i) => self.lan_yield(i, req, answer),
                        None => self.limbo.push(answer),
                    }
                }
                Out::Revoke { holder, why } => {
                    self.note(&format!("box resumes without {holder}: {why}"));
                    if let Some(i) = self.phone_of(&holder) {
                        self.ledger.revoke(i, now);
                    }
                }
                Out::FellBack { holder } => {
                    self.note(&format!("box fell back from {holder}"));
                    let r = self.ledger.fell_back(&holder, now, TICK_MS);
                    self.ok(r);
                }
                Out::TookBack { confirmed, changed } => {
                    self.note(&format!(
                        "box took back, confirmed={confirmed} changed={changed}"
                    ));
                    let s = &mut self.ledger.stats;
                    if changed {
                        s.take_backs_changed += 1;
                    } else if confirmed {
                        s.take_backs_confirmed += 1;
                    } else {
                        s.take_backs_unconfirmed += 1;
                    }
                }
            }
        }
    }

    fn deliver(&mut self, ev: Event) {
        let (now, wall) = (self.now(), self.wall());
        match ev {
            Event::AtBox { from, via, req } => {
                let peer = from.map_or(IpAddr::from(ADVERSARY_IP), |i| self.phones[i].ip);
                self.bx.request(from, peer, via, req);
            }
            Event::Reply { phone, msg, reply } => {
                self.phones[phone].on_reply(msg, &reply, now, &mut self.rng);
            }
            Event::AtPhone {
                phone,
                via,
                req,
                answer,
                boot,
            } => {
                if self.phones[phone].phase == Phase::Dead {
                    self.limbo.extend(answer);
                    return;
                }
                let (verdict, acts) =
                    self.phones[phone].on_yield(&req, answer, now, wall, &mut self.rng);
                self.note(&format!(
                    "phone{phone} take-back call {} {via:?}: {verdict:?}",
                    req.msg
                ));
                let r = self.ledger.yield_verdict(phone, via, &req, verdict);
                self.ok(r);
                self.phone_acts(phone, acts, boot);
            }
            Event::YieldAnswer { ok, answer, boot } => {
                if answer.send(ok).is_err() && ok && boot == self.boot {
                    self.fail("the phone confirmed a take-back after the box had stopped waiting and transmitted unconfirmed");
                }
            }
            Event::Beacon { from, to, frame } => self.hear(from, to, &frame),
        }
    }
}

fn base64_key(key: &[u8; 32]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(key)
}
